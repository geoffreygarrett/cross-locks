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

/// Shorthand when you **don’t** need a dedicated module on wasm
/// (i.e. the test name is unique anyway).
/// Generates either a `tokio::test` **or** a `wasm_bindgen_test`
/// for the same async function.
///
/// ```rust
/// cross_locks::dual_test! { my_other_test {
///     // …
/// }}
/// ```
#[macro_export]
macro_rules! dual_test {
    ($name:ident $body:block) => {
        #[cfg_attr(
            all(target_arch = "wasm32", feature = "browser"),
            wasm_bindgen_test::wasm_bindgen_test
        )]
        #[cfg_attr(
            not(all(target_arch = "wasm32", feature = "browser")),
            tokio::test(flavor = "multi_thread", worker_threads = 4)
        )]
        async fn $name() $body
    };
}
