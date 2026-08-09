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

    // Never release the newest snapshot (mirrors the bash script's
    // refusal to delete the last one — it is likely the one just taken,
    // and the only remaining source for the current farm links).
    let newest = snapshots.pop();
    if snapshots.is_empty() {
        anyhow::bail!(
            "low space for new snapshots, but only one snapshot ({}) exists — \
             use a larger storage medium or reduce CAM_SIZE",
            newest.as_deref().unwrap_or("?")
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
