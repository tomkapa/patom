//! Generic bounded-TTL cache.
//!
//! Cheap-clone by construction — the cache holds an internal `Arc`, so
//! cloning the handle costs one atomic increment and every clone shares
//! the same underlying state. Callers no longer need to wrap it in an
//! external `Arc<...>`.
//!
//! Hot path: one mutex round-trip + one HashMap lookup. On miss the lock
//! is released, the supplied loader runs without contention, then the
//! lock is re-acquired to insert. Cache size is bounded; eviction
//! prefers the oldest expired entry, falling back to the absolute
//! oldest. Two concurrent loaders for the same key may both run — the
//! second insert wins; this is acceptable here because every consumer
//! either serialises per-key already (the agent worker pool holds one
//! lease per session, and the agent registry has a single owner) or
//! treats the redundant compute as free.
//!
//! Hand-rolled rather than pulling in `moka` / `lru` (CLAUDE.md §8 — zero-dep
//! bias). The cache is the only one in the binary; reaching for a crate
//! would be premature.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock::SharedClock;

#[derive(Debug, Clone)]
struct Entry<V> {
    value: V,
    fetched_at: Instant,
}

/// Internal state held behind the [`BoundedTtlCache`]'s `Arc`. The
/// outer struct exists so we can derive `Clone` cheaply and so the
/// label / cap / ttl fields stay accessible without taking the lock.
#[derive(Debug)]
struct Inner<K, V> {
    state: Mutex<HashMap<K, Entry<V>>>,
    /// Monotonic counter bumped by every `clear` / `invalidate` so an
    /// in-flight `get_or_load` whose loader was racing the invalidation
    /// cannot resurrect a stale value. The loader snapshots this before
    /// awaiting; on completion the insert is skipped if the snapshot is
    /// behind. CLAUDE.md §6: this is the "fresh-after-invalidate"
    /// invariant in code form.
    epoch: AtomicU64,
    cap: usize,
    ttl: Duration,
    clock: SharedClock,
    label: &'static str,
}

/// Bounded TTL cache.
///
/// Cheap-clone — the handle itself is an `Arc<...>` so cloning shares
/// the underlying state. The value type must be `Clone`-cheap
/// (typically an `Arc<T>` or a reference-counted newtype) since
/// `get_or_load` returns owned values.
pub struct BoundedTtlCache<K, V> {
    inner: Arc<Inner<K, V>>,
}

