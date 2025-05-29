//! One source - runs under *native* (`cargo test --features native`)
//! …and under *browser Wasm* (`wasm-pack test --firefox --headless --features browser`).

use cross_locks::{cross_test, DefaultLock, GlobalLock};
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

/* ─────────────── spawn abstraction (trait + impls) ─────────────── */

/// Two slightly different signatures are needed because `tokio::spawn`
/// requires a `Send` future while `wasm_bindgen_futures::spawn_local` does not.
#[cfg(not(target_arch = "wasm32"))]
trait Spawn {
    type Handle;
    fn spawn<F>(fut: F) -> Self::Handle
    where
        F: std::future::Future<Output=usize> + Send + 'static;

    async fn join_all(handles: Vec<Self::Handle>);
}

#[cfg(target_arch = "wasm32")]
trait Spawn {
    type Handle;
    fn spawn<F>(fut: F) -> Self::Handle
    where
        F: std::future::Future<Output=usize> + 'static;

    async fn join_all(handles: Vec<Self::Handle>);
}

/* ------------------- native (Tokio) implementation ------------------- */

#[cfg(not(target_arch = "wasm32"))]
mod spawner_native {
    use super::*;
    pub struct Tokio;

    impl Spawn for Tokio {
        type Handle = tokio::task::JoinHandle<usize>;

        fn spawn<F>(fut: F) -> Self::Handle
        where
            F: std::future::Future<Output=usize> + Send + 'static,
        {
            tokio::spawn(fut)
        }

        async fn join_all(handles: Vec<Self::Handle>) {
            for h in handles {
                h.await.expect("task panicked");
            }
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
use spawner_native::Tokio as Spawner;

// #[cfg(all(target_arch = "wasm32", feature = "browser"))]
// pub async fn delay_ms(ms: u64) {
//     use web_sys::js_sys::Promise;
//     use wasm_bindgen::{closure::Closure, JsCast};
//     use wasm_bindgen_futures::JsFuture;
//
//     // build a JS Promise that resolves after `ms` ms
//     let promise = Promise::new(&mut |resolve, _reject| {
//         let _ = web_sys::window()
//             .unwrap()
//             .set_timeout_with_callback_and_timeout_and_arguments_0(
//                 resolve.unchecked_ref(),
//                 ms as i32,
//             );
//     });
//
//     // turn it into a Rust `Future`
//     let _ = JsFuture::from(promise).await;
// }

#[cfg(all(target_arch = "wasm32"))]
pub async fn delay_ms(ms: u64) {
    gloo_timers::future::TimeoutFuture::new(ms as u32).await;
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn delay_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

/* --------------------- browser / Wasm implementation ----------------- */

#[cfg(target_arch = "wasm32")]
mod spawner_wasm {
    use super::*;
    use futures::channel::oneshot;
    use wasm_bindgen_futures::spawn_local;

    pub struct Local;

    impl Spawn for Local {
        type Handle = oneshot::Receiver<usize>;

        fn spawn<F>(fut: F) -> Self::Handle
        where
            F: std::future::Future<Output=usize> + 'static,
        {
            let (tx, rx) = oneshot::channel();
            spawn_local(async move {
                let _ = tx.send(fut.await);
            });
            rx
        }

        async fn join_all(handles: Vec<Self::Handle>) {
            for h in handles {
                h.await.expect("task cancelled");
            }
        }
    }
}
#[cfg(target_arch = "wasm32")]
use spawner_wasm::Local as Spawner;

/* ─────────────── shared fixtures (identical on both targets) ───────── */

#[derive(Default)]
struct State {
    counter: usize,
}

#[derive(Clone)]
struct Client {
    state: Arc<Mutex<State>>,
    lock: Arc<DefaultLock>,
}

impl Client {
    fn new() -> Self {
        Self {
            state: Arc::default(),
            lock: Arc::new(DefaultLock::default()),
        }
    }

    async fn inc(&self) -> usize {
        let state = Arc::clone(&self.state);
        self.lock
            .with_lock("demo", Duration::from_secs(1), move || async move {
                let mut st = state.lock().await;
                st.counter += 1;
                st.counter
            })
            .await
            .unwrap()
    }

    async fn value(&self) -> usize {
        self.state.lock().await.counter
    }
}

/* ─────────────────────────── the dual test ─────────────────────────── */

cross_test! { arc_inner_reentrancy {
    let cli = Client::new();

    // fire 50 concurrent increments
    let handles: Vec<_> = (0..50)
        .map(|_| {
            let c = cli.clone();
            Spawner::spawn(async move { c.inc().await })
        })
        .collect();

    Spawner::join_all(handles).await;

    assert_eq!(cli.value().await, 50);
}}


// /// nested re-entrancy
cross_test! { reentrant_lock {

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
                        // delay_ms(10).await;

                        lock_inner
                            .with_reentrant("reentrant", || async {
                                // delay_ms(10).await;
                                42
                            })
                            .await
                    }
                })
                .await;

            assert_eq!(res.unwrap(), Ok(42));
        } }
