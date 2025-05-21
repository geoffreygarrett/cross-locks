//! # supabase-locks
//!
//! A **single-file, zero-dependency** (outside of `async-trait`, `thiserror` and
//! optional Tokio / wasm‐bindgen) implementation of Supabase/GoTrue-style
//! _global exclusive locks_ that work **identically**:
//!
//! | target                     | feature flag | backend                         |
//! |----------------------------|--------------|---------------------------------|
//! | Native (Tokio, multi-thread)| `native`     | fair FIFO queue (`Notify`)      |
//! | Browser WASM (2022+)*      | `browser`    | `Navigator.locks.request`       |
//! | Tests / single-thread CLI  | _none_       | no-op passthrough               |
//!
//! \*Safari shipped support in 16.4 (2023-03).
//! The crate auto-detects the right bounds (`?Send`) for `wasm32`.
//!
//! ## Guarantees
//!
//! * **FIFO fairness** for all waiters that request the same `name` on native.
//! * **Zero-timeout try-lock** semantics (`Duration::ZERO`) – mirrors JS SDK.
//! * **Task-local re-entrancy** helper shown in the doc-tests below.
//! * Works with `Arc<T>` out of the box – ideal for storing in Hyper / Axum
//!   state or in a `OnceCell`.
//!
//! ```toml
//! # Cargo.toml
//! [dependencies]
//! async-trait = "0.1"
//! thiserror   = "2"
//!
//! # pick ONE backend at compile-time
//! default-features = false
//! features = ["native"]       # or  "browser"
//! ```
//!
//! ```bash
//! # run the exhaustive test-suite (native)
//! cargo test --features nativ e
//!
//! # browser smoke-test (headless Firefox via wasm-pack)
//! wasm-pack test --firefox --headless --features browser
//! ```
#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod utils;

use async_trait::async_trait;
use std::{future::Future, sync::Arc, time::Duration};
use thiserror::Error;

/*────────────────────────────── errors ──────────────────────────────*/

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

/*────────────────────────── core abstraction ───────────────────────*/

/// Trait implemented by every backend.
///
/// Two mutually-exclusive variants are compiled so that the **exact same API**
/// is available, but the **`Send` bounds match the runtime**:
///
/// * native → futures **must be `Send`**
/// * wasm32 → single-threaded, so the bounds disappear (`?Send`)
#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
pub trait GlobalLock: Send + Sync + 'static {
    /// Acquire (or fail) a **named exclusive lock**.
    ///
    /// * `name`   – arbitrary identifier (use dotted namespaces like `auth.cs`).
    /// * `timeout`
    ///   * `>0`  → wait at most that long
    ///   * `0`   → immediate try-lock
    ///   * `<0`  → wait forever
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

/*──────────── blanket impl so `Arc<T>` is a lock by itself ───────────*/

#[cfg(all(feature = "browser", target_arch = "wasm32"))]
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

#[cfg(not(all(feature = "browser", target_arch = "wasm32")))]
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

/*────────────────────────── no-op backend ──────────────────────────*/

/// Passthrough implementation (useful in tests / single-thread CLIs).
#[derive(Clone, Debug, Default)]
pub struct NoopLock;

#[cfg(all(feature = "browser", target_arch = "wasm32"))]
#[async_trait(?Send)]
impl GlobalLock for NoopLock {
    async fn with_lock<R, F, Fut>(&self, _: &str, _t: Duration, op: F) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output=R> + 'static,
        R: 'static,
    { Ok(op().await) }
}

#[cfg(not(all(feature = "browser", target_arch = "wasm32")))]
#[async_trait]
impl GlobalLock for NoopLock {
    async fn with_lock<R, F, Fut>(&self, _: &str, _t: Duration, op: F) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output=R> + Send + 'static,
        R: Send + 'static,
    { Ok(op().await) }
}

/*──────────────────────── native FIFO backend ──────────────────────*/
#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
mod native_fifo {
    use super::*;
    use once_cell::sync::Lazy;
    use std::collections::{HashMap, VecDeque};
    use tokio::sync::{Mutex, Notify};

    type Queue = VecDeque<Arc<Notify>>;
    static QUEUES: Lazy<Mutex<HashMap<String, Queue>>> =
        Lazy::new(|| Mutex::new(HashMap::new()));

    /// Tokio multi-thread FIFO lock – fair ordering & timeout.
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

            // enqueue myself
            let me = Arc::new(Notify::new());
            let pos = {
                let mut map = QUEUES.lock().await;
                let q = map.entry(name.into()).or_default();
                q.push_back(me.clone());
                q.len() - 1
            };

            // wait for turn (or timeout)
            if pos != 0 && time::timeout(timeout, me.notified()).await.is_err() {
                // remove from queue after timeout
                QUEUES
                    .lock()
                    .await
                    .get_mut(name)
                    .map(|q| q.retain(|n| !Arc::ptr_eq(n, &me)));
                return Err(LockError::Timeout(timeout));
            }

            // critical section
            let out = op().await;

            // wake next
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
mod browser {
    use super::*;
    use futures::channel::oneshot;
    use std::{cell::RefCell, collections::HashSet, rc::Rc};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::{closure::Closure, JsCast};
    use wasm_bindgen_futures::future_to_promise;
    use web_sys::AbortController;

    /* ────────── 1.  API selection ────────── */

    #[cfg(web_sys_unstable_apis)]
    mod api {
        pub(crate) use web_sys::{LockManager, LockOptions, NavigatorExt};
    }

