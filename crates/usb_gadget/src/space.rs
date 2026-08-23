//! Free space management for the backing filesystem.
//!
//! Monitors disk usage and releases old snapshots when space runs low.

use anyhow::Result;
use tracing::{info, warn};

const BACKINGFILES: &str = "/backingfiles";
const MUTABLE: &str = "/mutable";

/// /mutable inode-headroom target: `min(max(20000, table/20), table/4)`.
///
/// Every retained clip costs one symlink inode in /mutable/TeslaCam
/// (the snapshot farm index), and the stock partition's table (~121k at
/// mkfs.ext4 defaults) fills long before a multi-TB /backingfiles feels
/// block-space pressure. The 2026-08-19 field failure ran a full day at
/// 100% inode usage with 71% of the bytes free: every ln and state-file
/// write ENOSPC'd, so new clips were never indexed, mapped, or archived
/// while notifications (network-only) kept working. At the observed
/// ~2.9k links/day the 20k floor is about a week of recovery headroom.
///
/// The CAP is what keeps the target reachable, and it is not optional.
/// Single-disk installs give /mutable a fixed 300 MiB partition whose
/// inode table is sized from the data area (`backingfiles_sectors /
/// 20000`), so a 128 GB card has ~11.8k inodes TOTAL — less than the
/// floor. Shipped v3.20.0-v3.20.8 demanded 20k free from that table,
/// which no amount of eviction can reach, so cleanup deleted every
/// releasable snapshot and then failed on a 60-second loop forever.
/// Two users lost 23 and 45 snapshots of real footage that way.
/// Capping at a quarter of the table keeps the target satisfiable on
/// every geometry while leaving 120,960 and 472,000 byte-identical to
/// the value that fixed the original incident. Matches
/// manage_free_space.sh, archiveloop, and healthcheck.rs.
const INODE_RESERVE_FLOOR: u64 = 20_000;
const INODE_RESERVE_DIVISOR: u64 = 20;
/// Never demand more than `table / INODE_RESERVE_CAP_DIVISOR` free.
const INODE_RESERVE_CAP_DIVISOR: u64 = 4;

/// How many consecutive snapshot releases may fail to free any /mutable
/// inodes (with the block target already met) before we conclude the
/// inode pressure is not from clip symlinks and stop evicting footage.
const MAX_STALE_INODE_RELEASES: u32 = 3;

/// Written when the stall guard trips; holds the free-inode count at
/// the stall. tmpfs on purpose: /mutable itself may be inode-full and
/// the root filesystem is read-only. Shared with manage_free_space.sh.
/// Without it, a 30-second retrying caller would re-enter and delete
/// three more snapshots per attempt against pressure snapshots cannot
/// relieve.
const INODE_STALL_LATCH: &str = "/run/sentryusb_inode_stall";

fn inode_reserve(total_inodes: u64) -> u64 {
    (total_inodes / INODE_RESERVE_DIVISOR)
        .max(INODE_RESERVE_FLOOR)
        .min(total_inodes / INODE_RESERVE_CAP_DIVISOR)
}

/// Whether /mutable is mounted read-write. Inode-driven eviction is
/// only safe then: unmounted, `stat /mutable` measures the root
/// filesystem; ro-remounted (ext4 error), release_snapshot deletes the
/// snapshot but cannot remove its symlinks — permanent footage loss
/// with zero inode recovery. (A write-probe would be wrong: an
/// inode-FULL filesystem also fails writes, and that is exactly the
/// state eviction exists to fix — so check the mount option.)
fn mutable_rw_mounted() -> bool {
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let _dev = fields.next();
        if fields.next() != Some("/mutable") {
            return false;
        }
        let _fstype = fields.next();
        fields
            .next()
            .is_some_and(|opts| opts.split(',').any(|o| o == "rw"))
    })
}

/// Consecutive-zero-gain counter behind the stall guard — see
/// [`INODE_STALL_LATCH`]. Counts only successful releases; any release
/// that actually freed /mutable inodes (or ran under block pressure,
/// where eviction is always legitimate) resets it.
struct StallGuard {
    consecutive: u32,
}

impl StallGuard {
    fn new() -> Self {
        Self { consecutive: 0 }
    }

    /// Record one successful release; returns true when the guard trips.
    fn record(
        &mut self,
        block_target_met: bool,
        inodes_before: Option<u64>,
        inodes_after: Option<u64>,
    ) -> bool {
        if !block_target_met {
            self.consecutive = 0;
            return false;
        }
        match (inodes_before, inodes_after) {
            (Some(before), Some(after)) if after <= before => {
                self.consecutive += 1;
                self.consecutive >= MAX_STALE_INODE_RELEASES
            }
            _ => {
                self.consecutive = 0;
                false
            }
        }
    }
}

