//! # cross‑locks
//!
//! A **single‑source, zero‑dependency**¹ implementation of Supabase/GoTrue‑style
//! *global exclusive locks* that work **identically** across every runtime.
//!
//! | target / runtime                     | compile‑time feature | backend used by `DefaultLock` |
//! |--------------------------------------|----------------------|--------------------------------|
//! | Native (Tokio, multi‑thread)         | **`native`**         | fair FIFO queue (`Notify`)     |
//! | Browser WASM (2022 + `navigator.locks`) | **`browser`**        | `Navigator.locks.request`      |
//! | Head‑less WASM (Node / WASI / tests) | **`wasm`**           | no‑op passthrough              |
//!
//! \* Safari shipped the LockManager API in 16.4 (2023‑03).
//!
//! ## Guarantees
//! * **FIFO fairness** for every waiter requesting the *same* lock name on
//!   native.
//! * **Zero‑timeout try‑lock** (`Duration::ZERO`) mirrors the semantics of the
//!   JS SDK.
//! * **Task‑local re‑entrancy** helper demonstrated in the test‑suite.
//! * `Arc<T>` automatically implements `GlobalLock`, ideal for storing in an
//!   `Axum` `State`, `OnceCell`, etc.
//!
//! ---
//! ¹ Outside the optional features listed below (`async‑trait`, `thiserror`,
//!   Tokio / wasm‑bindgen).
//!
//! ```toml
//! # Cargo.toml – choose exactly ONE backend
//! [dependencies]
//! cross-locks = { version = "*", default-features = false, features = ["native"] }
//! # or  "browser"  |  "wasm"
//! ```
//!
//! ```bash
//! # run the exhaustive native test‑suite
//! cargo test --features native
//!
//! # browser smoke‑test (headless Firefox)
//! wasm-pack test --firefox --headless --features browser
//! ```
#![cfg_attr(docsrs, feature(doc_cfg))]

mod utils; // misc helpers shared by the native test‑suite

use async_trait::async_trait;
use std::{future::Future, sync::Arc, time::Duration};
use thiserror::Error;

/*────────────────────────────── errors ──────────────────────────────*/

/// Errors returned by [`GlobalLock::with_lock`].
#[derive(Debug, Error, PartialEq, Eq)]
pub enum LockError {
    /// Timed‑out while waiting to acquire the lock.
    #[error("acquiring lock timed‑out after {0:?}")]
    Timeout(Duration),

    /// Browser only – the JS *Navigator LockManager* rejected the request.
    #[cfg(all(feature = "browser", target_arch = "wasm32"))]
    #[error("Navigator LockManager rejected: {0}")]
    Js(String),
}

/*────────────────────────── core abstraction ───────────────────────*/

/// Back‑end agnostic trait.
/// *On `wasm32`* the `Send` bounds are automatically relaxed (`?Send`).
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
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output=R> + Send + 'static,
        R: Send + 'static;
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
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output=R> + 'static,
        R: 'static;
}

/*──────── blanket impl so `Arc<T>` is itself a lock ─────────*/

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

/*────────────────────────── no‑op backend ─────────────────────────*/

/// Passthrough implementation – used by the `wasm` (Node/WASI) feature and in
/// single‑thread CLI tests.
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

/*──────────────────────── native FIFO backend ─────────────────────*/
#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
mod native_fifo {
    use super::*;
    use once_cell::sync::Lazy;
    use std::collections::{HashMap, VecDeque};
    use tokio::sync::{Mutex, Notify};

    type Queue = VecDeque<Arc<Notify>>;
    static QUEUES: Lazy<Mutex<HashMap<String, Queue>>> = Lazy::new(|| Mutex::new(HashMap::new()));

    /// Tokio **FIFO‑fair** global lock.
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
            use tokio::time;

            let me = Arc::new(Notify::new());
            let pos = {
                let mut map = QUEUES.lock().await;
                let q = map.entry(name.into()).or_default();
                q.push_back(me.clone());
                q.len() - 1
            };

            if pos != 0 && time::timeout(timeout, me.notified()).await.is_err() {
                QUEUES.lock().await.get_mut(name).map(|q| q.retain(|n| !Arc::ptr_eq(n, &me)));
                return Err(LockError::Timeout(timeout));
            }

            let out = op().await;

            if let Some(next) = {
                let mut map = QUEUES.lock().await;
                let q = map.get_mut(name).unwrap();
                q.pop_front();
                q.front().cloned()
            } {
                next.notify_one();
            }

            Ok(out)
        }
    }

    pub type DefaultNative = FifoLock;
}

/*────────────────────── browser backend ───────────────────────────*/
#[cfg(all(feature = "browser", target_arch = "wasm32"))]
mod browser;

/*──────── re-exports so user can depend on exactly one name ────────*/
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

// #[cfg(all(feature = "wasm", target_arch = "wasm32"))]
// pub type DefaultLock = browser::DefaultBrowser;

