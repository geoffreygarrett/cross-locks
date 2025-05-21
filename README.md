<!-- README.md – cross-locks -->
<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/geoffreygarrett/cross-locks/main/.github/banner-dark.svg">
    <img alt="cross-locks" src="https://raw.githubusercontent.com/geoffreygarrett/cross-locks/main/.github/banner-light.svg" width="660">
  </picture>
</p>

<p align="center">
  <a href="https://crates.io/crates/cross-locks"><img alt="crates.io" src="https://img.shields.io/crates/v/cross-locks.svg"></a>
  <a href="https://docs.rs/cross-locks"><img alt="docs.rs" src="https://docs.rs/cross-locks/badge.svg"></a>
  <a href="https://github.com/geoffreygarrett/cross-locks/actions?query=workflow%3Aci"><img alt="CI" src="https://github.com/geoffreygarrett/cross-locks/workflows/ci/badge.svg"></a>
</p>

> **cross-locks** provides a single-file, zero-dependency¹ implementation of **named, FIFO, re-entrant global locks that
Just Work everywhere** – native back-ends, WASM in the browser, and single-threaded test binaries – with identical APIs
> and semantics.

<sub>¹ Apart from the optional runtime adapters (`tokio`, `wasm-bindgen`) and the tiny derive helpers `async-trait` /
`thiserror`.</sub>

---

## ✨ Why?

* **Cross-platform parity** – ship the same locking guarantees in browsers (via `Navigator.locks`) or on the server
  without extra feature flags in your application code.
* **FIFO fairness** on native (using a Tokio queue); browser fairness delegated to the spec.
* **Zero-timeout “try-lock”** semantics that mirror Supabase / GoTrue behaviour.
* **Task-local re-entrancy** – a lock holder can call itself recursively without dead-locking.
* **`Arc<T>` auto-impl** – store the lock directly in shared state (e.g. Axum `Extension`).
* **Compile-time selection** – one concrete backend is compiled in; no runtime branches.

## 🚀 Quick start

```toml
# Cargo.toml
[dependencies]
cross-locks = { version = "0.1", default-features = false, features = ["native"] } # or "browser"
tokio = { version = "1", optional = true, features = ["sync", "rt-multi-thread"] }
````

```rust
use cross_locks::{GlobalLock, DefaultLock};
use std::time::Duration;

#[tokio::main]
async fn main() {
    let lock = DefaultLock::default();

    lock.with_lock("auth.cs", Duration::from_secs(1), || async {
        // … critical section …
    }).await.unwrap();
}
```

### Feature matrix

| Target                          | `--features` | Crate size | Guarantees                     |
|---------------------------------|--------------|-----------:|--------------------------------|
| **Native** (Tokio multi-thread) | `native`     |       tiny | FIFO, timeout, re-entry        |
| **Browser WASM** (2022+)        | `browser`    |       tiny | Delegates to `Navigator.locks` |
| **Tests / single-thread CLI**   | *(none)*     |       tiny | No-op passthrough              |

> Safari ≥ 16.4, Chrome ≥ 69, Firefox ≥ 94 already ship the Web Locks API.

## 🧩 Using your own runtime

`GlobalLock` is an async-trait with a blanket impl for `Arc<T>`, so you can plug in bespoke back-ends (e.g. a Postgres
advisory lock):

```rust
struct PgLock {
    pool: sqlx::PgPool
}

#[async_trait::async_trait]
impl GlobalLock for PgLock {
    async fn with_lock<R, F, Fut>(
        &self, name: &str, timeout: Duration, op: F
    ) -> Result<R, cross_locks::LockError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output=R> + Send + 'static,
        R: Send + 'static,
    {
        // acquire advisory lock, run op, release …
    }
}
```

## 🛠️ Tooling & tests

```bash
# exhaustive suite on native
cargo test --features native

# smoke-test in headless Firefox
wasm-pack test --firefox --headless --features browser
```

The repository ships a tiny **`dual_test!` macro** so you can write once, run everywhere:

```rust
cross_locks::dual_test! { arc_inner_reentrancy {
    /* body compiled as a Tokio test natively and as a wasm_bindgen_test in browsers */
}}
```

## 📜 License

Licensed under either 🅰 **Apache-2.0** or 🅱 **MIT** at your option – see [`LICENSE-*`](./LICENSE-Apache) for details.

## ❤️ Acknowledgements

Inspired by Supabase’s JS SDK and the GoTrue service; thanks to the Web Locks API editors for a sane browser primitive.

---

*Happy hacking & safe locking!*  — @geoffreygarrett
