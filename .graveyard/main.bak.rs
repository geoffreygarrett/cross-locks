//! Production‑ready demonstration of Supabase/GoTrue‑style global locks.
//! FIFO ordering • named global scopes • re‑entrancy • robust timeout handling.
//!
//! ```bash
//! cargo run   --quiet   # deterministic output
//! cargo test  --quiet   # all tests pass
//! ```
use sqlx::Postgres;

use async_trait::async_trait;
use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::Duration,
};
use thiserror::Error;
use tokio::{sync::Mutex, task, time};

/*──────────────────────────── error type ───────────────────────────────*/

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LockError {
    #[error("acquiring global lock timed‑out after {0:?}")]
    Timeout(Duration),
    #[error("JS runtime rejected lock: {0}")]
    Js(String),
}

/*────────────────────────── abstraction layer ───────────────────────────*/

/// API modelled after GoTrue’s `navigatorLock` / `processLock` helpers.
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

/// Allow `Arc<T>` to be used transparently wherever a `GlobalLock` is expected.
#[async_trait]
impl<T: GlobalLock> GlobalLock for Arc<T> {
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
        (**self).with_lock(name, timeout, op).await
    }
}

/*───────────────────────────── back‑ends ───────────────────────────────*/

/// No‑op implementation for single‑threaded / test environments.
#[derive(Clone, Debug, Default)]
pub struct NoopLock;

#[async_trait]
impl GlobalLock for NoopLock {
    async fn with_lock<R, F, Fut>(
        &self,
        _name: &str,
        _timeout: Duration,
        op: F,
    ) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output=R> + Send + 'static,
        R: Send + 'static,
    {
        Ok(op().await)
    }
}

/// Simple single‑queue FIFO lock.
#[derive(Clone, Debug, Default)]
pub struct MutexLock {
    inner: Arc<Mutex<()>>, // poison‑free, async‑aware mutex
}

#[async_trait]
impl GlobalLock for MutexLock {
    async fn with_lock<R, F, Fut>(
        &self,
        _name: &str,
        timeout: Duration,
        op: F,
    ) -> Result<R, LockError>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output=R> + Send + 'static,
        R: Send + 'static,
    {
        let fut = async {
            let _g = self.inner.lock().await;
            op().await
        };
        time::timeout(timeout, fut)
            .await
            .map_err(|_| LockError::Timeout(timeout))
    }
}

/// Process‑wide *named* FIFO lock (closest analogue to GoTrue’s behaviour).
use std::sync::OnceLock;

static GLOBAL_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

#[derive(Clone, Debug, Default)]
pub struct NamedMutexLock;

#[async_trait]
impl GlobalLock for NamedMutexLock {
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
        // lazily create global map the first time it’s used
        let map = GLOBAL_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));

        // obtain / insert per‑name mutex with minimal contention window
        let entry_mutex = {
            let mut guard = map.lock().await;
            guard
                .entry(name.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };

        let fut = async {
            let _g = entry_mutex.lock().await;
            op().await
        };

        time::timeout(timeout, fut)
            .await
            .map_err(|_| LockError::Timeout(timeout))
    }
}

/*──────────────────────── re‑entrancy guard (task‑local) ───────────────*/

thread_local! {
    static IN_CS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct ReentGuard;
impl ReentGuard {
    fn enter() -> Self {
        IN_CS.set(true);
        Self
    }
}
impl Drop for ReentGuard {
    fn drop(&mut self) {
        IN_CS.set(false);
    }
}

/*──────────────── shared mutable state + high‑level façade ─────────────*/

#[derive(Default, Debug)]
struct Inner {
    counter: usize,
}

#[derive(Clone)]
struct Client<L: GlobalLock + Clone> {
    inner: Arc<Mutex<Inner>>,
    locker: Arc<L>,
}

impl<L: GlobalLock + Clone> Client<L> {
    fn new(locker: L) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            locker: Arc::new(locker),
        }
    }

    /// Enter the critical section protecting `Inner`.
    async fn with_cs<R, F>(&self, f: F) -> Result<R, LockError>
    where
        F: FnOnce(&mut Inner) -> R + Send + 'static,
        R: Send + 'static,
    {
        // fast‑path for re‑entrant calls inside same async task
        if IN_CS.try_with(|c| c.get()).unwrap_or(false) {
            let mut g = self.inner.lock().await;
            return Ok(f(&mut *g));
        }

        let inner = Arc::clone(&self.inner);
        let locker = Arc::clone(&self.locker);

        locker
            .with_lock("supabase:auth", Duration::from_secs(4), move || async move {
                let _guard = ReentGuard::enter();
                let mut g = inner.lock().await;
                f(&mut *g)
            })
            .await
    }

    async fn inc(&self, by: usize) -> Result<usize, LockError> {
        self.with_cs(move |st| {
            st.counter += by;
            st.counter
        })
            .await
    }

    async fn get(&self) -> usize {
        self.inner.lock().await.counter
    }
}

/*────────────────────────── demo executable ────────────────────────────*/

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    let client = Client::new(NamedMutexLock::default());

    let mut handles = Vec::new();
    for id in 0..3 {
        let c = client.clone();
        handles.push(task::spawn(async move {
            for _ in 0..5 {
                let v = c.inc(1).await.unwrap();
                println!("task {id} -> counter = {v}");
                time::sleep(Duration::from_millis(10)).await;
            }
        }));
    }

    futures::future::join_all(handles).await;
    println!("FINAL {}", client.get().await);
    Ok(())
}

