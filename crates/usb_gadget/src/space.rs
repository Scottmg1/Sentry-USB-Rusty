//! Free space management for the backing filesystem.
//!
//! Monitors disk usage and releases old snapshots when space runs low.

use anyhow::Result;
use tracing::{info, warn};

const BACKINGFILES: &str = "/backingfiles";

/// Absolute headroom term of the reserve: roughly one recording cycle's
/// worth of writes plus the next snapshot's copy-on-write growth.
const FIXED_RESERVE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

/// Capacity-relative term divisor — `total/33` ≈ 3.03%, matching
/// archiveloop's integer arithmetic exactly.
const RESERVE_PCT_DIVISOR: u64 = 33;

/// Free-space target in BYTES: `10 GiB + total/33`.
///
/// This is archiveloop's own formula (`freespacemanager`, which computes
/// the reserve and passes it to manage_free_space.sh). Legacy full-bash
/// installs have always honored it; the Rust path used to ignore the
/// forwarded value and substitute a flat 5% of total, so the same
/// archiveloop call meant different things depending on install vintage
/// — much stricter than intended on multi-TB drives (5% of 7.4 TB = 372
/// GB withheld vs ~236 GB) and looser on small ones (5% of 226 GB = 11.3
/// GB vs ~16.8 GB).
///
/// Additive, not `max()`/`min()`: the two terms measure different needs —
/// absolute write headroom for the next cycle, plus a cushion that scales
/// with filesystem size.
fn default_reserve_bytes(total: u64) -> u64 {
    FIXED_RESERVE_BYTES.saturating_add(total / RESERVE_PCT_DIVISOR)
}

/// Numeric slot of a `snap-NNNNNN` name, if it has one.
fn slot_num(name: &str) -> Option<u32> {
    name.strip_prefix("snap-")
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
}

/// Given snapshots in ascending AGE order (oldest `snap.bin` mtime
/// first), return those eligible for release — everything except the
/// two we must never drop:
///
/// * the newest by mtime (the one most likely just taken, and the
///   source the current farm links point at), and
/// * the HIGHEST SLOT NUMBER.
///
/// The second guard exists because mtime is wall-clock and the Pi
/// usually has no battery-backed RTC: archiveloop starts
/// `freespacemanager` before `timesyncloop`, so a boot can evict while
/// the clock still holds a fake-hwclock time from the past. A snapshot
/// created in that window sorts as the OLDEST and would be released
/// first — deleting the footage just captured. Slot allocation is
/// monotonic (max+1), so the highest number is the most recent creation
/// no matter what the clock claimed. At most two snapshots are withheld;
/// in the healthy case they are the same one.
fn releasable(mut in_age_order: Vec<String>) -> Vec<String> {
    if in_age_order.len() < 2 {
        return Vec::new();
    }
    // Highest slot across the WHOLE set, computed before popping: when
    // the clock is healthy this is the same snapshot as the mtime-newest
    // and only one is withheld. Computing it after the pop would shield
    // a second, arbitrarily old snapshot (e.g. a stale reflash leftover
    // carrying the highest number) for no reason.
    let highest = in_age_order
        .iter()
        .filter_map(|n| slot_num(n).map(|s| (s, n.clone())))
        .max_by_key(|(s, _)| *s)
        .map(|(_, n)| n);
    in_age_order.pop(); // newest by mtime
    if let Some(highest) = highest {
        in_age_order.retain(|n| *n != highest);
    }
    in_age_order
}

