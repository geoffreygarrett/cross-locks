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