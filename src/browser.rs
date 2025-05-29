//! browser.rs – Navigator-locks backend for cross-locks
//!
//! Changelog vs. earlier draft
//! ---------------------------
//! * Added explicit `R: 'static` bound on `reentrant_inner`.
//! * Never capture a `&str` in the `'static` closure – we clone it
//!   into an owned `String` instead.
//! * Timeout handling (0 / finite / ∞) identical on stable & unstable
//!   APIs.
//! * Kept the public `GlobalLock` trait surface exactly the same.

use super::*;
use futures::channel::oneshot;
use std::{
    cell::RefCell,
    collections::HashSet,
    rc::Rc,
    time::Duration,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{closure::Closure, JsCast};
use wasm_bindgen_futures::future_to_promise;
use web_sys::AbortController;

/* ────────── 1.  JS-API selection ────────── */

#[cfg(web_sys_unstable_apis)]
mod api {
    pub(crate) use web_sys::{LockManager, LockOptions};
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
    pub(crate) fn request_lock(n: &str, opts: &JsValue, cb: &js_sys::Function) {
        req_lock(n, opts, cb);
    }
}

/* ────────── 2.  task-local helpers ────────── */

thread_local! {
    /// Names already *owned* by **this** async-task via `with_reentrant`.
    static OWNED : RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

thread_local! {
    /// Names for which this task currently holds the browser lock.
    /// Needed to make `timeout == 0` “try-lock” re-entrant-safe.
    static ACTIVE: RefCell<HashSet<String>> = RefCell::new(HashSet::new());
}

/* ────────── 3.  Navigator backend ────────── */

#[derive(Clone, Debug, Default)]
pub struct NavigatorLock;

impl NavigatorLock {
    /// Internal helper that injects re-entrancy on top of the raw lock.
    async fn reentrant_inner<R: 'static, F, Fut>(
        &self,
        name: &str,
        f: F,
    ) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output=R> + 'static,
    {
        /* fast path – already inside */
        if OWNED.with(|s| s.borrow().contains(name)) {
            return Ok(f().await);
        }

        /* first entry – acquire exactly once */
        let name_owned = name.to_owned();
        self.with_lock(name, Duration::MAX, move || {
            let name_cloned = name_owned.clone();
            async move {
                OWNED.with(|s| s.borrow_mut().insert(name_cloned.clone()));
                let out = f().await;
                OWNED.with(|s| s.borrow_mut().remove(&name_cloned));
                out
            }
        })
            .await
    }
}

#[async_trait(?Send)]
impl GlobalLock for NavigatorLock {
    /* ── public re-entrant entry point ── */
    async fn with_reentrant<'a, R, F, Fut>(
        &'a self,
        name: &'a str,
        f: F,
    ) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output=R> + 'static,
        R: 'static,
    {
        self.reentrant_inner(name, f).await
    }

    /* ── raw browser lock ── */
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
        /* prevent re-entrant try-lock success */
        if timeout.is_zero() && ACTIVE.with(|s| s.borrow().contains(name)) {
            return Err(LockError::Timeout(Duration::ZERO));
        }

        /* channel for Rust result */
        let (tx, rx) = oneshot::channel::<R>();

        /* shared state for JS callback */
        let tx_cell = Rc::new(RefCell::new(Some(tx)));
        let op_cell = Rc::new(RefCell::new(Some(op)));
        let name_rc = Rc::new(name.to_owned());

        let js_cb = Closure::<dyn FnMut() -> js_sys::Promise>::wrap(Box::new({
            let tx_cell = tx_cell.clone();
            let op_cell = op_cell.clone();
            let name_rc = name_rc.clone();

            move || {
                ACTIVE.with(|s| s.borrow_mut().insert((*name_rc).clone()));

                let tx_cell2 = tx_cell.clone();
                let op_cell2 = op_cell.clone();
                let name_rc2 = name_rc.clone();

                future_to_promise(async move {
                    let tx = tx_cell2.borrow_mut().take().expect("tx reused");
                    let op = op_cell2.borrow_mut().take().expect("op reused");
                    let _ = tx.send(op().await);
                    ACTIVE.with(|s| s.borrow_mut().remove(&*name_rc2));
                    Ok(JsValue::UNDEFINED)
                })
            }
        }));

        /* -------- stable & unstable API branches -------- */

        #[cfg(web_sys_unstable_apis)]
        {
            use api::*;
            let win = web_sys::window().expect("no Window");
            let locks: LockManager = win.navigator().locks();

            let mut opts = LockOptions::new();
            match timeout {
                t if t.is_zero() => {
                    opts.if_available(true);
                }
                t if t == Duration::MAX => { /* wait forever */ }
                t => {
                    let ac = AbortController::new().unwrap();
                    let sig = ac.signal();
                    let abort = Closure::<dyn FnMut()>::wrap(Box::new(move || ac.abort()));

                    win.set_timeout_with_callback_and_timeout_and_arguments_0(
                        abort.as_ref().unchecked_ref(),
                        t.as_millis().try_into().unwrap_or(i32::MAX),
                    ).expect("setTimeout failed");

                    abort.forget();
                    opts.signal(&sig);
                }
            }


            locks.request_with_options_and_callback(
                &*name_rc,
                &opts,
                js_cb.as_ref().unchecked_ref(),
            );
        }

        #[cfg(not(web_sys_unstable_apis))]
        {
            use api::request_lock;
            let win = web_sys::window().expect("no Window");

            let opts_obj = {
                let o = js_sys::Object::new();
                js_sys::Reflect::set(&o, &"mode".into(), &"exclusive".into()).unwrap();

                match timeout {
                    t if t.is_zero() => {
                        // try-lock: `ifAvailable: true`
                        let _ = js_sys::Reflect::set(&o, &"ifAvailable".into(), &JsValue::TRUE).unwrap();
                    }
                    t if t == Duration::MAX => {
                        // infinite wait – nothing to add
                    }
                    t => {
                        // finite deadline – abort after ≤ i32::MAX ms
                        let ac = AbortController::new().unwrap();
                        let sig = ac.signal();
                        let abort = Closure::<dyn FnMut()>::wrap(Box::new(move || ac.abort()));

                        win.set_timeout_with_callback_and_timeout_and_arguments_0(
                            abort.as_ref().unchecked_ref(),
                            t.as_millis().try_into().unwrap_or(i32::MAX),
                        ).expect("setTimeout failed");

                        abort.forget();
                        let _ = js_sys::Reflect::set(&o, &"signal".into(), &sig).unwrap();
                    }
                }
                o
            };

            request_lock(
                &*name_rc,
                &opts_obj.into(),
                js_cb.as_ref().unchecked_ref(),
            );
        }

        js_cb.forget(); // drop after JS resolves

        rx.await.map_err(|_| LockError::Js("oneshot cancelled".into()))
    }
}

/* ────────── public type alias ────────── */

/// `use cross_locks::DefaultLock` in browser builds.
pub type DefaultBrowser = NavigatorLock;