/// Release old snapshots until `free >= reserve`.
///
/// `reserve_bytes` is the caller's target (archiveloop forwards its
/// computed reserve through the `manage_free_space.sh` wrapper); `None`
/// falls back to [`default_reserve_bytes`] for the same filesystem, so
/// every entry point enforces one policy.
pub async fn manage_free_space(reserve_bytes: Option<u64>) -> Result<()> {
    // Held across the whole release loop, matching manage_free_space.sh; the
    // releases below therefore use the already-locked entry point.
    let _lock = super::snapshot::lock_snapshots_dir()?;

    let (total, free) = get_space(BACKINGFILES)?;

    let (reserve, source) = match reserve_bytes {
        Some(r) => (r, "arg"),
        None => (default_reserve_bytes(total), "default"),
    };
    // A target at or above capacity can never be satisfied; releasing
    // every snapshot but the newest would not get there either, so
    // refuse before deleting anything rather than emptying the store.
    if reserve >= total {
        anyhow::bail!(
            "reserve {} bytes (source={}) >= filesystem capacity {} — refusing to release snapshots",
            reserve,
            source,
            total
        );
    }

    info!(
        "Disk space: {} free / {} total bytes; reserve={} (source={}, formula=10GiB+total/33)",
        free, total, reserve, source
    );

    // `>=`, deliberately one byte off from the bash script's `-gt`.
    // At free == reserve the policy is satisfied, and archiveloop only
    // invokes the manager when free < reserve — so bash's strict `>`
    // (effective target reserve+1) is a quirk its production caller
    // never exercises, and honoring it would evict a whole snapshot at
    // the exact boundary for nothing.
    if free >= reserve {
        return Ok(());
    }

    info!("Free space below reserve ({} bytes), releasing old snapshots...", reserve);

    // ACTUAL age order (snap.bin mtime), not name order — slot numbers
    // are not time-monotonic in the field (a reflash can leave a stale
    // high-numbered snapshot above a restarted sequence), and releasing
    // by name deleted newer footage while sparing the truly oldest.
    let mut snapshots = super::snapshot::list_snapshots_by_age();
    if snapshots.is_empty() {
        // Same outcome as the bash script's "no snapshots exist" branch,
        // which exits non-zero: the target is unmet and nothing here can
        // fix it. archiveloop treats that as `|| sleep 30`.
        anyhow::bail!(
            "low space for new snapshots, but no snapshots exist — \
             use a larger storage medium or reduce CAM_SIZE"
        );
    }

    // Withhold the newest-by-mtime AND the highest slot number — see
    // `releasable`: a clock rollback (no RTC; eviction starts before
    // timesync) can make the snapshot just taken look oldest.
    let total_snaps = snapshots.len();
    let snapshots = releasable(snapshots);
    if snapshots.is_empty() {
        anyhow::bail!(
            "low space for new snapshots, but the only {} snapshot(s) present are the \
             protected newest/highest — use a larger storage medium or reduce CAM_SIZE",
            total_snaps
        );
    }

    // Release oldest snapshots first until we're above the threshold
    let mut recovered = false;
    for snap in &snapshots {
        if let Err(e) = super::snapshot::release_snapshot_locked(snap).await {
            warn!("Failed to release {}: {}", snap, e);
            continue;
        }

        let (_, new_free) = get_space(BACKINGFILES)?;
        info!("After releasing {}: {} bytes free (reserve {})", snap, new_free, reserve);

        if new_free >= reserve {
            recovered = true;
            break;
        }
    }
    if !recovered {
        // Bash exits non-zero when it cannot reach the target; match it
        // so archiveloop's `|| sleep 30` backs off instead of spinning.
        anyhow::bail!(
            "free space still below reserve ({} bytes) with only the newest snapshot retained",
            reserve
        );
    }

    Ok(())
}