impl<K, V> Clone for BoundedTtlCache<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<K, V> BoundedTtlCache<K, V>
where
    K: Eq + Hash + Copy,
    V: Clone,
{
    /// Build a cache with `cap` entries and `ttl` lifetime per entry.
    /// `label` rides on assertion messages so a poisoned-mutex panic
    /// names the offending cache.
    #[must_use]
    pub fn new(cap: usize, ttl: Duration, clock: SharedClock, label: &'static str) -> Self {
        // §6: zero-cap or zero-TTL would silently disable the cache.
        assert!(cap > 0, "invariant: {label} cap must be > 0");
        assert!(!ttl.is_zero(), "invariant: {label} ttl must be > 0");
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(HashMap::new()),
                epoch: AtomicU64::new(0),
                cap,
                ttl,
                clock,
                label,
            }),
        }
    }

    /// Return the cached value for `key`, calling `load` to produce one
    /// on miss or expiry. The lock is released before `load` runs so a
    /// slow loader does not block other workers.
    ///
    /// The loader-side insert is fenced against a concurrent `clear` or
    /// `invalidate` via the epoch counter: if the cache was invalidated
    /// while `load` was awaiting, the freshly loaded value is dropped
    /// rather than resurrecting a stale entry past the invalidation
    /// boundary.
    pub async fn get_or_load<F, Fut, E>(&self, key: K, load: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>>,
    {
        let now = self.inner.clock.now();
        if let Some(value) = self.lookup_fresh(key, now) {
            return Ok(value);
        }
        let started_at_epoch = self.inner.epoch.load(Ordering::Acquire);
        let value = load().await?;
        let now_after = self.inner.clock.now();
        self.insert_if_epoch(key, value.clone(), now_after, started_at_epoch);
        Ok(value)
    }

    fn lookup_fresh(&self, key: K, now: Instant) -> Option<V> {
        let cache = self.lock();
        let entry = cache.get(&key)?;
        if now.saturating_duration_since(entry.fetched_at) >= self.inner.ttl {
            return None;
        }
        Some(entry.value.clone())
    }

    /// Insert `key → value` only if the epoch hasn't moved since the
    /// loader started. Skipped silently when a concurrent
    /// `clear` / `invalidate` raced ahead — the next lookup will miss
    /// and trigger a fresh load against the post-invalidation source of
    /// truth.
    fn insert_if_epoch(&self, key: K, value: V, now: Instant, started_at_epoch: u64) {
        let mut cache = self.lock();
        if self.inner.epoch.load(Ordering::Acquire) != started_at_epoch {
            return;
        }
        if cache.len() >= self.inner.cap && !cache.contains_key(&key) {
            evict_one(&mut cache, now, self.inner.ttl);
            assert!(
                cache.len() < self.inner.cap,
                "invariant: {} eviction made room",
                self.inner.label
            );
        }
        cache.insert(
            key,
            Entry {
                value,
                fetched_at: now,
            },
        );
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<K, Entry<V>>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|_| panic!("invariant: {} mutex never poisoned", self.inner.label))
    }

    /// Drop every entry. Used when an external event invalidates the
    /// entire keyspace (e.g. an org-wide config change that affects every
    /// agent that org owns). Cheaper than scanning to find the matching
    /// keys and acceptable for the rare invalidation paths.
    ///
    /// Bumps the epoch so any in-flight `get_or_load` that was racing
    /// the invalidation drops its result on insert.
    pub fn clear(&self) {
        // AcqRel so the bump synchronises with the loader's Acquire load
        // before its insert, AND with any earlier writes other threads
        // performed before observing the new epoch.
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        self.lock().clear();
    }

    /// Drop the entry for `key` if present. No-op when absent. Bumps
    /// the epoch for the same race-free invariant as [`Self::clear`].
    pub fn invalidate(&self, key: K) {
        self.inner.epoch.fetch_add(1, Ordering::AcqRel);
        self.lock().remove(&key);
    }

    /// Test/inspection: number of entries currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
}

impl<K, V> fmt::Debug for BoundedTtlCache<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedTtlCache")
            .field("label", &self.inner.label)
            .field("cap", &self.inner.cap)
            .field("ttl", &self.inner.ttl)
            .finish_non_exhaustive()
    }
}

