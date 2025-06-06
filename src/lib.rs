//! # cross-locks
//!
//! A single-source implementation of Supabase/GoTrue-style *global
//! exclusive locks* that works in native, browser-WASM and head-less WASM
//! with **zero external deps** apart from the optional feature crates.
//!
//! ┌────────────── target ───────────────┬─ feature ─┬─ backend used by `DefaultLock` ┐
//! │  Native (Tokio, multi-thread)        │ `native`  │ fair FIFO queue (`Notify`)     │
//! │  Browser WASM ( `navigator.locks`)   │ `browser` │ `Navigator.locks.request`      │
//! │  Head-less WASM (Node / WASI)        │ `wasm`    │ no-op passthrough              │
//! └──────────────────────────────────────┴───────────┴───────────────────────────────┘
//!
//! Guarantees
//! ----------
//! * FIFO-fairness for waiters on the *same* lock name (native backend).
//! * Zero-timeout try-lock mirrors the JS SDK (`Duration::ZERO`).
//! * Task-local re-entrancy helper.
//! * `Arc<T>` automatically implements `GlobalLock`.
//!
//! ```toml
//! # choose **exactly one** backend
//! [dependencies]
//! cross-locks = { version = "*", default-features = false, features = ["native"] }
//! # or: "browser" | "wasm"
//! ```
//!
//! ## Test-driving
//! ```bash
//! cargo test --features native            # exhaustive native suite
//! wasm-pack test --firefox --headless \
//!            --features browser           # browser smoke-test
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]

mod utils; // helpers for the native test-suite

use async_trait::async_trait;
use std::{future::Future, sync::Arc, time::Duration};
use thiserror::Error;

/*──────────────────────── errors ────────────────────────*/

/// Errors returned by [`GlobalLock::with_lock`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LockError {
    /// Timed-out while waiting to acquire the lock.
    #[error("acquiring lock timed-out after {0:?}")]
    Timeout(Duration),

    /// Browser only – the JS *Navigator LockManager* rejected the request.
    #[cfg(all(feature = "browser", target_arch = "wasm32"))]
    #[error("Navigator LockManager rejected: {0}")]
    Js(String),
}

/*────────────────── core abstraction ───────────────────*/