    #[cfg(not(web_sys_unstable_apis))]
    mod api {
        use wasm_bindgen::prelude::*;
        #[wasm_bindgen(inline_js = r#"
            export function req_lock(name, opts, cb) {
                return navigator.locks.request(name, opts, cb);
            }"#)]
        extern "C" {
            #[wasm_bindgen(js_name = req_lock)]
            fn req_lock(name: &str, opts: &JsValue, cb: &js_sys::Function);
        }
        pub(crate) fn request_lock(
            n: &str,
            opts: &JsValue,
            cb: &js_sys::Function,
        ) {
            // just a JS call – no Rust-side safety issues
            req_lock(n, opts, cb);
        }
    }

    /* ────────── 2.  task-local re-entrancy guard ────────── */
    thread_local! {
        static ACTIVE: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
    }

    #[derive(Clone, Debug, Default)]
    pub struct NavigatorLock;

    #[async_trait(?Send)]
    impl GlobalLock for NavigatorLock {
        async fn with_lock<R, F, Fut>(
            &self,
            name: &str,
            timeout: Duration,
            op: F,
        ) -> Result<R, LockError>
        where
            F: FnOnce() -> Fut + 'static,
            Fut: Future<Output=R> + 'static,
            R: 'static,
        {
            /* fast-fail for re-entrant try-lock */
            if timeout.is_zero()
                && ACTIVE.with(|s| s.borrow().contains(name))
            {
                return Err(LockError::Timeout(Duration::ZERO));
            }

            /* channel back to Rust */
            let (tx, rx) = oneshot::channel::<R>();

            /* share state between outer callback & inner async move */
            let tx_cell = Rc::new(RefCell::new(Some(tx)));
            let op_cell = Rc::new(RefCell::new(Some(op)));
            let name_rc = Rc::new(name.to_owned());

            let js_cb = Closure::<dyn FnMut() -> js_sys::Promise>::wrap(Box::new({
                // first clones – they live for the whole lifetime of the callback
                let tx_cell = tx_cell.clone();
                let op_cell = op_cell.clone();
                let name_rc = name_rc.clone();

                move || {
                    ACTIVE.with(|s| s.borrow_mut().insert((*name_rc).clone()));

                    // second, “one-shot” clones – owned by the async block only
                    let tx_cell2 = tx_cell.clone();
                    let op_cell2 = op_cell.clone();
                    let name_rc2 = name_rc.clone();

                    future_to_promise(async move {
                        let tx = tx_cell2
                            .borrow_mut()
                            .take()
                            .expect("tx reused");
                        let op = op_cell2
                            .borrow_mut()
                            .take()
                            .expect("op reused");

                        let _ = tx.send(op().await);

                        ACTIVE.with(|s| s.borrow_mut().remove(&*name_rc2));
                        Ok(JsValue::UNDEFINED)
                    })
                }
            }));


            /* build options and call LockManager */

            #[cfg(web_sys_unstable_apis)]
            {
                use api::*;
                let win = web_sys::window().expect("no Window");
                let locks: LockManager = win.navigator().locks();

                let mut opts = LockOptions::new();
                if timeout.is_zero() {
                    opts.if_available(true);
                } else {
                    let ac = AbortController::new().unwrap();
                    let sig = ac.signal();
                    let abort =
                        Closure::<dyn FnMut()>::wrap(Box::new(move || ac.abort()));
                    win.set_timeout_with_callback_and_timeout_and_arguments_0(
                        abort.as_ref().unchecked_ref(),
                        timeout.as_millis() as i32,
                    )
                        .unwrap();
                    abort.forget();
                    opts.signal(Some(&sig));
                }

                let _ = locks.request_with_options_and_callback(
                    &*name_rc,
                    &opts,
                    js_cb.as_ref().unchecked_ref(),
                );
            }

            #[cfg(not(web_sys_unstable_apis))]
            {
                use api::request_lock;

                let opts_obj = {
                    let o = js_sys::Object::new();
                    js_sys::Reflect::set(&o, &"mode".into(), &"exclusive".into())
                        .unwrap();

                    if timeout.is_zero() {
                        js_sys::Reflect::set(
                            &o,
                            &"ifAvailable".into(),
                            &JsValue::TRUE,
                        )
                            .unwrap();
                    } else {
                        let ac = AbortController::new().unwrap();
                        let sig = ac.signal();
                        let abort =
                            Closure::<dyn FnMut()>::wrap(Box::new(move || ac.abort()));
                        web_sys::window()
                            .unwrap()
                            .set_timeout_with_callback_and_timeout_and_arguments_0(
                                abort.as_ref().unchecked_ref(),
                                timeout.as_millis() as i32,
                            )
                            .unwrap();
                        abort.forget();
                        js_sys::Reflect::set(&o, &"signal".into(), &sig).unwrap();
                    }
                    o
                };

                request_lock(
                    &*name_rc,
                    &opts_obj.into(),
                    js_cb.as_ref().unchecked_ref(),
                );
            }

            js_cb.forget(); /* one-shot */

            rx.await.map_err(|_| LockError::Js("oneshot cancelled".into()))
        }
    }

    /// `use supabase_locks::DefaultLock` in WASM builds.
    pub type DefaultBrowser = NavigatorLock;
}


/*──────── re-exports so user can depend on exactly one name ────────*/
#[cfg(all(feature = "native", not(target_arch = "wasm32")))]
pub type DefaultLock = native_fifo::DefaultNative;

#[cfg(all(feature = "browser", target_arch = "wasm32"))]
pub type DefaultLock = browser::DefaultBrowser;

/*────────────────────────── exhaustive tests ───────────────────────*/
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