/// Get total and free bytes for a filesystem.
fn get_space(path: &str) -> Result<(u64, u64)> {
    let output = std::process::Command::new("stat")
        .args(["--file-system", "--format=%b %S %f", path])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("stat failed for {}", path);
    }

    // Fail closed: malformed output used to collapse to (0, 0), and a
    // zero total made the caller return success — free-space management
    // silently doing nothing forever. An unreadable filesystem is an
    // error, not "nothing to do".
    let s = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = s.trim().split_whitespace().collect();
    if parts.len() < 3 {
        anyhow::bail!("unexpected stat output for {}: {:?}", path, s.trim());
    }
    let parse = |i: usize, what: &str| -> Result<u64> {
        parts[i]
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("bad {} in stat output for {}: {}", what, path, e))
    };
    let blocks = parse(0, "block count")?;
    let block_size = parse(1, "block size")?;
    let free_blocks = parse(2, "free block count")?;
    if blocks == 0 || block_size == 0 {
        anyhow::bail!("stat reported zero capacity for {}", path);
    }
    Ok((
        blocks.saturating_mul(block_size),
        free_blocks.saturating_mul(block_size),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The Pi has no battery-backed clock on most boards (the RTC is
    /// opt-in and Pi 5 only), and archiveloop starts `freespacemanager`
    /// BEFORE `timesyncloop`, so a boot can run eviction while the clock
    /// still holds whatever fake-hwclock restored — a time in the past.
    /// A snapshot created in that window carries a past mtime and sorts
    /// as the OLDEST, so protecting only the mtime-newest would release
    /// the snapshot that was just taken. Slot numbers are allocated
    /// monotonically (max+1), so the highest number is the most recent
    /// creation regardless of what the clock said.
    #[test]
    fn releasable_protects_highest_slot_when_clock_rolled_back() {
        // Age order says the just-created snap-000042 is "oldest"
        // because the clock was rolled back when it was written.
        let age_order = vec![
            "snap-000042".to_string(), // just created, past-dated mtime
            "snap-000010".to_string(),
            "snap-000011".to_string(),
            "snap-000012".to_string(), // newest by mtime
        ];
        let out = releasable(age_order);
        assert!(
            !out.contains(&"snap-000042".to_string()),
            "highest slot number must never be released: {:?}",
            out
        );
        assert!(
            !out.contains(&"snap-000012".to_string()),
            "newest by mtime must never be released: {:?}",
            out
        );
        assert_eq!(out, vec!["snap-000010".to_string(), "snap-000011".to_string()]);
    }

    /// Normal (healthy clock) case: highest slot IS the mtime-newest, so
    /// exactly one snapshot is withheld and everything older is fair game
    /// in true age order — including a stale high-mtime-old leftover.
    #[test]
    fn releasable_normal_clock_withholds_only_the_newest() {
        let age_order = vec![
            "snap-000414".to_string(), // stale reflash leftover, truly oldest
            "snap-000010".to_string(),
            "snap-000421".to_string(), // newest by mtime AND highest slot
        ];
        assert_eq!(
            releasable(age_order),
            vec!["snap-000414".to_string(), "snap-000010".to_string()],
        );
    }

    #[test]
    fn releasable_empty_when_one_or_zero_snapshots() {
        assert!(releasable(vec![]).is_empty());
        assert!(releasable(vec!["snap-000001".to_string()]).is_empty());
    }

    /// The unified reserve must reproduce archiveloop's own arithmetic
    /// (`10G` + `total/33`) exactly, including its integer truncation —
    /// the bash and Rust paths otherwise disagree about when to evict.
    #[test]
    fn reserve_matches_archiveloop_formula() {
        // Field device sizes from real dashboards.
        let small = 226 * GIB;
        let large = 7448 * GIB;
        assert_eq!(default_reserve_bytes(small), 10 * GIB + small / 33);
        assert_eq!(default_reserve_bytes(large), 10 * GIB + large / 33);

        // Sanity on the human-readable magnitudes: ~16.8 GiB and ~235.7 GiB.
        assert_eq!(default_reserve_bytes(small) / GIB, 16);
        assert_eq!(default_reserve_bytes(large) / GIB, 235);

        // Zero-capacity filesystem still yields the fixed term (callers
        // bail on total == 0 before this matters, but no underflow).
        assert_eq!(default_reserve_bytes(0), 10 * GIB);
    }

    /// The old flat 5% withheld far more than intended on multi-TB
    /// drives and less than intended on small ones. Pin the direction of
    /// the change so a future "simplification" back to a flat percentage
    /// is caught.
    #[test]
    fn reserve_is_looser_than_5pct_on_large_and_stricter_on_small() {
        let five_pct = |t: u64| t / 20;
        let small = 226 * GIB;
        let large = 7448 * GIB;
        assert!(
            default_reserve_bytes(small) > five_pct(small),
            "small drives must reserve MORE than the old flat 5%",
        );
        assert!(
            default_reserve_bytes(large) < five_pct(large),
            "large drives must reserve LESS than the old flat 5% (keep more history)",
        );
    }

    /// No overflow panic on an absurd capacity.
    #[test]
    fn reserve_saturates_on_absurd_capacity() {
        assert_eq!(default_reserve_bytes(u64::MAX), 10 * GIB + u64::MAX / 33);
    }
}