/// Backend-agnostic async mutex.
///
/// *On `wasm32`* the `Send` bounds are relaxed automatically.
/// Backend-agnostic async mutex (native build).
#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
pub trait GlobalLock: Send + Sync + 'static {
    async fn with_lock<R, F, Fut>(
        &self,
        name: &str,
        timeout: Duration,
        op: F,
    ) -> Result<R, LockError>
    where
        F:  FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R:  Send + 'static;

    /// Re-entrant helper – lets the *same* task enter again without
    /// trying to take the lock a second time.
    async fn with_reentrant<'a, R, F, Fut>(
        &'a self,
        name: &'a str,
        f: F,
    ) -> Result<R, LockError>
    where
        F:  FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = R> + Send + 'static,
        R:  Send + 'static,
    {
        use std::cell::Cell;
        use tokio::task_local;

        // one counter per async-task
        task_local! { static DEPTH: Cell<usize>; }

        // already inside → run immediately
        if DEPTH.try_with(|c| c.get()).unwrap_or(0) > 0 {
            return Ok(f().await);
        }

        // first entry → acquire the real lock **once**
        DEPTH
            .scope(Cell::new(1), async {
                self.with_lock(name, Duration::MAX, || async {
                    // bump depth for any nested calls
                    DEPTH.with(|c| c.set(c.get() + 1));
                    let out = f().await;
                    DEPTH.with(|c| c.set(c.get() - 1));
                    out                                // ← **no extra `Ok` here**
                })
                    .await
            })
            .await
    }

}

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
pub trait GlobalLock: Send + Sync + 'static {
    async fn with_lock<R, F, Fut>(
        &self,
        name: &str,
        timeout: Duration,
        op: F,
    ) -> Result<R, LockError>
    where
        F:  FnOnce() -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R:  'static;

    async fn with_reentrant<'a, R, F, Fut>(
        &'a self,
        name: &'a str,
        f: F,
    ) -> Result<R, LockError>
    where
        F:  FnOnce() -> Fut + 'static,
        Fut: Future<Output = R> + 'static,
        R:  'static,
    {
        use std::cell::Cell;
        use tokio::task_local;
        // use futures_util::task_local;
        // // use futures::task_local;

        task_local! { static DEPTH: Cell<usize>; }

        if DEPTH.try_with(|c| c.get()).unwrap_or(0) > 0 {
            return Ok(f().await);
        }

        DEPTH
            .scope(Cell::new(1), async {
                self.with_lock(name, Duration::MAX, || async {
                    DEPTH.with(|c| c.set(c.get() + 1));
                    let out = f().await;
                    DEPTH.with(|c| c.set(c.get() - 1));
                    out
                })
                    .await
            })
            .await
    }

}
/*──── blanket impl so `Arc<T>` **is** a lock ────*/

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
impl<T: GlobalLock> GlobalLock for Arc<T> {
    async fn with_lock<R, F, Fut>(
        &self,
        n: &str,
        t: Duration,
        f: F,
    ) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output=R> + 'static,
        R: 'static,
    {
        (**self).with_lock(n, t, f).await
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl<T: GlobalLock> GlobalLock for Arc<T> {
    async fn with_lock<R, F, Fut>(
        &self,
        n: &str,
        t: Duration,
        f: F,
    ) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output=R> + Send + 'static,
        R: Send + 'static,
    {
        (**self).with_lock(n, t, f).await
    }
}

/*─────────────────── no-op backend ───────────────────*/

/// Passthrough implementation – used in pure WASM/WASI or tests.
#[derive(Clone, Debug, Default)]
pub struct NoopLock;

#[cfg(target_arch = "wasm32")]
#[async_trait(?Send)]
impl GlobalLock for NoopLock {
    async fn with_lock<R, F, Fut>(&self, _: &str, _t: Duration, op: F) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output=R> + 'static,
        R: 'static,
    {
        Ok(op().await)
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl GlobalLock for NoopLock {
    async fn with_lock<R, F, Fut>(&self, _: &str, _t: Duration, op: F) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output=R> + Send + 'static,
        R: Send + 'static,
    {
        Ok(op().await)
    }
}

/*──────────────── native FIFO backend ───────────────*/

#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
mod native_fifo {
    use super::*;
    use once_cell::sync::Lazy;
    use std::collections::{HashMap, VecDeque};
    use tokio::sync::{Mutex, Notify};

    type Queue = VecDeque<Arc<Notify>>;
    static QUEUES: Lazy<Mutex<HashMap<String, Queue>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    /// Tokio **FIFO-fair** global lock.
    #[derive(Clone, Debug, Default)]
    pub struct FifoLock;

    #[async_trait]
    impl GlobalLock for FifoLock {
        async fn with_lock<R, F, Fut>(
            &self,
            name: &str,
            timeout: Duration,
            op: F,
        ) -> Result<R, LockError>
        where
            F: FnOnce() -> Fut + Send + 'static,
            Fut: Future<Output=R> + Send + 'static,
            R: Send + 'static,
        {
            use futures::FutureExt;
            use std::panic::{self, AssertUnwindSafe};
            use tokio::time;

            /* 1️⃣ enqueue our Notify handle */
            let me = Arc::new(Notify::new());
            let pos = {
                let mut map = QUEUES.lock().await;
                map.entry(name.into())
                    .or_default()
                    .push_back(me.clone());
                map[&name.to_string()].len() - 1
            };

            /* 2️⃣ wait (with timeout) until we’re at the head */
            if pos != 0 && time::timeout(timeout, me.notified()).await.is_err() {
                QUEUES
                    .lock()
                    .await
                    .get_mut(name)
                    .map(|q| q.retain(|n| !Arc::ptr_eq(n, &me)));
                return Err(LockError::Timeout(timeout));
            }

            /* 3️⃣ run CS, catching panics */
            let result_or_panic = {
                let fut = AssertUnwindSafe(op()).catch_unwind();
                fut.await
            };

            /* 4️⃣ pop queue entry & wake next waiter */
            if let Some(next) = {
                let mut map = QUEUES.lock().await;
                let q = map.get_mut(name).unwrap();
                q.pop_front();
                q.front().cloned()
            } {
                next.notify_one();
            }

            /* 5️⃣ propagate */
            match result_or_panic {
                Ok(v) => Ok(v),
                Err(p) => panic::resume_unwind(p),
            }
        }
    }

    pub type DefaultNative = FifoLock;
}

/*──────── browser backend stub (unchanged) ────────*/
#[cfg(all(feature = "browser", target_arch = "wasm32"))]
mod browser;
// mod raii;
/*──────── re-exports so downstream picks just one name ─────*/

#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
pub type DefaultLock = native_fifo::DefaultNative;

#[cfg(target_arch = "wasm32")]
cfg_if::cfg_if! {
    if #[cfg(feature = "browser")] {
        pub type DefaultLock = browser::DefaultBrowser;
    } else {
        pub type DefaultLock = NoopLock;
    }
}

/*────────────────────  exhaustive tests  ───────────────────*/

#[cfg(test)]
mod tests {
    /*============================  native  ============================*/
    #[cfg(all(feature = "native", not(target_arch = "wasm32")))]
    mod native {
        use super::super::*;
        use futures::{future::join_all, FutureExt};
        use std::{sync::Arc, time::{Duration, Instant}};
        use tokio::{task, time::sleep};

        /// FIFO fairness
        #[tokio::test(flavor = "multi_thread")]
        async fn fifo_is_exclusive() {
            let lock = DefaultLock::default();
            let spans = Arc::new(tokio::sync::Mutex::new(Vec::<(u128, u128)>::new()));

            let mut jobs = Vec::new();
            for _ in 0..5 {
                let l = lock.clone();
                let s = spans.clone();
                jobs.push(task::spawn(async move {
                    l.with_lock("cs", Duration::from_secs(1), move || async move {
                        let st = Instant::now();
                        sleep(Duration::from_millis(10)).await;
                        let et = Instant::now();
                        s.lock()
                            .await
                            .push((st.elapsed().as_micros(), et.elapsed().as_micros()));
                    })
                        .await
                        .unwrap();
                }));
            }
            join_all(jobs).await;
            let v = spans.lock().await.clone();
            for w in v.windows(2) {
                assert!(w[0].1 <= w[1].0, "critical sections overlapped");
            }
        }

        /// zero-timeout semantics
        #[tokio::test]
        async fn try_lock_semantics() {
            let lock = DefaultLock::default();
            lock.with_lock("demo", Duration::ZERO, || async {}).await.unwrap();

            let l2 = lock.clone();
            let _guard = task::spawn(async move {
                l2.with_lock("demo", Duration::from_secs(1), || async {
                    sleep(Duration::from_millis(50)).await;
                })
                    .await
                    .unwrap();
            });

            sleep(Duration::from_millis(5)).await;

            let err = lock
                .with_lock("demo", Duration::ZERO, || async {})
                .await
                .unwrap_err();
            assert_eq!(err, LockError::Timeout(Duration::ZERO));
        }
        /// nested re-entrancy
        cross_test! { reentrant_lock {
            use tokio::time::{sleep, Duration};

            let lock = DefaultLock::default();

            // The receiver we call the method on
            let lock_outer = lock.clone();

            // Create a *second* clone that we are allowed to move
            let lock_for_closure = lock_outer.clone();

            let res = lock_outer
                .with_reentrant("reentrant", move || {
                    // Inside the closure we can freely clone / move this handle
                    let lock_inner = lock_for_closure.clone();

                    async move {
                        sleep(Duration::from_millis(10)).await;

                        lock_inner
                            .with_reentrant("reentrant", || async {
                                sleep(Duration::from_millis(10)).await;
                                42
                            })
                            .await
                    }
                })
                .await;

            assert_eq!(res.unwrap(), Ok(42));
        } }


        /// timeout propagates
        #[tokio::test]
        async fn timeout_propagates() {
            use std::future::Future;

            #[derive(Clone, Default)]
            struct Stubborn;
            #[async_trait::async_trait]
            impl GlobalLock for Stubborn {
                async fn with_lock<R, F, Fut>(
                    &self,
                    _: &str,
                    t: Duration,
                    _: F,
                ) -> Result<R, LockError>
                where
                    F: FnOnce() -> Fut + Send + 'static,
                    Fut: Future<Output=R> + Send + 'static,
                    R: Send + 'static,
                {
                    sleep(t + Duration::from_millis(5)).await;
                    Err(LockError::Timeout(t))
                }
            }

            let err = Stubborn::default()
                .with_lock("x", Duration::from_millis(1), || async {})
                .await
                .unwrap_err();
            assert!(matches!(err, LockError::Timeout(_)));
        }

        /// panic inside CS must not poison the lock
        #[tokio::test(flavor = "multi_thread")]
        async fn lock_released_after_panic() {
            use futures::FutureExt;

            let lock = DefaultLock::default();

            {
                let l = lock.clone();
                tokio::spawn(async move {
                    let _ = std::panic::AssertUnwindSafe(
                        l.with_lock("boom", Duration::from_secs(1), || async {
                            sleep(Duration::from_millis(5)).await;
                            panic!("intentional!");
                        }),
                    )
                        .catch_unwind()
                        .await;
                });
            }

            sleep(Duration::from_millis(10)).await;

            let res = lock
                .with_lock("boom", Duration::from_millis(20), || async {})
                .await;

            assert!(res.is_ok(), "lock remained poisoned: {res:?}");
        }
    }

    /*===========================  wasm  ===============================*/
    #[cfg(all(feature = "browser", target_arch = "wasm32"))]
    mod wasm {
        use super::super::*;
        use gloo_timers::future::TimeoutFuture;
        use wasm_bindgen_test::*;
        wasm_bindgen_test_configure!(run_in_browser);

        fn micros() -> u128 {
            web_sys::window()
                .unwrap()
                .performance()
                .unwrap()
                .now() as u128
                * 1_000
        }

        #[wasm_bindgen_test]
        async fn free_trylock_succeeds() {
            DefaultLock::default()
                .with_lock("free", Duration::ZERO, || async {})
                .await
                .unwrap();
        }

        #[wasm_bindgen_test]
        async fn trylock_times_out_when_busy() {
            let lock = DefaultLock::default();
            {
                let l = lock.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    l.with_lock("busy", Duration::from_secs(1), || async {
                        TimeoutFuture::new(50).await;
                    })
                        .await
                        .unwrap();
                });
            }

            TimeoutFuture::new(5).await;

            let err = lock
                .with_lock("busy", Duration::ZERO, || async {})
                .await
                .unwrap_err();
            assert_eq!(err, LockError::Timeout(Duration::ZERO));
        }

        #[wasm_bindgen_test]
        async fn serialises_same_name() {
            let lock = DefaultLock::default();
            let mut ts = vec![];

            for _ in 0..3 {
                let l = lock.clone();
                ts.push(async move {
                    l.with_lock("fifo", Duration::from_secs(1), || async {
                        let st = micros();
                        TimeoutFuture::new(20).await;
                        let et = micros();
                        (st, et)
                    })
                        .await
                        .unwrap()
                });
            }

            let mut spans = futures::future::join_all(ts).await;
            spans.sort_by_key(|s| s.0);
            for pair in spans.windows(2) {
                assert!(pair[1].0 >= pair[0].1, "overlap detected");
            }
        }

        #[wasm_bindgen_test]
        async fn distinct_names_overlap() {
            let a = DefaultLock::default();
            let b = a.clone();

            let t1 = a.with_lock("A", Duration::from_secs(1), || async {
                TimeoutFuture::new(30).await;
                micros()
            });

            let t2 = b.with_lock("B", Duration::from_secs(1), || async {
                TimeoutFuture::new(5).await;
                micros()
            });

            let (end_a, end_b) = futures::join!(t1, t2);
            assert!(end_b.unwrap() < end_a.unwrap());
        }
    }
}