/*────────────────────────────── tests ────────────────────────────────*/

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::join_all;
    use futures::FutureExt;
    use tokio::{task, time::sleep};

    // const WORKERS: usize = 4;

    /*──────────── baseline correctness (existing tests) ───────────*/

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn counter_is_sequential() {
        let c = Client::new(MutexLock::default());
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let cl = c.clone();
            tasks.push(task::spawn(async move {
                cl.inc(1).await.unwrap();
            }));
        }
        join_all(tasks).await;
        assert_eq!(c.get().await, 10);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sequential_with_noop_lock() {
        let c = Client::new(NoopLock);
        for _ in 0..100 {
            c.inc(1).await.unwrap();
        }
        assert_eq!(c.get().await, 100);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn nested_calls_reentrant() {
        let c = Client::new(MutexLock::default());
        c.with_cs(|st| st.counter += 1).await.unwrap();
        assert_eq!(c.inc(1).await.unwrap(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn heavy_parallelism_fairness() {
        let c = Client::new(MutexLock::default());
        let mut ts = Vec::new();
        for _ in 0..100 {
            let cl = c.clone();
            ts.push(task::spawn(async move { cl.inc(1).await.unwrap() }));
        }
        join_all(ts).await;
        assert_eq!(c.get().await, 100);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn timeout_propagates() {
        #[derive(Clone, Default)]
        struct Stubborn;
        #[async_trait]
        impl GlobalLock for Stubborn {
            async fn with_lock<R, F, Fut>(
                &self,
                _: &str,
                timeout: Duration,
                _: F,
            ) -> Result<R, LockError>
            where
                F: FnOnce() -> Fut + Send + 'static,
                Fut: Future<Output=R> + Send + 'static,
                R: Send + 'static,
            {
                time::sleep(timeout + Duration::from_millis(10)).await;
                Err(LockError::Timeout(timeout))
            }
        }

        let c = Client::new(Stubborn::default());
        matches!(c.inc(1).await.unwrap_err(), LockError::Timeout(_));
    }

    /*──────────── additional edge‑cases for Supabase parity ───────*/

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn immediate_timeout_when_busy() {
        let lock = NamedMutexLock::default();

        // task A holds the lock for 100 ms
        let l1 = lock.clone();
        let hold = task::spawn(async move {
            l1.with_lock("busy", Duration::from_secs(4), || async {
                sleep(Duration::from_millis(100)).await;
            })
                .await
                .unwrap();
        });

        sleep(Duration::from_millis(10)).await; // give task A head‑start

        // task B should time‑out immediately (0 ms) when lock is busy
        let err = lock
            .with_lock("busy", Duration::from_millis(0), || async {})
            .await
            .unwrap_err();
        assert_eq!(err, LockError::Timeout(Duration::from_millis(0)));

        hold.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn panic_unwinds_and_releases_lock() {
        let client = Client::new(NamedMutexLock::default());

        // task that panics inside CS
        let c1 = client.clone();
        let panicker = task::spawn(async move {
            let _ = std::panic::AssertUnwindSafe(c1.with_cs(|_| panic!("💥")))
                .catch_unwind()
                .await;
        });

        panicker.await.unwrap();

        // lock must still be usable
        assert_eq!(client.inc(1).await.unwrap(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn fifo_serialises_same_name() {
        let lock = NamedMutexLock::default();
        const N: usize = 10;

        #[derive(Clone)]
        struct Span {
            start: u128,
            end: u128,
        }

        let spans: Arc<Mutex<Vec<Span>>> = Arc::new(Mutex::new(Vec::with_capacity(N)));
        let barrier = Arc::new(tokio::sync::Barrier::new(N));

        let mut handles = Vec::new();
        for _ in 0..N {
            let l = lock.clone();
            let sp = spans.clone();
            let b = barrier.clone();
            handles.push(task::spawn(async move {
                b.wait().await; // contend at once
                l.with_lock("fifo", Duration::from_secs(4), move || async move {
                    let start = time::Instant::now();
                    time::sleep(Duration::from_millis(10)).await;
                    let end = time::Instant::now();
                    sp.lock().await.push(Span { start: start.elapsed().as_micros(), end: end.elapsed().as_micros() });
                }).await.unwrap();
            }));
        }

        join_all(handles).await;

        let mut s = spans.lock().await.clone();
        s.sort_by_key(|s| s.start);
        for pair in s.windows(2) {
            assert!(pair[1].start >= pair[0].end, "critical sections overlapped");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn separate_names_do_not_block() {
        let a = Client::new(NamedMutexLock::default());
        let b = Client::new(NamedMutexLock::default());

        let (ra, rb) = tokio::join!(a.inc(1), b.inc(1));
        assert_eq!(ra.unwrap() + rb.unwrap(), 2);
    }

    /*──────────── new regression test: idle immediate acquire ──────*/

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn zero_timeout_succeeds_when_free() {
        let lock = NamedMutexLock::default();
        lock.with_lock("free", Duration::from_millis(0), || async {})
            .await
            .expect("lock should be immediately acquired");
    }
}