/// Evict the oldest expired entry, falling back to the absolute oldest
/// when nothing has expired. Bounded scan over `cap` entries.
fn evict_one<K, V>(cache: &mut HashMap<K, Entry<V>>, now: Instant, ttl: Duration)
where
    K: Eq + Hash + Copy,
{
    let mut oldest_expired: Option<K> = None;
    let mut oldest_overall: Option<(K, Instant)> = None;
    for (k, entry) in cache.iter() {
        if now.saturating_duration_since(entry.fetched_at) >= ttl {
            oldest_expired = Some(*k);
            break;
        }
        match oldest_overall {
            None => oldest_overall = Some((*k, entry.fetched_at)),
            Some((_, ts)) if entry.fetched_at < ts => {
                oldest_overall = Some((*k, entry.fetched_at));
            }
            _ => {}
        }
    }
    let victim = oldest_expired.or_else(|| oldest_overall.map(|(k, _)| k));
    if let Some(k) = victim {
        cache.remove(&k);
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::clock::TestClock;

    use super::*;

    fn cache(cap: usize, ttl_secs: u64) -> (BoundedTtlCache<u64, u64>, Arc<TestClock>) {
        let clock = Arc::new(TestClock::new());
        let shared: SharedClock = clock.clone();
        let c = BoundedTtlCache::new(cap, Duration::from_secs(ttl_secs), shared, "test");
        (c, clock)
    }

    #[tokio::test]
    async fn get_or_load_runs_loader_once_within_ttl() {
        let (c, _clock) = cache(8, 60);
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let calls = calls.clone();
            let _: u64 = c
                .get_or_load(7u64, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(42u64)
                })
                .await
                .expect("ok");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ttl_expiry_re_runs_loader() {
        let (c, clock) = cache(8, 60);
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let calls = calls.clone();
            let _: u64 = c
                .get_or_load(7u64, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, Infallible>(42u64)
                })
                .await
                .expect("ok");
            clock.advance(Duration::from_secs(61));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn cap_evicts_when_full() {
        let (c, _clock) = cache(2, 60);
        for k in 0..3u64 {
            let _: u64 = c
                .get_or_load(k, || async move { Ok::<_, Infallible>(k) })
                .await
                .expect("ok");
        }
        assert!(c.len() <= 2, "stayed under cap, got {}", c.len());
    }

    #[derive(Debug, PartialEq)]
    struct Boom;

    #[tokio::test]
    async fn loader_error_is_not_cached() {
        let (c, _clock) = cache(2, 60);
        let first: Result<u64, Boom> = c.get_or_load(1u64, || async { Err(Boom) }).await;
        assert!(first.is_err());

        let second: Result<u64, Boom> =
            c.get_or_load(1u64, || async { Ok::<_, Boom>(99u64) }).await;
        assert_eq!(second, Ok(99u64));
    }

    #[tokio::test]
    async fn clones_share_state() {
        let (c, _clock) = cache(8, 60);
        let _: u64 = c
            .get_or_load(1u64, || async { Ok::<_, Infallible>(11u64) })
            .await
            .expect("ok");
        let cloned = c.clone();
        let v: u64 = cloned
            .get_or_load(1u64, || async { Ok::<_, Infallible>(22u64) })
            .await
            .expect("ok");
        // Same key resolved through the clone returns the original value
        // — proves the inner Arc state is shared.
        assert_eq!(v, 11u64);
    }

    // The next two tests pin the epoch-fenced invalidation contract: an
    // in-flight `get_or_load` whose loader is racing a `clear` /
    // `invalidate` must drop its result rather than resurrect the value
    // past the invalidation boundary. The next lookup must re-run the
    // loader against the post-invalidation source of truth.
    //
    // Shape (both tests): start a loader that parks on a oneshot,
    // observe it has started, invalidate while it's parked, release the
    // loader, then prove the cache is empty by checking that a second
    // `get_or_load` runs the loader a second time.

    #[tokio::test]
    async fn clear_drops_inflight_loader_insert() {
        let (c, _clock) = cache(8, 60);
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        let c2 = c.clone();
        let calls2 = calls.clone();
        let task = tokio::spawn(async move {
            c2.get_or_load(42u64, || async move {
                calls2.fetch_add(1, Ordering::SeqCst);
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok::<_, Infallible>(7u64)
            })
            .await
            .expect("ok")
        });

        started_rx.await.expect("loader started");
        c.clear();
        let _ = release_tx.send(());
        task.await.expect("join ok");

        let calls3 = calls.clone();
        let _: u64 = c
            .get_or_load(42u64, || async move {
                calls3.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(9u64)
            })
            .await
            .expect("ok");

        // Loader fired twice (once across the clear, once for the
        // post-clear lookup); the racing fill did not stick.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn invalidate_drops_inflight_loader_insert() {
        let (c, _clock) = cache(8, 60);
        let calls = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        let c2 = c.clone();
        let calls2 = calls.clone();
        let task = tokio::spawn(async move {
            c2.get_or_load(42u64, || async move {
                calls2.fetch_add(1, Ordering::SeqCst);
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok::<_, Infallible>(7u64)
            })
            .await
            .expect("ok")
        });

        started_rx.await.expect("loader started");
        c.invalidate(42u64);
        let _ = release_tx.send(());
        task.await.expect("join ok");

        let calls3 = calls.clone();
        let _: u64 = c
            .get_or_load(42u64, || async move {
                calls3.fetch_add(1, Ordering::SeqCst);
                Ok::<_, Infallible>(9u64)
            })
            .await
            .expect("ok");

        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