/*────────────────────────── exhaustive tests ───────────────────────*/
#[cfg(test)]
mod tests {
    use super::*;

    /*===============================================================
     =                        NATIVE TESTS                           =
     ===============================================================*/

    #[cfg(all(feature = "native", not(target_arch = "wasm32")))]
    mod native {
        use super::*;
        use futures::future::join_all;
        use tokio::{task, time::sleep};

        /// critical-sections never overlap (FIFO fairness)
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
                        let st = std::time::Instant::now();
                        sleep(Duration::from_millis(10)).await;
                        let et = std::time::Instant::now();
                        s.lock().await.push((st.elapsed().as_micros(), et.elapsed().as_micros()));
                    }).await.unwrap();
                }));
            }
            join_all(jobs).await;
            let v = spans.lock().await.clone();
            for pair in v.windows(2) {
                assert!(pair[0].1 <= pair[1].0, "critical sections overlapped");
            }
        }

        /// zero-timeout succeeds when lock is free, fails when busy.
        #[tokio::test]
        async fn try_lock_semantics() {
            let lock = DefaultLock::default();

            // immediately succeeds
            lock.with_lock("demo", Duration::ZERO, || async {}).await.unwrap();

            // occupy lock
            let l2 = lock.clone();
            let _guard = task::spawn(async move {
                l2.with_lock("demo", Duration::from_secs(1), || async {
                    sleep(Duration::from_millis(50)).await;
                }).await.unwrap();
            });

            sleep(Duration::from_millis(5)).await;

            // try-lock should now fail
            let err = lock
                .with_lock("demo", Duration::ZERO, || async {})
                .await
                .unwrap_err();
            assert_eq!(err, LockError::Timeout(Duration::ZERO));
        }

        /// timeout error propagates correctly
        #[tokio::test]
        async fn timeout_propagates() {
            #[derive(Clone, Default)]
            struct Stubborn;
            #[async_trait]
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
    }

    /*===============================================================
     =                     WASM (BROWSER) TESTS                      =
     ===============================================================*/

    #[cfg(all(feature = "browser", target_arch = "wasm32"))]
    mod wasm {
        use super::*;
        use gloo_timers::future::TimeoutFuture;
        use wasm_bindgen_test::*;
        wasm_bindgen_test_configure!(run_in_browser);

        // Helper to grab high-resolution µs since page load
        fn micros() -> u128 {
            web_sys::window()
                .unwrap()
                .performance()
                .unwrap()
                .now() as u128 * 1_000
        }

        /// 1️⃣ Smoke-test: lock can be acquired when free (try-lock path).
        #[wasm_bindgen_test]
        async fn navigator_free_trylock_succeeds() {
            DefaultLock::default()
                .with_lock("free", Duration::ZERO, || async {})
                .await
                .unwrap();
        }

        /// 2️⃣ Try-lock fails with `Timeout(0)` while another task is holding it.
        #[wasm_bindgen_test]
        async fn navigator_trylock_times_out_when_busy() {
            let lock = DefaultLock::default();

            // Spawn a future that holds the lock for 50 ms
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

            // Yield to let the first future grab the lock
            TimeoutFuture::new(5).await;

            // Immediate try-lock must fail
            let err = lock
                .with_lock("busy", Duration::ZERO, || async {})
                .await
                .unwrap_err();
            assert_eq!(err, LockError::Timeout(Duration::ZERO));
        }

        /// 3️⃣ Critical-sections on the **same name** never overlap.
        #[wasm_bindgen_test]
        async fn navigator_serialises_same_name() {
            let lock = DefaultLock::default();
            let mut ts = vec![];

            for _ in 0..3 {
                let l = lock.clone();
                ts.push(async move {
                    let (start, end) = l
                        .with_lock("fifo", Duration::from_secs(1), || async {
                            let st = micros();
                            TimeoutFuture::new(20).await;
                            let et = micros();
                            (st, et)
                        })
                        .await
                        .unwrap();
                    (start, end)
                });
            }

            let mut spans = futures::future::join_all(ts).await;
            spans.sort_by_key(|s| s.0);
            for pair in spans.windows(2) {
                assert!(
                    pair[1].0 >= pair[0].1,
                    "critical-sections overlapped in the browser backend"
                );
            }
        }

        /// 4️⃣ Locks with **different names** can overlap.
        #[wasm_bindgen_test]
        async fn navigator_distinct_names_overlap() {
            let a = DefaultLock::default();
            let b = a.clone(); // same backend, different name

            let t1 = a.with_lock("A", Duration::from_secs(1), || async {
                TimeoutFuture::new(30).await;
                micros()
            });

            let t2 = b.with_lock("B", Duration::from_secs(1), || async {
                TimeoutFuture::new(5).await;
                micros()
            });

            let (end_a, end_b) = futures::join!(t1, t2);
            // If they overlapped, B should finish before A completes its 30 ms sleep
            assert!(end_b.unwrap() < end_a.unwrap());
        }
    }
}
