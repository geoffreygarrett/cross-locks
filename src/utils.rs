/*────────────────── cross-platform test helpers ──────────────────*/

/// Expands to `tokio::test` on native targets and to a
/// `wasm_bindgen_test` **module** on `wasm32` (so the name doesn’t
/// clash with the outer function space).
///
/// ```rust
/// cross_locks::async_test!(my_test {
///     /* … runs on both platforms … */
/// });
/// ```
#[macro_export]
macro_rules! async_test {
    ($name:ident $body:block) => {
        /* ---- Browser (wasm32 + feature=browser) ------------------- */
        #[cfg(target_arch = "wasm32")]
        mod $name {
            use super::*;
            use wasm_bindgen_test::*;
            #[cfg(feature = "browser")]
            wasm_bindgen_test_configure!(run_in_browser);

            #[wasm_bindgen_test]
            async fn run() $body
        }

        /* ---- Native (everything else) ---------------------------- */
        #[cfg(not(all(target_arch = "wasm32")))]
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() $body
    };
}


#[macro_export]
macro_rules! cross_test {
    ($name:ident $body:block) => {
        /* ---- Browser (wasm32 + feature=browser) ------------------- */
        #[cfg(target_arch = "wasm32")]
        mod $name {
            use super::*;
use wasm_bindgen_test::*;

            #[cfg(feature = "browser")]
            wasm_bindgen_test_configure!(run_in_browser);

            #[wasm_bindgen_test]
            async fn run() $body
        }

        /* ---- Native (everything else) ---------------------------- */
        #[cfg(not(all(target_arch = "wasm32")))]
        #[tokio::test]
        // #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
        // #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $name() $body
    };
}

