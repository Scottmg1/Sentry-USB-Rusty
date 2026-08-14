//! Free space management for the backing filesystem.
//!
//! Monitors disk usage and releases old snapshots when space runs low.

use anyhow::Result;
use tracing::{info, warn};

const BACKINGFILES: &str = "/backingfiles";

/// Headroom for one recording cycle and the next snapshot's COW growth.
const FIXED_RESERVE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GiB

/// `total/33` is approximately 3.03%.
const RESERVE_PCT_DIVISOR: u64 = 33;

/// Free-space target in BYTES: `10 GiB + total/33`.
///
/// The terms are additive: fixed write headroom plus a capacity-scaled
/// cushion. This must match archiveloop's integer arithmetic.
fn default_reserve_bytes(total: u64) -> u64 {
    FIXED_RESERVE_BYTES.saturating_add(total / RESERVE_PCT_DIVISOR)
}

/// Numeric slot of a `snap-NNNNNN` name, if it has one.
fn slot_num(name: &str) -> Option<u32> {
    name.strip_prefix("snap-")
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse().ok())
}

/// Return release candidates from an oldest-first list.
///
/// Protect both the mtime-newest snapshot and the highest slot. The latter is
/// monotonic and remains reliable when an unsynchronized Pi clock makes a new
/// snapshot appear oldest. Normally both protections identify the same item.
fn releasable(mut in_age_order: Vec<String>) -> Vec<String> {
    if in_age_order.len() < 2 {
        return Vec::new();
    }
    // Compute the highest slot before removing the mtime-newest item.
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
    // Serialize the entire release loop.
    let _lock = super::snapshot::lock_snapshots_dir()?;

    let (total, free) = get_space(BACKINGFILES)?;

    let (reserve, source) = match reserve_bytes {
        Some(r) => (r, "arg"),
        None => (default_reserve_bytes(total), "default"),
    };
    // Reject impossible targets before deleting anything.
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

    // Equality satisfies the target; do not evict a snapshot at the boundary.
    if free >= reserve {
        return Ok(());
    }

    info!("Free space below reserve ({} bytes), releasing old snapshots...", reserve);

    // Slot numbering can restart after a reflash, so release by mtime age.
    let mut snapshots = super::snapshot::list_snapshots_by_age();
    if snapshots.is_empty() {
        anyhow::bail!(
            "low space for new snapshots, but no snapshots exist — \
             use a larger storage medium or reduce CAM_SIZE"
        );
    }

    // A clock rollback can make the highest-slot snapshot appear oldest.
    let total_snaps = snapshots.len();
    let snapshots = releasable(snapshots);
    if snapshots.is_empty() {
        anyhow::bail!(
            "low space for new snapshots, but the only {} snapshot(s) present are the \
             protected newest/highest — use a larger storage medium or reduce CAM_SIZE",
            total_snaps
        );
    }

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

    // Malformed filesystem data is an error, not an empty filesystem.
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

    /// The monotonic slot protects a new snapshot from a rolled-back clock.
    #[test]
    fn releasable_protects_highest_slot_when_clock_rolled_back() {
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

    /// A healthy clock normally makes the highest slot mtime-newest.
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

    /// The reserve must match archiveloop, including integer truncation.
    #[test]
    fn reserve_matches_archiveloop_formula() {
        let small = 226 * GIB;
        let large = 7448 * GIB;
        assert_eq!(default_reserve_bytes(small), 10 * GIB + small / 33);
        assert_eq!(default_reserve_bytes(large), 10 * GIB + large / 33);

        assert_eq!(default_reserve_bytes(small) / GIB, 16);
        assert_eq!(default_reserve_bytes(large) / GIB, 235);

        assert_eq!(default_reserve_bytes(0), 10 * GIB);
    }

    /// The additive reserve is stricter on small media and looser on large media.
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

    #[test]
    fn reserve_saturates_on_absurd_capacity() {
        assert_eq!(default_reserve_bytes(u64::MAX), 10 * GIB + u64::MAX / 33);
    }
}
