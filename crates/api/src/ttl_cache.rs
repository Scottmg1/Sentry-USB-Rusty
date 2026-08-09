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

pub struct StaleWhileRevalidate<K, T> {
    ttl: Duration,
    /// `(key, stored_at, value)`. A key mismatch is treated as a miss so
    /// one slot can serve a parameterised endpoint (e.g. `?days=`).
    slot: Mutex<Option<(K, Instant, T)>>,
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
            slot: Mutex::new(None),
            refreshing: tokio::sync::Mutex::const_new(()),
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
    pub async fn get<F>(&'static self, key: K, load: F) -> T
    where
        F: FnOnce() -> T + Send + 'static,
    {
        if let Some(v) = self.fresh(&key) {
            return v;
        }

        if let Some(stale) = self.stale(&key) {
            // try_lock: if a refresh is already in flight, skip starting
            // another and just serve what we have.
            if let Ok(guard) = self.refreshing.try_lock() {
                tokio::spawn(async move {
                    let _guard = guard;
                    if let Ok(v) = tokio::task::spawn_blocking(load).await {
                        *self.slot.lock().unwrap() = Some((key, Instant::now(), v));
                    }
                });
            }
            return stale;
        }

        // Cold: nothing cached for this key, so this caller has to load.
        // Serialized so a burst of first-hits makes one query, not N.
        let _guard = self.refreshing.lock().await;
        if let Some(v) = self.fresh(&key) {
            return v;
        }
        let value = tokio::task::spawn_blocking(load)
            .await
            .expect("ttl_cache loader task failed");
        *self.slot.lock().unwrap() = Some((key, Instant::now(), value.clone()));
        value
    }

    /// Invalidate whatever is stored, so the next `get` reloads.
    pub fn clear(&self) {
        *self.slot.lock().unwrap() = None;
    }

    fn fresh(&self, key: &K) -> Option<T> {
        let slot = self.slot.lock().unwrap();
        slot.as_ref()
            .filter(|(k, at, _)| k == key && at.elapsed() < self.ttl)
            .map(|(_, _, v)| v.clone())
    }

    fn stale(&self, key: &K) -> Option<T> {
        let slot = self.slot.lock().unwrap();
        slot.as_ref()
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

        assert_eq!(C.get((), || { LOADS.fetch_add(1, Ordering::SeqCst); 7 }).await, 7);
        assert_eq!(C.get((), || { LOADS.fetch_add(1, Ordering::SeqCst); 9 }).await, 7);
        assert_eq!(LOADS.load(Ordering::SeqCst), 1, "second call must not reload");
    }

    /// The property this type exists for: once warm, an expired entry
    /// never makes the caller wait on a slow read.
    #[tokio::test]
    async fn expired_entry_returns_stale_immediately() {
        static C: StaleWhileRevalidate<(), u32> =
            StaleWhileRevalidate::new(Duration::from_millis(10));

        assert_eq!(C.get((), || 1).await, 1);
        tokio::time::sleep(Duration::from_millis(30)).await;

        let started = Instant::now();
        let v = C.get((), || {
            std::thread::sleep(Duration::from_millis(600));
            2
        })
        .await;
        assert_eq!(v, 1, "must serve the previous value, not the slow reload");
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "must not wait on the refresh, took {:?}",
            started.elapsed()
        );

        // The background refresh eventually lands.
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(C.get((), || 3).await, 2);
    }

    #[tokio::test]
    async fn different_key_is_a_miss() {
        static C: StaleWhileRevalidate<u32, u32> =
            StaleWhileRevalidate::new(Duration::from_secs(60));

        assert_eq!(C.get(1, || 10).await, 10);
        assert_eq!(C.get(2, || 20).await, 20, "a new key must reload");
        assert_eq!(C.get(2, || 99).await, 20);
    }
}
