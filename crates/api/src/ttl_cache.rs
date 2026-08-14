//! Stale-while-revalidate cache for poll-driven read endpoints.
//!
//! Archive I/O can make normally fast database reads take seconds. Two
//! behaviours keep polling responsive:
//!
//! * TTL avoids queries faster than the underlying sampler changes.
//! * Single-flight allows one refresh while other requests receive stale data.
//!
//! Cache raw rows, not rendered relative times, so handlers recompute age.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cache contents and generation under one mutex for atomic publication.
struct State<K, T> {
    /// Loads from an earlier generation are discarded after `clear()`.
    generation: u64,
    /// `(key, stored_at, value)`; a key mismatch is a cache miss.
    slot: Option<(K, Instant, T)>,
}

pub struct StaleWhileRevalidate<K, T> {
    ttl: Duration,
    state: Mutex<State<K, T>>,
    /// Held by whichever request is currently refreshing.
    refreshing: tokio::sync::Mutex<()>,
}

impl<K, T> StaleWhileRevalidate<K, T>
where
    K: PartialEq + Clone + Send + 'static,
    T: Clone + Send + 'static,
{
    pub const fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            state: Mutex::new(State { generation: 0, slot: None }),
            refreshing: tokio::sync::Mutex::const_new(()),
        }
    }

    /// Store only if no invalidation occurred after this load started.
    fn store_if_current(&self, generation: u64, key: K, value: T) {
        let mut st = self.state.lock().unwrap();
        if st.generation == generation {
            st.slot = Some((key, Instant::now(), value));
        }
    }

    /// Cached value for `key`.
    ///
    /// Warm callers receive stale data immediately while refresh runs in the
    /// background; only a cold cache waits for a load.
    ///
    /// `None` from `load` means failure and never replaces a cached value.
    /// Represent a successful empty query inside `T`.
    pub async fn get<F>(&'static self, key: K, load: F) -> Option<T>
    where
        F: FnOnce() -> Option<T> + Send + 'static,
    {
        if let Some(v) = self.fresh(&key) {
            return Some(v);
        }

        if let Some(stale) = self.stale(&key) {
            // Start a refresh only if none is already running.
            if let Ok(guard) = self.refreshing.try_lock() {
                let generation = self.state.lock().unwrap().generation;
                tokio::spawn(async move {
                    let _guard = guard;
                    if let Ok(Some(v)) = tokio::task::spawn_blocking(load).await {
                        self.store_if_current(generation, key, v);
                    }
                });
            }
            return Some(stale);
        }

        // Serialize cold misses into one load.
        let _guard = self.refreshing.lock().await;
        if let Some(v) = self.fresh(&key) {
            return Some(v);
        }
        // An invalidation during the load must not be undone.
        let generation = self.state.lock().unwrap().generation;
        let value = tokio::task::spawn_blocking(load).await.ok().flatten()?;
        self.store_if_current(generation, key, value.clone());
        // Only cache publication is generation-gated.
        Some(value)
    }

    /// Invalidate whatever is stored, so the next `get` reloads. Also
    /// invalidates any load already in flight (see `store_if_current`).
    pub fn clear(&self) {
        let mut st = self.state.lock().unwrap();
        st.generation = st.generation.wrapping_add(1);
        st.slot = None;
    }

    fn fresh(&self, key: &K) -> Option<T> {
        let st = self.state.lock().unwrap();
        st.slot
            .as_ref()
            .filter(|(k, at, _)| k == key && at.elapsed() < self.ttl)
            .map(|(_, _, v)| v.clone())
    }

    fn stale(&self, key: &K) -> Option<T> {
        let st = self.state.lock().unwrap();
        st.slot
            .as_ref()
            .filter(|(k, _, _)| k == key)
            .map(|(_, _, v)| v.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LOADS: AtomicUsize = AtomicUsize::new(0);

    #[tokio::test]
    async fn serves_cached_value_without_reloading() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_secs(60));
        LOADS.store(0, Ordering::SeqCst);

        assert_eq!(C.get((), || { LOADS.fetch_add(1, Ordering::SeqCst); Some(7) }).await, Some(7));
        assert_eq!(C.get((), || { LOADS.fetch_add(1, Ordering::SeqCst); Some(9) }).await, Some(7));
        assert_eq!(LOADS.load(Ordering::SeqCst), 1, "second call must not reload");
    }

    #[tokio::test]
    async fn expired_entry_returns_stale_immediately() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_millis(10));

        assert_eq!(C.get((), || Some(1)).await, Some(1));
        tokio::time::sleep(Duration::from_millis(30)).await;

        let started = Instant::now();
        let v = C.get((), || {
            std::thread::sleep(Duration::from_millis(600));
            Some(2)
        })
        .await;
        assert_eq!(v, Some(1), "must serve the previous value, not the slow reload");
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "must not wait on the refresh, took {:?}",
            started.elapsed()
        );

        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(C.get((), || Some(3)).await, Some(2));
    }

    #[tokio::test]
    async fn failed_refresh_retains_stale() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_millis(10));

        assert_eq!(C.get((), || Some(5)).await, Some(5));
        tokio::time::sleep(Duration::from_millis(30)).await;

        assert_eq!(C.get((), || None).await, Some(5));
        tokio::time::sleep(Duration::from_millis(200)).await;

        assert_eq!(C.get((), || None).await, Some(5));
    }

    #[tokio::test]
    async fn cold_load_failure_reports_none() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_secs(60));
        assert_eq!(C.get((), || None).await, None);
    }

    #[tokio::test]
    async fn different_key_is_a_miss() {
        static C: StaleWhileRevalidate<u32, u32> =
            StaleWhileRevalidate::new(Duration::from_secs(60));

        assert_eq!(C.get(1, || Some(10)).await, Some(10));
        assert_eq!(C.get(2, || Some(20)).await, Some(20), "a new key must reload");
        assert_eq!(C.get(2, || Some(99)).await, Some(20));
    }
}

#[cfg(test)]
mod clear_race_tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A refresh started before invalidation cannot publish afterward.
    #[tokio::test]
    async fn stale_refresh_cannot_overwrite_a_clear() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_millis(1));
        static GATE: AtomicU32 = AtomicU32::new(0);

        assert_eq!(C.get((), || Some(1)).await, Some(1));
        tokio::time::sleep(Duration::from_millis(5)).await;

        let stale = C
            .get((), || {
                while GATE.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Some(1)
            })
            .await;
        assert_eq!(stale, Some(1), "stale value is served immediately");

        C.clear();
        GATE.store(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            C.get((), || Some(2)).await,
            Some(2),
            "post-clear load must win; a pre-clear refresh must not reinstate old data",
        );
    }

    #[tokio::test]
    async fn clear_without_inflight_load_reloads() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_secs(60));
        assert_eq!(C.get((), || Some(10)).await, Some(10));
        assert_eq!(C.get((), || Some(99)).await, Some(10), "served from cache");
        C.clear();
        assert_eq!(C.get((), || Some(11)).await, Some(11), "reloaded after clear");
    }
}
