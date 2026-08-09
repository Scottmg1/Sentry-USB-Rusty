//! Stale-while-revalidate cache for poll-driven read endpoints.
//!
//! The dashboard and BLE panel poll several telemetry endpoints every
//! few seconds. Their queries are sub-millisecond on an idle disk but
//! measured 5-10s during an archive run, because the archive streams
//! gigabytes of video through the page cache and evicts the DB's hot
//! pages. Two behaviours matter under that load:
//!
//!   * TTL — a poll cadence faster than the sampler writes is pure
//!     waste; serving the previous answer costs no freshness.
//!   * Single-flight — the read pool has two connections, so two slow
//!     scans serialize every other read behind them (observed: a 2ms
//!     query that waited 5130ms). Only one refresh is ever in flight;
//!     everyone else gets the last value immediately instead of
//!     queueing on a connection.
//!
//! Cache the raw row data, never a rendered response: handlers that
//! derive "seconds ago" from the current clock must recompute those
//! from the cached timestamps, or the age readout freezes.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cache contents plus the generation they belong to, under ONE mutex so
/// a write can atomically check that its data is still wanted.
struct State<K, T> {
    /// Bumped by every `clear()`. A load that started before the bump
    /// carries a stale generation and is discarded on completion.
    generation: u64,
    /// `(key, stored_at, value)`. A key mismatch is treated as a miss so
    /// one slot can serve a parameterised endpoint (e.g. `?days=`).
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

    /// Store `value` only if no `clear()` happened since `generation` was
    /// sampled. Without this check a refresh that began BEFORE a mutation
    /// could finish after it and reinstate pre-mutation data as fresh —
    /// e.g. edit a charge's tags, then watch the old tags reappear for a
    /// full TTL because an in-flight poll won the race.
    fn store_if_current(&self, generation: u64, key: K, value: T) {
        let mut st = self.state.lock().unwrap();
        if st.generation == generation {
            st.slot = Some((key, Instant::now(), value));
        }
    }

    /// Cached value for `key`.
    ///
    /// Once a value exists, this NEVER waits on the database: an expired
    /// entry is returned immediately and the refresh runs in the
    /// background. Blocking the request that happens to notice the
    /// expiry would hand a single slow read (7s+ during an archive) to
    /// whichever poll drew the short straw — the symptom this exists to
    /// remove. Only the very first call, with nothing cached, waits.
    ///
    /// `load` returns `None` to mean **the load failed**, which never
    /// replaces a good cached value — a transient DB error during an
    /// archive must not blank a working dashboard. Encode "queried fine,
    /// no rows" inside `T` instead. `None` comes back only when the
    /// cache is cold and the load failed.
    pub async fn get<F>(&'static self, key: K, load: F) -> Option<T>
    where
        F: FnOnce() -> Option<T> + Send + 'static,
    {
        if let Some(v) = self.fresh(&key) {
            return Some(v);
        }

        if let Some(stale) = self.stale(&key) {
            // try_lock: if a refresh is already in flight, skip starting
            // another and just serve what we have.
            if let Ok(guard) = self.refreshing.try_lock() {
                // Sample the generation BEFORE the load starts.
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

        // Cold: nothing cached for this key, so this caller has to load.
        // Serialized so a burst of first-hits makes one query, not N.
        let _guard = self.refreshing.lock().await;
        if let Some(v) = self.fresh(&key) {
            return Some(v);
        }
        // Same generation discipline on the cold path: a clear() during
        // this load must not be undone by it.
        let generation = self.state.lock().unwrap().generation;
        let value = tokio::task::spawn_blocking(load).await.ok().flatten()?;
        self.store_if_current(generation, key, value.clone());
        // Return the freshly loaded value either way — the caller asked
        // for data now; only the CACHE write is generation-gated.
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

    /// The property this type exists for: once warm, an expired entry
    /// never makes the caller wait on a slow read.
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

        // The background refresh eventually lands.
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(C.get((), || Some(3)).await, Some(2));
    }

    /// A failed refresh must never blank a working value — during an
    /// archive the DB read is exactly what's flaky.
    #[tokio::test]
    async fn failed_refresh_retains_stale() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_millis(10));

        assert_eq!(C.get((), || Some(5)).await, Some(5));
        tokio::time::sleep(Duration::from_millis(30)).await;

        // Expired: serves stale and kicks off a refresh that fails.
        assert_eq!(C.get((), || None).await, Some(5));
        tokio::time::sleep(Duration::from_millis(200)).await;

        // The failure must not have replaced the good value.
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

    /// A refresh that STARTED before an invalidation must not install its
    /// (pre-mutation) result afterwards. Deterministic: the loader blocks
    /// on a gate we open only after `clear()` has run, so the stale write
    /// always lands last — the exact interleaving that previously made an
    /// edit appear to revert for a full TTL.
    #[tokio::test]
    async fn stale_refresh_cannot_overwrite_a_clear() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_millis(1));
        static GATE: AtomicU32 = AtomicU32::new(0);

        // Seed the cache with the pre-mutation value.
        assert_eq!(C.get((), || Some(1)).await, Some(1));
        // Let it go stale so the next get() spawns a background refresh.
        tokio::time::sleep(Duration::from_millis(5)).await;

        // This refresh reads "1" (pre-mutation) but is held at the gate.
        let stale = C
            .get((), || {
                while GATE.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Some(1)
            })
            .await;
        assert_eq!(stale, Some(1), "stale value is served immediately");

        // The mutation happens while that load is still in flight.
        C.clear();
        // Now let the in-flight load finish and try to write "1" back.
        GATE.store(1, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(50)).await;

        // The cache must be EMPTY, not repopulated with the stale 1.
        assert_eq!(
            C.get((), || Some(2)).await,
            Some(2),
            "post-clear load must win; a pre-clear refresh must not reinstate old data",
        );
    }

    /// clear() with no load in flight still simply empties the cache.
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