/// Both eviction targets: block free space on /backingfiles at or above
/// the reserve, and /mutable free inodes strictly above the inode
/// reserve (`None` = /mutable unreadable, e.g. dev containers — treat
/// as satisfied rather than evicting on unknown data; matches the bash
/// script skipping the check when stat fails).
fn targets_met(free: u64, reserve: u64, free_inodes: Option<u64>, ireserve: u64) -> bool {
    free >= reserve && free_inodes.map_or(true, |f| f > ireserve)
}

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

/// Give up on inode-driven eviction and latch the verdict.
///
/// Parity with `halt_inode_eviction` in manage_free_space.sh. Called from the
/// terminal branches when nothing can be released and the BLOCK target is
/// already met, so inode pressure is the only thing driving eviction and no
/// further attempt can help. Latching stops the caller re-entering every 30
/// seconds, and keeps the advice honest: the CAM_SIZE remedy is for block
/// pressure and cannot add inodes to an existing filesystem.
fn halt_inode_eviction(free_inodes: Option<u64>) {
    let _ = std::fs::write(INODE_STALL_LATCH, free_inodes.unwrap_or(0).to_string());
    warn!(
        "clip index (/mutable inodes) is low but no snapshot can be released to \
         relieve it; cleanup cannot add inodes to an existing filesystem. \
         Not retrying automatically."
    );
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

    // /mutable inode headroom — see INODE_RESERVE_DIVISOR. Anything
    // that makes the inode data untrustworthy or eviction unsafe (not
    // rw-mounted, unreadable stats, a standing stall latch) disables
    // the inode policy only: block-space eviction must keep working.
    let mutable_inodes = if mutable_rw_mounted() { get_inodes(MUTABLE).ok() } else { None };
    let mut ireserve = mutable_inodes.map_or(0, |(t, _)| inode_reserve(t));
    let mut free_inodes = mutable_inodes.map(|(_, f)| f);

    // Defence in depth against an unreachable inode target. `inode_reserve`
    // caps at a quarter of the table so this cannot fire today, but a future
    // edit to the formula must never again be able to demand more free inodes
    // than the filesystem physically has: that is what made shipped v3.20.x
    // delete every releasable snapshot on single-disk installs and then fail
    // forever. Disable the inode policy instead of evicting toward a target
    // no amount of deletion can reach; block-space eviction is unaffected.
    if let Some((total_inodes, _)) = mutable_inodes {
        if ireserve >= total_inodes {
            warn!(
                "inode reserve {} >= /mutable inode table {} — target unreachable,                  disabling inode-driven eviction (no snapshots will be released for inodes)",
                ireserve, total_inodes
            );
            ireserve = 0;
            free_inodes = None;
        }
    }

    // Honor a previous stall verdict: resume inode-driven eviction only
    // after free inodes actually rose above the latched value.
    if let Some(latched) = read_stall_latch() {
        match free_inodes {
            Some(now) if now > latched => {
                let _ = std::fs::remove_file(INODE_STALL_LATCH);
            }
            _ => {
                if free_inodes.is_some() {
                    info!(
                        "inode-driven eviction suspended (stalled at {} free; see {})",
                        latched, INODE_STALL_LATCH
                    );
                }
                ireserve = 0;
                free_inodes = None;
            }
        }
    }

    info!(
        "Disk space: {} free / {} total bytes; reserve={} (source={}, formula=10GiB+total/33); \
         /mutable inodes free={:?} reserve={}",
        free, total, reserve, source, free_inodes, ireserve
    );

    // Byte equality satisfies the target; do not evict a snapshot at the boundary.
    if targets_met(free, reserve, free_inodes, ireserve) {
        return Ok(());
    }

    if free < reserve {
        info!("Free space below reserve ({} bytes), releasing old snapshots...", reserve);
    } else {
        info!(
            "/mutable free inodes {:?} at or below reserve {}, releasing old snapshots...",
            free_inodes, ireserve
        );
    }

    // Slot numbering can restart after a reflash, so release by mtime age.
    let snapshots = super::snapshot::list_snapshots_by_age();
    if snapshots.is_empty() {
        if free >= reserve {
            halt_inode_eviction(free_inodes);
            anyhow::bail!("clip index low but no snapshots exist to release");
        }
        anyhow::bail!(
            "low space for new snapshots, but no snapshots exist — \
             use a larger storage medium or reduce CAM_SIZE"
        );
    }

    // A clock rollback can make the highest-slot snapshot appear oldest.
    let total_snaps = snapshots.len();
    let snapshots = releasable(snapshots);
    if snapshots.is_empty() {
        if free >= reserve {
            halt_inode_eviction(free_inodes);
            anyhow::bail!(
                "clip index low but the only {} snapshot(s) present are the \
                 protected newest/highest",
                total_snaps
            );
        }
        anyhow::bail!(
            "low space for new snapshots, but the only {} snapshot(s) present are the \
             protected newest/highest — use a larger storage medium or reduce CAM_SIZE",
            total_snaps
        );
    }

    // Release oldest snapshots first until both targets are met.
    let mut recovered = false;
    let mut guard = StallGuard::new();
    let inode_policy_on = ireserve > 0;
    for snap in &snapshots {
        let inodes_before =
            if inode_policy_on { get_inodes(MUTABLE).ok().map(|(_, f)| f) } else { None };
        if let Err(e) = super::snapshot::release_snapshot_locked(snap).await {
            warn!("Failed to release {}: {}", snap, e);
            continue;
        }

        let (_, new_free) = get_space(BACKINGFILES)?;
        let new_free_inodes =
            if inode_policy_on { get_inodes(MUTABLE).ok().map(|(_, f)| f) } else { None };
        info!(
            "After releasing {}: {} bytes free (reserve {}); /mutable inodes free={:?} (reserve {})",
            snap, new_free, reserve, new_free_inodes, ireserve
        );

        if targets_met(new_free, reserve, new_free_inodes, ireserve) {
            recovered = true;
            break;
        }

        // Stall guard, mirroring manage_free_space.sh: releasing a
        // snapshot only relieves /mutable inode pressure when it still
        // owns clip symlinks there. If the block target is already met
        // and several releases in a row free no inodes, the table is
        // being eaten by something else — latch that verdict (so
        // retrying callers don't drain the snapshot store three at a
        // time) and stop.
        if inode_policy_on && guard.record(new_free >= reserve, inodes_before, new_free_inodes) {
            if let Some(f) = new_free_inodes {
                let _ = std::fs::write(INODE_STALL_LATCH, f.to_string());
            }
            anyhow::bail!(
                "inode pressure on /mutable ({:?} free, reserve {}) not relieved by \
                 releasing snapshots — something other than clip symlinks is \
                 consuming inodes",
                new_free_inodes,
                ireserve
            );
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

/// Free-inode count recorded by a previous stall, if any.
fn read_stall_latch() -> Option<u64> {
    std::fs::read_to_string(INODE_STALL_LATCH).ok()?.trim().parse().ok()
}

/// Get total and free inode counts for a filesystem.
///
/// Same fail-closed parsing as [`get_space`]: malformed stat output is
/// an error, not (0, 0) — a zero total would silently disable the
/// inode-headroom policy at the call site.
fn get_inodes(path: &str) -> Result<(u64, u64)> {
    let output = std::process::Command::new("stat")
        .args(["--file-system", "--format=%c %d", path])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("stat failed for {}", path);
    }

    let s = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = s.trim().split_whitespace().collect();
    if parts.len() < 2 {
        anyhow::bail!("unexpected stat output for {}: {:?}", path, s.trim());
    }
    let total = parts[0]
        .parse::<u64>()
        .map_err(|e| anyhow::anyhow!("bad inode total in stat output for {}: {}", path, e))?;
    let free = parts[1]
        .parse::<u64>()
        .map_err(|e| anyhow::anyhow!("bad free inode count in stat output for {}: {}", path, e))?;
    if total == 0 {
        anyhow::bail!("stat reported zero inode capacity for {}", path);
    }
    Ok((total, free))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1024 * 1024 * 1024;

    /// The 2026-08-19 field failure in numbers: the stock /mutable
    /// partition has 120,960 inodes at mkfs.ext4 defaults, and clip
    /// symlinks consumed all of them while 71% of the bytes were free.
    /// The 20k floor (≈ a week of links at ~2.9k/day) governs small
    /// tables; the /20 term takes over only on denser ones, so the
    /// reserve tracks recovery time instead of table size.
    #[test]
    fn inode_reserve_is_floored_at_a_week_of_links() {
        assert_eq!(inode_reserve(120_960), 20_000); // stock table: floor wins
        assert_eq!(inode_reserve(472_000), 23_600); // dense table: /20 wins
        assert_eq!(inode_reserve(0), 0);
    }

    /// THE v3.20.x REGRESSION. Single-disk installs size /mutable's inode
    /// table from the data area, so real cards carry far fewer inodes than
    /// the 20k floor: a 128 GB card measures 11,856 total. Shipped v3.20.0
    /// demanded 20,000 FREE from that table, which no amount of eviction can
    /// reach, so cleanup deleted every releasable snapshot and then failed on
    /// a 60-second loop forever (two users lost 23 and 45 snapshots).
    ///
    /// The invariant that matters is not any particular number: the reserve
    /// must always be strictly reachable on the table it is measured on.
    #[test]
    fn inode_reserve_is_always_reachable_on_real_geometries() {
        // Measured on affected hardware / reproduced with mkfs.ext4.
        for &total in &[11_856_u64, 19_200, 22_000, 24_000, 73_488, 120_960, 131_072, 472_000] {
            let r = inode_reserve(total);
            assert!(
                r < total,
                "reserve {} must be reachable on a {}-inode table",
                r,
                total
            );
            // Reachable is not enough: an empty filesystem must comfortably
            // satisfy it, or the device evicts from the moment it boots.
            assert!(
                r <= total / 4,
                "reserve {} exceeds a quarter of the {}-inode table",
                r,
                total
            );
        }
    }

    /// The cap must not disturb the geometries the original 2026-08-19 fix
    /// was calibrated against — a hotfix for small tables must not quietly
    /// weaken the large-table protection that motivated the feature.
    #[test]
    fn inode_reserve_unchanged_on_large_tables() {
        assert_eq!(inode_reserve(120_960), 20_000);
        assert_eq!(inode_reserve(131_072), 20_000);
        assert_eq!(inode_reserve(472_000), 23_600);
    }

    /// Small tables get a proportional, satisfiable target instead of the
    /// impossible flat floor.
    #[test]
    fn inode_reserve_scales_down_on_small_tables() {
        assert_eq!(inode_reserve(11_856), 2_964); // 128 GB single-disk card
        assert_eq!(inode_reserve(19_200), 4_800); // 300 MiB mkfs default
    }

    /// Zero-gain releases only count against the guard while the block
    /// target is met (inode-only eviction); block-pressure releases and
    /// any release that actually freed inodes reset it.
    #[test]
    fn stall_guard_trips_after_three_zero_gain_releases() {
        let mut g = StallGuard::new();
        assert!(!g.record(true, Some(100), Some(100)));
        assert!(!g.record(true, Some(100), Some(100)));
        assert!(g.record(true, Some(100), Some(100)));
    }

    #[test]
    fn stall_guard_resets_on_progress_or_block_pressure() {
        let mut g = StallGuard::new();
        assert!(!g.record(true, Some(100), Some(100)));
        assert!(!g.record(true, Some(100), Some(100)));
        // Progress: the release freed inodes.
        assert!(!g.record(true, Some(100), Some(5_000)));
        assert!(!g.record(true, Some(100), Some(100)));
        // Block pressure: eviction is legitimate regardless of inodes.
        assert!(!g.record(false, Some(100), Some(100)));
        assert!(!g.record(true, Some(100), Some(100)));
        assert!(!g.record(true, Some(100), Some(100)));
        assert!(g.record(true, Some(100), Some(100)));
    }

    /// Unreadable inode counts must not accumulate toward a trip.
    #[test]
    fn stall_guard_ignores_unreadable_inode_counts() {
        let mut g = StallGuard::new();
        for _ in 0..5 {
            assert!(!g.record(true, None, None));
            assert!(!g.record(true, Some(100), None));
        }
    }

    /// Byte target met but inode target not: eviction must continue —
    /// this is exactly the state the 2026-08-19 incident sat in for a
    /// day (block-space policy satisfied, index unusable).
    #[test]
    fn targets_not_met_when_inodes_low_despite_free_bytes() {
        assert!(!targets_met(100 * GIB, 10 * GIB, Some(5_000), 20_160));
    }

    /// Both satisfied → done. Inodes use strict `>` (bash `-gt` parity).
    #[test]
    fn targets_met_needs_inodes_strictly_above_reserve() {
        assert!(targets_met(100 * GIB, 10 * GIB, Some(20_161), 20_160));
        assert!(!targets_met(100 * GIB, 10 * GIB, Some(20_160), 20_160));
    }

    /// Unreadable /mutable (dev containers) disables the inode check
    /// rather than blocking block-space eviction — and must never make
    /// a low-bytes state look satisfied.
    #[test]
    fn targets_ignore_inodes_when_mutable_unreadable() {
        assert!(targets_met(100 * GIB, 10 * GIB, None, 0));
        assert!(!targets_met(GIB, 10 * GIB, None, 0));
    }

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
