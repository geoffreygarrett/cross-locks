/*───────────────────── RAII helper – optional layer ───────────────────
 *
 *   use cross_locks::{GlobalLock, GuardExt};
 *
 *   async fn demo<L: GlobalLock>(lock: &L) {
 *       let _g = lock.lock_guard("db", Duration::from_secs(1)).await?;
 *       // critical-section starts ──────┐
 *       /* … do work … */               |
 *   }                                   | (_g dropped here → unlock)
 *   // ---------------------------------┘
 *──────────────────────────────────────────────────────────────────────*/

use crate::{GlobalLock, LockError};
use async_trait::async_trait;
use futures::channel::oneshot;
use std::time::Duration;
use std::{
    marker::PhantomData,
    sync::Arc,
};

/// A drop-based guard returned by [`GuardExt::lock_guard`].
#[derive(Debug)]
pub struct LockGuard<'a, L: ?Sized + GlobalLock> {
    _rel: Option<oneshot::Sender<()>>,
    _p: PhantomData<&'a L>,
}

impl<'a, L: GlobalLock + ?Sized> Drop for LockGuard<'a, L> {
    fn drop(&mut self) {
        if let Some(tx) = self._rel.take() {
            // Ignore send errors – receiver might be gone already.
            let _ = tx.send(());
        }
    }
}

/// Extension trait adding *RAII style* acquisition on top of `GlobalLock`.
#[async_trait]
pub trait GuardExt: GlobalLock {
    /// Acquires `name` and returns a guard that releases in `Drop`.
    async fn lock_guard<'a>(
        &'a self,
        name: impl Into<String> + Send,
        timeout: Duration,
    ) -> Result<LockGuard<'a, Self>, LockError>
    where
        Self: Sized + Sync + Send,
    {
        let name = name.into();

        // Channel used to (1) signal that the critical-section has started
        // and (2) keep it open until the guard is dropped.
        let (started_tx, started_rx) = oneshot::channel::<Result<(), LockError>>();
        let (release_tx, release_rx) = oneshot::channel::<()>();

        // Hold *another* reference because the async move block below outlives
        // the borrow by `&self`.
        let myself: Arc<Self> = Arc::new(()); // ZST trampoline
        // SAFETY: we never move/own `self`, only cast the pointer back.
        let raw: *const Self = self;

        // Spawn the lock-holding task on the current runtime.
        cfg_if::cfg_if! {
            if #[cfg(all(target_arch = "wasm32", feature = "browser"))] {
                wasm_bindgen_futures::spawn_local(async move {
                    let lock_ref: &Self = unsafe { &*raw };
                    let res = lock_ref
                        .with_lock(&name, timeout, || async {
                            // signal success first
                            let _ = started_tx.send(Ok(()));
                            // then await release
                            let _ = release_rx.await;
                        })
                        .await;
                    // if the acquisition itself failed, propagate
                    if res.is_err() {
                        let _ = started_tx.send(res.map(|_|()));
                    }
                });
            } else {
                tokio::spawn(async move {
                    let lock_ref: &Self = unsafe { &*raw };
                    let res = lock_ref
                        .with_lock(&name, timeout, || async {
                            let _ = started_tx.send(Ok(()));
                            let _ = release_rx.await;
                        })
                        .await;
                    if res.is_err() {
                        let _ = started_tx.send(res.map(|_|()));
                    }
                });
            }
        }

        // Wait until the lock was actually acquired (or failed).
        match started_rx.await.unwrap_or_else(|_| Err(LockError::Timeout(timeout)))? {
            () => Ok(LockGuard {
                _rel: Some(release_tx),
                _p: PhantomData,
            }),
        }
    }
}

impl<T: GlobalLock + ?Sized> GuardExt for T {}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::DefaultLock;

    /*───────────────── RAII / drop-based guard tests ─────────────────*/

    // <-- extension trait we just added
    use std::time::Instant;
    use futures::future::join_all;
    use tokio::time::sleep;

    /// 1️⃣  FIFO-fairness & mutual exclusion with drop-based guards.
    #[tokio::test(flavor = "multi_thread")]
    async fn raii_fifo_is_exclusive() {
        let lock = DefaultLock::default();
        let spans = Arc::new(tokio::sync::Mutex::new(Vec::<(u128, u128)>::new()));

        let mut jobs = Vec::new();
        for _ in 0..5 {
            let l = lock.clone();
            let s = spans.clone();
            jobs.push(tokio::spawn(async move {
                // guard is held for 10 ms
                let _g = l.lock_guard("raii-fifo", Duration::from_secs(1)).await.unwrap();
                let st = Instant::now();
                sleep(Duration::from_millis(10)).await;
                let et = Instant::now();
                s.lock().await.push((st.elapsed().as_micros(), et.elapsed().as_micros()));
                // _g drops here → unlock
            }));
        }
        join_all(jobs).await;
        let v = spans.lock().await.clone();
        for w in v.windows(2) {
            assert!(w[0].1 <= w[1].0, "critical-sections overlapped");
        }
    }

    /// 2️⃣  `Duration::ZERO` implements a true *try-lock* for guards.
    #[tokio::test]
    async fn raii_try_lock_semantics() {
        let lock = DefaultLock::default();

        // occupy the lock for 50 ms in a background task
        let l2 = lock.clone();
        let _hold = tokio::spawn(async move {
            let _g = l2.lock_guard("raii-try", Duration::from_secs(1)).await.unwrap();
            sleep(Duration::from_millis(50)).await;
        });

        sleep(Duration::from_millis(5)).await;           // let it grab the lock

        let err = lock
            .lock_guard("raii-try", Duration::ZERO)      // immediate try-lock
            .await
            .unwrap_err();
        assert_eq!(err, LockError::Timeout(Duration::ZERO));
    }

    /// 3️⃣  Releasing (dropping) the guard really frees the lock.
    #[tokio::test]
    async fn raii_guard_drop_releases() {
        let lock = DefaultLock::default();

        // first acquisition succeeds
        {
            let _g = lock.lock_guard("raii-drop", Duration::from_secs(1)).await.unwrap();
            // guard dropped at the end of this block
        }

        // should succeed immediately again – lock is free
        let res = lock
            .lock_guard("raii-drop", Duration::ZERO)
            .await;
        assert!(res.is_ok(), "second acquisition failed: {res:?}");
    }
}