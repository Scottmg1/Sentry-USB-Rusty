//! Reflink snapshots, TOC deduplication, and TeslaCam link bookkeeping.
//! The link farm feeds archiveloop, drive processing, the Viewer, and iOS.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use tracing::{info, warn};

/// Snapshot filesystem; link sweeps fail closed unless it is mounted.
const BACKINGFILES: &str = "/backingfiles";
const SNAPSHOTS_DIR: &str = "/backingfiles/snapshots";

/// autofs root targeted by each snapshot's stable `mnt` symlink.
const AUTOFS_SNAPSHOTS: &str = "/tmp/snapshots";
const CAM_DISK: &str = "/backingfiles/cam_disk.bin";
const REBUILD_FLAG: &str = "/mutable/.rebuild_snapshot_symlinks";

/// Persistent marker for the one-time legacy event-cross-link purge.
const PURGE_MARKER: &str = "/mutable/.recentclips_events_purged";

const TESLACAM: &str = "/mutable/TeslaCam";

/// Drive-processor manifest of event timestamps that fill genuine RecentClips
/// gaps. Only these events are cross-linked for continuous playback.
const GAPFILL_MANIFEST: &str = "/mutable/.gapfill_recent_links";

/// Last applied gap-fill manifest; content changes trigger old-snapshot backfill.
const GAPFILL_APPLIED_MARKER: &str = "/mutable/.gapfill_links_applied";

/// Load gap-fill timestamps; unreadable manifests safely produce no cross-links.
fn load_gapfill_stamps() -> HashSet<String> {
    match std::fs::read_to_string(GAPFILL_MANIFEST) {
        Ok(s) => s
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect(),
        Err(_) => HashSet::new(),
    }
}

/// Shared timestamp prefix used to match every camera for a clip minute.
fn clip_stamp(name: &str) -> Option<&str> {
    if name.len() >= 19 && looks_like_dated_clip(name) {
        Some(&name[..19])
    } else {
        None
    }
}

/// Create a cam snapshot and its TOC/link bookkeeping. `skip_fsck` avoids a
/// redundant post-reboot check. Returns None for a duplicate snapshot.
pub async fn make_snapshot(skip_fsck: bool) -> Result<Option<String>> {
    // Serialize all snapshot producers and sweeps.
    let _lock = lock_snapshots_dir()?;

    if !Path::new(CAM_DISK).exists() {
        bail!("cam disk image not found at {}", CAM_DISK);
    }

    // Reuse an unmounted prior slot when no TOC was committed.
    let (snap_num, prev_toc) = pick_next_snapshot_slot()?;
    let snap_name = format!("snap-{:06}", snap_num);
    let snap_dir = format!("{}/{}", SNAPSHOTS_DIR, snap_name);
    let snap_file = format!("{}/snap.bin", snap_dir);
    let snap_mnt = format!("/tmp/snapshots/{}", snap_name);
    let snap_mnt_link = format!("{}/mnt", snap_dir);

    std::fs::create_dir_all(&snap_dir)?;
    info!("Taking snapshot of cam disk in {}", snap_dir);

    // reflink=auto retains a full-copy fallback. Low best-effort priority keeps
    // car writes responsive while still progressing under continuous recording.
    let cp_result = sentryusb_shell::run_with_timeout(
        Duration::from_secs(600),
        "ionice",
        &["-c2", "-n7", "nice", "-n19", "cp", "--reflink=auto", CAM_DISK, &snap_file],
    )
    .await;
    if let Err(e) = cp_result {
        let _ = std::fs::remove_dir_all(&snap_dir);
        bail!("cp --reflink failed: {}", e);
    }

    if !skip_fsck {
        if let Err(e) = fsck_snapshot(&snap_file).await {
            warn!("fsck on {} failed (non-fatal): {}", snap_file, e);
        }
    }

    if cfg!(target_pointer_width = "32") {
        let _ = apply_bookworm_32bit_timestamp_fix(&snap_file).await;
    }

    // Start autofs before traversing its snapshot roots.
    wait_for_autofs().await;

    info!("Took snapshot {}", snap_name);

    // Trigger the autofs mount before generating the TOC.
    let _ = sentryusb_shell::run("ls", &[&format!("{}/", snap_mnt)]).await;

    let toc_path = format!("{}.toc", snap_file);
    let toc_path_tmp = format!("{}_", toc_path);
    if let Err(e) = generate_toc(&snap_mnt, &toc_path_tmp).await {
        warn!("toc generation failed for {}: {}", snap_mnt, e);
    }

    // Discard snapshots with no TOC changes versus the prior capture.
    let is_new = match prev_toc.as_ref() {
        Some(prev) => toc_has_additions(prev, &toc_path_tmp).unwrap_or(true),
        None => true,
    };

    let is_duplicate = run_link_maintenance_before_duplicate_check(is_new, || {
        // Apply changed manifests even when the cam snapshot is a duplicate.
        if let Err(e) = backfill_gapfill_links() {
            warn!("backfill_gapfill_links: {}", e);
        }
    });

    if is_duplicate {
        info!("Snapshot {} identical to previous; discarding", snap_name);
        let _ = std::fs::remove_file(&toc_path_tmp);
        let _ = std::fs::remove_file(&snap_file);
        let _ = std::fs::remove_dir_all(&snap_dir);
        return Ok(None);
    }

    // Snapshot links intentionally outlive car-initiated or user clip deletions.

    // Create the stable target before per-clip links reference it.
    if !Path::new(&snap_mnt_link).exists() {
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&snap_mnt, &snap_mnt_link);
    }

    if let Err(e) = make_links_for_snapshot(&snap_mnt, &snap_mnt_link) {
        warn!("make_links_for_snapshot failed: {}", e);
    }

    // TOC rename commits the snapshot for reuse and pruning decisions.
    let _ = std::fs::rename(&toc_path_tmp, &toc_path);

    if Path::new(REBUILD_FLAG).exists() {
        if let Err(e) = rebuild_all_snapshot_links() {
            warn!("rebuild_all_snapshot_links: {}", e);
        }
        let _ = std::fs::remove_file(REBUILD_FLAG);
    }

    // Purge legacy event cross-links once per device.
    if !Path::new(PURGE_MARKER).exists() {
        let gapfill = load_gapfill_stamps();
        if let Err(e) = purge_event_links_in(&Path::new(TESLACAM).join("RecentClips"), &gapfill) {
            warn!("purge_event_links_in: {}", e);
        }
        let _ = std::fs::write(PURGE_MARKER, b"done\n");
    }

    Ok(Some(snap_name))
}

fn run_link_maintenance_before_duplicate_check(
    is_new: bool,
    maintenance: impl FnOnce(),
) -> bool {
    maintenance();
    !is_new
}

/// Normalize a bare name or full path to a validated `snap-NNNNNN` basename.
fn normalize_snap_name(input: &str) -> Option<String> {
    let name = Path::new(input).file_name()?.to_str()?;
    if name.starts_with("snap-") && !name.contains("..") {
        Some(name.to_string())
    } else {
        None
    }
}

/// Release (delete) a snapshot. Accepts a bare `snap-NNNNNN` name or a full
/// path under the snapshots dir (see [`normalize_snap_name`]).
pub async fn release_snapshot(snap_name: &str) -> Result<()> {
    let _lock = lock_snapshots_dir()?;
    release_snapshot_locked(snap_name).await
}

/// Release body for callers already holding the non-reentrant snapshot lock.
pub(crate) async fn release_snapshot_locked(snap_name: &str) -> Result<()> {
    let name = match normalize_snap_name(snap_name) {
        Some(n) => n,
        None => bail!("invalid snapshot name: {}", snap_name),
    };

    let snap_dir = format!("{}/{}", SNAPSHOTS_DIR, name);
    if !Path::new(&snap_dir).exists() {
        bail!("snapshot not found: {}", name);
    }

    let mnt_dir = format!("{}/mnt", snap_dir);
    if Path::new(&mnt_dir).exists() {
        let _ = sentryusb_shell::run("umount", &[&mnt_dir]).await;
    }

    std::fs::remove_dir_all(&snap_dir)?;
    // Remove stored-target links and directories they leave empty.
    let pruned = prune_links_into(&name);
    if pruned > 0 {
        info!("Pruned {} TeslaCam link(s) into {}", pruned, name);
    }
    info!("Released snapshot: {}", name);
    Ok(())
}

/// Remove link-farm symlinks whose stored target names this snapshot, without
/// resolving them through autofs.
fn prune_links_into(snap_name: &str) -> usize {
    let snapshots = Path::new(SNAPSHOTS_DIR);
    let autofs = Path::new(AUTOFS_SNAPSHOTS);
    let dead = |_: &Path, target: &str| released_link_is_dead(target, snap_name, snapshots, autofs);
    let (removed, aborted) =
        prune_farm_links(Path::new(TESLACAM), &dead, &backingfiles_mounted, false);
    if aborted {
        warn!(
            "{} unmounted during prune of {} — aborted after {} link(s)",
            BACKINGFILES, snap_name, removed
        );
    }
    removed
}

/// Match only producer-shaped targets to a snapshot still proven absent.
fn released_link_is_dead(target: &str, snap_name: &str, snapshots: &Path, autofs: &Path) -> bool {
    owned_snap_component(target, snapshots, autofs) == Some(snap_name)
        && matches!(snapshot_state(snapshots, snap_name), SnapState::Gone)
}

/// Re-prove the snapshot filesystem is mounted throughout pruning.
fn backingfiles_mounted() -> bool {
    is_mounted(Path::new(BACKINGFILES))
}

/// Depth-capped link-farm prune with guard checks per directory and unlink.
/// Returns `(removed, aborted)` and preserves top-level categories.
fn prune_farm_links(
    farm: &Path,
    dead: &dyn Fn(&Path, &str) -> bool,
    guard: &dyn Fn() -> bool,
    dry_run: bool,
) -> (usize, bool) {
    fn walk(
        dir: &Path,
        depth: u8,
        dead: &dyn Fn(&Path, &str) -> bool,
        guard: &dyn Fn() -> bool,
        dry_run: bool,
        removed: &mut usize,
    ) -> bool {
        if depth > 4 {
            return true;
        }
        if !guard() {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return true };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ftype) = entry.file_type() else { continue };
            if ftype.is_symlink() {
                if let Ok(target) = std::fs::read_link(&path) {
                    let target = target.to_string_lossy().to_string();
                    if dead(&path, &target) {
                        // Re-prove mount state immediately before deletion.
                        if !guard() {
                            return false;
                        }
                        if dry_run || unlink_if_still_dead(&path, &target, dead) {
                            *removed += 1;
                        }
                    }
                }
            } else if ftype.is_dir() {
                if !walk(&path, depth + 1, dead, guard, dry_run, removed) {
                    return false;
                }
                // Only date/event directories are eligible; non-empty removal fails safely.
                if !dry_run && depth >= 1 {
                    let _ = std::fs::remove_dir(&path);
                }
            }
        }
        true
    }
    let mut removed = 0;
    let completed = walk(farm, 0, dead, guard, dry_run, &mut removed);
    (removed, !completed)
}

/// Re-read immediately before unlinking so most concurrent retargets survive.
/// A residual path-based readlink/unlink race remains.
fn unlink_if_still_dead(link: &Path, target: &str, dead: &dyn Fn(&Path, &str) -> bool) -> bool {
    let Ok(fresh) = std::fs::read_link(link) else { return false };
    let fresh = fresh.to_string_lossy().to_string();
    fresh == target && dead(link, &fresh) && std::fs::remove_file(link).is_ok()
}

/// Delete producer-shaped links to snapshots proven absent, without resolving
/// targets. Dry-run returns the prospective count.
pub fn sweep_dangling_links(dry_run: bool) -> Result<usize> {
    // An unmounted backingfiles path can contain misleading stale directories.
    if !is_mounted(Path::new(BACKINGFILES)) {
        bail!("{} is not a mount point — sweep skipped", BACKINGFILES);
    }
    // Share the producer flock to narrow relink/unlink races.
    let _lock = lock_snapshots_dir()?;
    // Never mutate an active overlay's lower directory.
    if archive_overlay_active() {
        bail!("archive overlay is mounted — sweep skipped");
    }
    sweep_dangling_links_in(
        Path::new(TESLACAM),
        Path::new(SNAPSHOTS_DIR),
        Path::new(AUTOFS_SNAPSHOTS),
        &backingfiles_mounted,
        dry_run,
    )
}

/// Testable sweep over explicit roots; a false guard aborts the walk.
fn sweep_dangling_links_in(
    farm: &Path,
    snapshots: &Path,
    autofs: &Path,
    guard: &dyn Fn() -> bool,
    dry_run: bool,
) -> Result<usize> {
    // No visible snapshots may mean backingfiles is unmounted; fail closed.
    let any_snap = std::fs::read_dir(snapshots)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_name().to_string_lossy().starts_with("snap-") && e.path().is_dir()
            })
        })
        .unwrap_or(false);
    if !any_snap {
        bail!("no snapshots visible under {} — sweep skipped", snapshots.display());
    }
    let dead = |_: &Path, target: &str| {
        // Remove only producer-shaped links to snapshots proven gone.
        owned_snap_component(target, snapshots, autofs)
            .is_some_and(|name| matches!(snapshot_state(snapshots, name), SnapState::Gone))
    };
    let (removed, aborted) = prune_farm_links(farm, &dead, guard, dry_run);
    if aborted {
        bail!("mount state changed mid-sweep — aborted after {} link(s)", removed);
    }
    Ok(removed)
}

/// Whether a snapshot dir is present, provably gone, or undeterminable.
enum SnapState {
    Live,
    Gone,
    Unknown,
}

/// Classify a snapshot; unreadable state is Unknown rather than Gone.
fn snapshot_state(snapshots: &Path, name: &str) -> SnapState {
    match std::fs::metadata(snapshots.join(name)) {
        Ok(md) if md.is_dir() => SnapState::Live,
        Ok(_) => SnapState::Gone,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::metadata(snapshots).is_ok_and(|m| m.is_dir()) {
                SnapState::Gone
            } else {
                SnapState::Unknown
            }
        }
        Err(_) => SnapState::Unknown,
    }
}

/// Extract a snapshot from the stable-mnt or autofs target shapes minted here.
fn owned_snap_component<'a>(target: &'a str, snapshots: &Path, autofs: &Path) -> Option<&'a str> {
    // Reject non-absolute or non-canonical components that escape the prefix.
    if !target.starts_with('/')
        || target[1..].split('/').any(|c| c.is_empty() || c == "." || c == "..")
    {
        return None;
    }
    let under = |root: &Path| -> Option<&'a str> {
        target.strip_prefix(root.to_str()?)?.strip_prefix('/')
    };
    let is_snap = |c: &&str| {
        c.strip_prefix("snap-")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
    };
    if let Some(rest) = under(snapshots) {
        let mut parts = rest.split('/');
        let name = parts.next().filter(is_snap)?;
        // Stable retargeted links include the snapshot's `mnt` component.
        return (parts.next() == Some("mnt") && parts.next().is_some()).then_some(name);
    }
    let mut parts = under(autofs)?.split('/');
    let name = parts.next().filter(is_snap)?;
    parts.next().is_some().then_some(name)
}

/// Mount-point check using /proc/mounts, with st_dev fallback only if unreadable.
fn is_mounted(path: &Path) -> bool {
    let want = path.to_string_lossy();
    if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
        return mounts
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(want.as_ref()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Do not follow a symlink onto another device for the fallback check.
        let (Ok(md), Some(parent)) = (std::fs::symlink_metadata(path), path.parent()) else {
            return false;
        };
        if md.file_type().is_symlink() {
            return false;
        }
        std::fs::metadata(parent).is_ok_and(|p| p.dev() != md.dev())
    }
    #[cfg(not(unix))]
    false
}

/// Bounded advisory flock shared by bash/Rust producers and the sweep.
pub(crate) fn lock_snapshots_dir() -> Result<std::fs::File> {
    let _ = std::fs::create_dir_all(SNAPSHOTS_DIR);
    let file = std::fs::File::open(SNAPSHOTS_DIR)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if crate::cycle_lock::try_flock_exclusive(&file)? {
            return Ok(file);
        }
        if std::time::Instant::now() >= deadline {
            bail!("snapshots dir lock busy — skipped");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// True while archiveloop's archive overlay (lower = the TeslaCam farm) is
/// mounted at /tmp/cam/merged.
fn archive_overlay_active() -> bool {
    std::fs::read_to_string("/proc/mounts")
        .map(|m| m.lines().any(|l| l.split_whitespace().nth(1) == Some("/tmp/cam/merged")))
        .unwrap_or(false)
}

/// List all snapshots.
pub fn list_snapshots() -> Vec<String> {
    let mut snaps = Vec::new();
    if let Ok(entries) = std::fs::read_dir(SNAPSHOTS_DIR) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("snap-") && entry.path().is_dir() {
                snaps.push(name);
            }
        }
    }
    snaps.sort();
    snaps
}

/// Eviction candidates ordered by `snap.bin` mtime, then name. Slot numbers can
/// restart after reflashing, so lexical order is unsafe. Only physical numeric
/// directories with readable images qualify.
pub fn list_snapshots_by_age() -> Vec<String> {
    list_snapshots_by_age_in(Path::new(SNAPSHOTS_DIR))
}

fn list_snapshots_by_age_in(base: &Path) -> Vec<String> {
    let mut aged: Vec<(std::time::SystemTime, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let numeric = name
                .strip_prefix("snap-")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
            // Never admit symlinks as eviction candidates.
            if !numeric || !entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                continue;
            }
            let Ok(meta) = std::fs::symlink_metadata(entry.path().join("snap.bin")) else {
                continue;
            };
            let Ok(mtime) = meta.modified() else {
                continue;
            };
            aged.push((mtime, name));
        }
    }
    aged.sort();
    aged.into_iter().map(|(_, n)| n).collect()
}

/// Find the next slot, reusing an unmounted incomplete snapshot. Return its
/// number and the preceding completed TOC, if any.
fn pick_next_snapshot_slot() -> Result<(u32, Option<String>)> {
    pick_next_snapshot_slot_in(Path::new(SNAPSHOTS_DIR))
}

/// Check nested and autofs mounts for a snapshot using prefix matching.
fn snapshot_slot_has_mounts(base: &Path, name: &str) -> bool {
    let under = format!("{}/{}/", base.to_string_lossy(), name);
    let autofs = format!("/tmp/snapshots/{}", name);
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        // Unknown mount state fails closed against destructive reuse.
        return true;
    };
    mounts.lines().any(|l| {
        let Some(mp) = l.split_whitespace().nth(1) else {
            return false;
        };
        mp.starts_with(&under) || mp == autofs || mp.starts_with(&format!("{}/", autofs))
    })
}

fn pick_next_snapshot_slot_in(base: &Path) -> Result<(u32, Option<String>)> {
    pick_next_snapshot_slot_with(base, &|b, n| snapshot_slot_has_mounts(b, n))
}

fn pick_next_snapshot_slot_with(
    base: &Path,
    has_mounts: &dyn Fn(&Path, &str) -> bool,
) -> Result<(u32, Option<String>)> {
    // Option distinguishes a real snap-000000 from no prior snapshot.
    let mut max_num: Option<u32> = None;
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Only physical numeric directories drive allocation.
            let num = name
                .strip_prefix("snap-")
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<u32>().ok());
            if let Some(num) = num
                && entry.file_type().is_ok_and(|ft| ft.is_dir())
                && max_num.is_none_or(|m| num > m)
            {
                max_num = Some(num);
            }
        }
    }

    let Some(max_num) = max_num else {
        info!("slot pick: picker=rust max_seen=none action=fresh next=1");
        return Ok((1, None));
    };

    let prev_name = format!("snap-{:06}", max_num);
    let prev_dir = base.join(&prev_name);
    let prev_toc = prev_dir.join("snap.bin.toc");
    let prev_bin = prev_dir.join("snap.bin");

    // Reuse the highest slot when no TOC was committed.
    if !prev_toc.exists() || !prev_bin.exists() {
        // Append instead if the incomplete slot may still be mounted.
        if has_mounts(base, &prev_name) {
            let next = max_num + 1;
            let backstop = if max_num > 0 {
                let p = base.join(format!("snap-{:06}/snap.bin.toc", max_num - 1));
                if p.exists() { Some(p.to_string_lossy().into_owned()) } else { None }
            } else {
                None
            };
            warn!(
                "slot pick: picker=rust max_seen={} incomplete BUT MOUNTED — appending next={}",
                max_num, next
            );
            return Ok((next, backstop));
        }
        let _ = std::fs::remove_dir_all(&prev_dir);
        let next = max_num;
        // Look one further back for a usable previous TOC.
        let backstop = if next > 0 {
            let p = base.join(format!("snap-{:06}/snap.bin.toc", next - 1));
            if p.exists() { Some(p.to_string_lossy().into_owned()) } else { None }
        } else {
            None
        };
        info!(
            "slot pick: picker=rust max_seen={} action=reuse-incomplete next={}",
            max_num, next
        );
        return Ok((next, backstop));
    }

    info!(
        "slot pick: picker=rust max_seen={} action=append next={}",
        max_num,
        max_num + 1
    );
    Ok((max_num + 1, Some(prev_toc.to_string_lossy().into_owned())))
}

/// fsck through a temporary loop device. Failure is logged but does not abort
/// the archive cycle.
async fn fsck_snapshot(snap_file: &str) -> Result<()> {
    let loop_dev = losetup_find_show(snap_file).await?;
    let part = format!("{}p1", loop_dev);

    // `-p` works for vfat and exfat; report but do not propagate failure.
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(120),
        "fsck",
        &[&part, "--", "-p"],
    )
    .await;

    let _ = sentryusb_shell::run("losetup", &["-d", &loop_dev]).await;
    Ok(())
}

/// Retry `losetup -f -P --show` around kernels that race partition probing.
async fn losetup_find_show(file: &str) -> Result<String> {
    for attempt in 0..5 {
        let out = sentryusb_shell::run("losetup", &["-f", "-P", "--show", file]).await;
        match out {
            Ok(s) => {
                let dev = s.trim().to_string();
                if !dev.is_empty() && Path::new(&dev).exists() {
                    return Ok(dev);
                }
            }
            Err(_) if attempt < 4 => {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
            Err(e) => bail!("losetup failed: {}", e),
        }
    }
    bail!("losetup did not produce a usable device for {}", file)
}

/// Wait up to about 30 seconds for autofs without hanging an archive cycle.
async fn wait_for_autofs() {
    for _ in 0..30 {
        if sentryusb_shell::run("systemctl", &["--quiet", "is-active", "autofs"])
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    warn!("autofs is not active after 30s; symlinks may dangle");
}

/// Write a `<size> <relative-path>` TOC for files under `root`.
async fn generate_toc(root: &str, out_path: &str) -> Result<()> {
    let cmd = format!(
        "find {} -type f -printf '%s %P\\n' > {}",
        shell_escape(root),
        shell_escape(out_path)
    );
    sentryusb_shell::run("bash", &["-c", &cmd])
        .await
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("find/toc: {}", e))
}

fn shell_escape(s: &str) -> String {
    // Snapshot paths are fixed and contain no quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Whether a TOC adds or grows any file; compare whole size/path lines so a
/// fuller copy is not discarded as a duplicate.
fn toc_has_additions(old_toc: &str, new_toc: &str) -> Result<bool> {
    let old = std::fs::read_to_string(old_toc).unwrap_or_default();
    let new = std::fs::read_to_string(new_toc)?;
    let old_set: std::collections::HashSet<&str> =
        old.lines().filter(|l| !l.is_empty()).collect();
    Ok(new
        .lines()
        .any(|line| !line.is_empty() && !old_set.contains(line)))
}

/// Build TeslaCam category links, retargeting initial autofs paths through the
/// stable `<snapdir>/mnt` symlink.
fn make_links_for_snapshot(cur_mnt: &str, final_mnt: &str) -> Result<()> {
    let saved = format!("{}/SavedClips", TESLACAM);
    let sentry = format!("{}/SentryClips", TESLACAM);
    let track = format!("{}/TeslaTrackMode", TESLACAM);
    let _ = std::fs::create_dir_all(&saved);
    let _ = std::fs::create_dir_all(&sentry);

    // Only manifest-listed event timestamps fill RecentClips driving gaps.
    let gapfill = load_gapfill_stamps();

    info!("Making links for {}, retargeted to {}", cur_mnt, final_mnt);

    // Date-bucket flat RecentClips files.
    let recents_root = format!("{}/TeslaCam/RecentClips", cur_mnt);
    if let Ok(entries) = std::fs::read_dir(&recents_root) {
        for entry in entries.flatten() {
            link_clip_into_recents(&entry.path(), cur_mnt, final_mnt);
        }
    }

    let saved_root = format!("{}/TeslaCam/SavedClips", cur_mnt);
    if let Ok(events) = std::fs::read_dir(&saved_root) {
        for evt in events.flatten() {
            let evt_path = evt.path();
            if !evt_path.is_dir() {
                continue;
            }
            let event_time = evt.file_name().to_string_lossy().to_string();
            let evt_dest = format!("{}/{}", saved, event_time);
            let _ = std::fs::create_dir_all(&evt_dest);

            if let Ok(clips) = std::fs::read_dir(&evt_path) {
                for clip in clips.flatten() {
                    // Keep events out of RecentClips unless the gap manifest lists them.
                    let link = format!(
                        "{}/{}",
                        evt_dest,
                        clip.file_name().to_string_lossy()
                    );
                    let _ = std::fs::remove_file(&link);
                    #[cfg(unix)]
                    {
                        let target = retarget_path(&clip.path(), cur_mnt, final_mnt);
                        let _ = std::os::unix::fs::symlink(&target, &link);
                    }
                    // Cross-link only drive-processor gap fills.
                    maybe_gapfill_recent_link(&clip.path(), cur_mnt, final_mnt, &gapfill);
                }
            }
        }
    }

    let sentry_root = format!("{}/TeslaCam/SentryClips", cur_mnt);
    if let Ok(events) = std::fs::read_dir(&sentry_root) {
        for evt in events.flatten() {
            let evt_path = evt.path();
            if !evt_path.is_dir() {
                continue;
            }
            let event_time = evt.file_name().to_string_lossy().to_string();
            let evt_dest = format!("{}/{}", sentry, event_time);
            let _ = std::fs::create_dir_all(&evt_dest);

            if let Ok(clips) = std::fs::read_dir(&evt_path) {
                for clip in clips.flatten() {
                    // Keep ordinary Sentry events in their event folder.
                    let link = format!(
                        "{}/{}",
                        evt_dest,
                        clip.file_name().to_string_lossy()
                    );
                    let _ = std::fs::remove_file(&link);
                    #[cfg(unix)]
                    {
                        let target = retarget_path(&clip.path(), cur_mnt, final_mnt);
                        let _ = std::os::unix::fs::symlink(&target, &link);
                    }
                    // Cross-link only manifest-listed driving-hole fills.
                    maybe_gapfill_recent_link(&clip.path(), cur_mnt, final_mnt, &gapfill);
                }
            }
        }
    }

    // TrackMode remains flat and uses its original mount target.
    let track_root = format!("{}/TeslaTrackMode", cur_mnt);
    if let Ok(entries) = std::fs::read_dir(&track_root) {
        let mut made = false;
        for entry in entries.flatten() {
            if !made {
                let _ = std::fs::create_dir_all(&track);
                made = true;
            }
            let link = format!(
                "{}/{}",
                track,
                entry.file_name().to_string_lossy()
            );
            let _ = std::fs::remove_file(&link);
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&entry.path(), &link);
        }
    }

    info!("Made all links for {}", cur_mnt);
    Ok(())
}

/// Link one clip into its dated RecentClips bucket.
#[cfg_attr(not(unix), allow(unused_variables))]
fn link_clip_into_recents(file: &Path, cur_mnt: &str, final_mnt: &str) {
    let filename = match file.file_name().map(|s| s.to_string_lossy().to_string()) {
        Some(f) => f,
        None => return,
    };
    if !looks_like_dated_clip(&filename) {
        return;
    }
    let filedate = &filename[..10];
    let recents = format!("{}/RecentClips/{}", TESLACAM, filedate);
    let _ = std::fs::create_dir_all(&recents);
    let link = format!("{}/{}", recents, filename);
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    {
        let target = retarget_path(file, cur_mnt, final_mnt);
        let _ = std::os::unix::fs::symlink(&target, &link);
    }
}

/// Cross-link a manifest-listed event gap into dated RecentClips; ignore all
/// other events so parked footage does not flood the continuous timeline.
#[cfg_attr(not(unix), allow(unused_variables))]
fn maybe_gapfill_recent_link(
    clip: &Path,
    cur_mnt: &str,
    final_mnt: &str,
    gapfill: &HashSet<String>,
) {
    if gapfill.is_empty() {
        return;
    }
    let filename = match clip.file_name().map(|s| s.to_string_lossy().to_string()) {
        Some(f) => f,
        None => return,
    };
    let stamp = match clip_stamp(&filename) {
        Some(s) => s,
        None => return,
    };
    if !gapfill.contains(stamp) {
        return;
    }
    let filedate = &filename[..10];
    let recents = format!("{}/RecentClips/{}", TESLACAM, filedate);
    let _ = std::fs::create_dir_all(&recents);
    let link = format!("{}/{}", recents, filename);
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    {
        let target = retarget_path(clip, cur_mnt, final_mnt);
        let _ = std::os::unix::fs::symlink(&target, &link);
    }
}

/// Backfill changed manifest entries from existing event links without touching
/// snapshot mounts or overwriting RecentClips entries.
pub fn backfill_gapfill_links() -> Result<()> {
    let manifest_body = std::fs::read_to_string(GAPFILL_MANIFEST).unwrap_or_default();
    if std::fs::read_to_string(GAPFILL_APPLIED_MARKER).unwrap_or_default() == manifest_body {
        return Ok(());
    }
    let gapfill = load_gapfill_stamps();
    let (made, skipped_dead, retry) =
        backfill_gapfill_links_in(
            Path::new(TESLACAM),
            Path::new(SNAPSHOTS_DIR),
            Path::new(AUTOFS_SNAPSHOTS),
            &gapfill,
        );
    // Mark only after a determinate pass so crashes/errors remain retryable.
    if retry == 0 {
        std::fs::write(GAPFILL_APPLIED_MARKER, &manifest_body)?;
    }
    if made > 0 || skipped_dead > 0 || retry > 0 {
        info!(
            "gap-fill backfill: created {} RecentClips link(s) for manifest clips ({} dead-target source(s) skipped, {} deferred to next pass)",
            made, skipped_dead, retry
        );
    }
    Ok(())
}

/// Testable backfill returning `(created, dead sources, retryable sources)`.
fn backfill_gapfill_links_in(
    teslacam: &Path,
    snapshots: &Path,
    autofs: &Path,
    gapfill: &HashSet<String>,
) -> (usize, usize, usize) {
    if gapfill.is_empty() {
        return (0, 0, 0);
    }
    let mut made = 0usize;
    let mut skipped_dead = 0usize;
    let mut retry = 0usize;
    for sub in ["SavedClips", "SentryClips"] {
        let Ok(events) = std::fs::read_dir(teslacam.join(sub)) else {
            continue;
        };
        for evt in events.flatten() {
            let evt_path = evt.path();
            if !evt_path.is_dir() {
                continue;
            }
            if let Ok(clips) = std::fs::read_dir(&evt_path) {
                for clip in clips.flatten() {
                    match backfill_recent_link(teslacam, snapshots, autofs, &clip.path(), gapfill)
                    {
                        BackfillOutcome::Made => made += 1,
                        BackfillOutcome::DeadTarget => skipped_dead += 1,
                        BackfillOutcome::Retry => retry += 1,
                        BackfillOutcome::Skipped => {}
                    }
                }
            }
        }
    }
    (made, skipped_dead, retry)
}

/// What [`backfill_recent_link`] did with one event-tree entry.
enum BackfillOutcome {
    Made,
    /// Source target is gone; do not create a born-dangling link.
    DeadTarget,
    /// Undetermined source; leave the manifest pending for retry.
    Retry,
    Skipped,
}

/// Backfill one manifest event into RecentClips from its existing event link.
#[cfg_attr(not(unix), allow(unused_variables))]
fn backfill_recent_link(
    teslacam: &Path,
    snapshots: &Path,
    autofs: &Path,
    clip: &Path,
    gapfill: &HashSet<String>,
) -> BackfillOutcome {
    let filename = match clip.file_name().map(|s| s.to_string_lossy().to_string()) {
        Some(f) => f,
        None => return BackfillOutcome::Skipped,
    };
    let stamp = match clip_stamp(&filename) {
        Some(s) => s,
        None => return BackfillOutcome::Skipped,
    };
    if !gapfill.contains(stamp) {
        return BackfillOutcome::Skipped;
    }
    let recents = teslacam.join("RecentClips").join(&filename[..10]);
    let link = recents.join(&filename);
    // Existing continuous or previously backfilled entries always win.
    if std::fs::symlink_metadata(&link).is_ok() {
        return BackfillOutcome::Skipped;
    }
    // Reuse a link's stable stored target; plain files use their own path.
    let target = match std::fs::read_link(clip) {
        Ok(t) => t.to_string_lossy().to_string(),
        Err(_) => clip.to_string_lossy().to_string(),
    };
    // Check target snapshot state without resolving through autofs.
    match owned_snap_component(&target, snapshots, autofs).map(|s| snapshot_state(snapshots, s)) {
        Some(SnapState::Gone) => return BackfillOutcome::DeadTarget,
        // Transient metadata failure is not proof of release.
        Some(SnapState::Unknown) => return BackfillOutcome::Retry,
        Some(SnapState::Live) | None => {}
    }
    let _ = std::fs::create_dir_all(&recents);
    #[cfg(unix)]
    {
        if std::os::unix::fs::symlink(&target, &link).is_ok() {
            BackfillOutcome::Made
        } else {
            BackfillOutcome::Retry
        }
    }
    #[cfg(not(unix))]
    BackfillOutcome::Skipped
}

/// One-time removal of legacy event cross-links from RecentClips. Classify by
/// stored target without resolving autofs, preserve manifest gap fills, and
/// prune emptied date directories.
fn purge_event_links_in(recents_root: &Path, gapfill: &HashSet<String>) -> Result<()> {
    if !recents_root.is_dir() {
        return Ok(());
    }
    info!(
        "purging Saved/Sentry cross-links from {}",
        recents_root.display()
    );
    let date_dirs = match std::fs::read_dir(recents_root) {
        Ok(d) => d,
        Err(_) => return Ok(()),
    };
    for date_entry in date_dirs.flatten() {
        let date_dir = date_entry.path();
        if !date_dir.is_dir() {
            continue;
        }
        if let Ok(clips) = std::fs::read_dir(&date_dir) {
            for clip in clips.flatten() {
                let path = clip.path();
                // Manifest gap fills remain valid despite targeting event folders.
                if let Some(stamp) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(clip_stamp)
                {
                    if gapfill.contains(stamp) {
                        continue;
                    }
                }
                // Inspect only stored symlink targets.
                let is_symlink = std::fs::symlink_metadata(&path)
                    .map(|m| m.file_type().is_symlink())
                    .unwrap_or(false);
                if !is_symlink {
                    continue;
                }
                if let Ok(target) = std::fs::read_link(&path) {
                    let t = target.to_string_lossy().replace('\\', "/");
                    if t.contains("/SavedClips/") || t.contains("/SentryClips/") {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
        }
        // Remove dates that contained only purged events.
        let now_empty = std::fs::read_dir(&date_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if now_empty {
            let _ = std::fs::remove_dir(&date_dir);
        }
    }
    Ok(())
}

/// Retarget an autofs path through stable `<snapdir>/mnt`.
#[cfg(unix)]
fn retarget_path(file: &Path, cur_mnt: &str, final_mnt: &str) -> String {
    let s = file.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_prefix(cur_mnt) {
        format!("{}{}", final_mnt, stripped)
    } else {
        s
    }
}

/// Recognize a dated Tesla clip basename.
fn looks_like_dated_clip(name: &str) -> bool {
    let b = name.as_bytes();
    if b.len() < 10 {
        return false;
    }
    b[0].is_ascii_digit()
        && b[1].is_ascii_digit()
        && b[2].is_ascii_digit()
        && b[3].is_ascii_digit()
        && b[4] == b'-'
        && b[5].is_ascii_digit()
        && b[6].is_ascii_digit()
        && b[7] == b'-'
        && b[8].is_ascii_digit()
        && b[9].is_ascii_digit()
}

/// Rebuild missing link-farm entries from completed snapshots.
pub fn rebuild_all_snapshot_links() -> Result<()> {
    let mut rebuilt = 0usize;
    let entries = match std::fs::read_dir(SNAPSHOTS_DIR) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let snap_dir_path = entry.path();
        if !snap_dir_path.is_dir() {
            continue;
        }
        let snap_name = entry.file_name().to_string_lossy().to_string();
        if !snap_name.starts_with("snap-") {
            continue;
        }
        let toc = snap_dir_path.join("snap.bin.toc");
        let bin = snap_dir_path.join("snap.bin");
        if !toc.exists() || !bin.exists() {
            continue;
        }
        let snap_mnt = format!("/tmp/snapshots/{}", snap_name);
        let snap_mnt_link = snap_dir_path.join("mnt");

        if !snap_mnt_link.exists() {
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&snap_mnt, &snap_mnt_link);
        }

        if has_existing_links_into_snapshot(&snap_name) {
            continue;
        }

        // Confirm the snapshot mounts before walking it.
        if std::fs::read_dir(&snap_mnt).is_err() {
            warn!("rebuild: snapshot {} not mountable, skipping", snap_name);
            continue;
        }

        if let Err(e) = make_links_for_snapshot(
            &snap_mnt,
            &snap_mnt_link.to_string_lossy().to_string(),
        ) {
            warn!("rebuild: make_links_for_snapshot {}: {}", snap_name, e);
            continue;
        }
        rebuilt += 1;
    }

    if rebuilt > 0 {
        info!("Rebuilt symlinks for {} snapshot(s)", rebuilt);
    }
    Ok(())
}

/// Whether the link farm already references a snapshot.
fn has_existing_links_into_snapshot(snap_name: &str) -> bool {
    let needle = format!("/{}/", snap_name);
    walk_for_symlink_pointing_at(Path::new(TESLACAM), &needle, 0)
}

fn walk_for_symlink_pointing_at(dir: &Path, needle: &str, depth: u8) -> bool {
    if depth > 4 {
        return false;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let md = match entry.file_type() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if md.is_symlink() {
            if let Ok(t) = std::fs::read_link(&p) {
                if t.to_string_lossy().contains(needle) {
                    return true;
                }
            }
        } else if md.is_dir() {
            if walk_for_symlink_pointing_at(&p, needle, depth + 1) {
                return true;
            }
        }
    }
    false
}

/// Repair post-Y2038 exFAT atimes on 32-bit Bookworm before fsck.
async fn apply_bookworm_32bit_timestamp_fix(snap_file: &str) -> Result<()> {
    let osr = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let is_bookworm = osr
        .lines()
        .any(|l| l.trim() == "VERSION_ID=\"12\"" || l.trim() == "VERSION_ID=12");
    if !is_bookworm {
        return Ok(());
    }

    let tmpmnt = sentryusb_shell::run("mktemp", &["-d"]).await?.trim().to_string();
    if tmpmnt.is_empty() {
        return Ok(());
    }
    let mount_ok = sentryusb_shell::run(
        "/root/bin/mountimage",
        &[snap_file, &tmpmnt, "rw"],
    )
    .await
    .is_ok();
    if !mount_ok {
        let _ = sentryusb_shell::run("rmdir", &[&tmpmnt]).await;
        return Ok(());
    }
    let cmd = format!(
        "find {} -newerat 20380101 -print0 | xargs -r -0 touch",
        shell_escape(&tmpmnt)
    );
    let _ = sentryusb_shell::run("bash", &["-c", &cmd]).await;
    let _ = sentryusb_shell::run("umount", &[&tmpmnt]).await;
    let _ = sentryusb_shell::run("rmdir", &[&tmpmnt]).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_snapshot_still_runs_link_maintenance() {
        let mut ran = false;
        let duplicate = run_link_maintenance_before_duplicate_check(false, || ran = true);
        assert!(ran, "manifest backfill must run before a duplicate returns");
        assert!(duplicate);
    }

    #[test]
    fn normalize_accepts_bare_name() {
        assert_eq!(normalize_snap_name("snap-000001").as_deref(), Some("snap-000001"));
    }

    #[test]
    fn normalize_accepts_full_path() {
        // UI and script callers pass full paths as well as bare IDs.
        assert_eq!(
            normalize_snap_name("/backingfiles/snapshots/snap-000001").as_deref(),
            Some("snap-000001"),
        );
    }

    #[test]
    fn normalize_accepts_trailing_slash() {
        assert_eq!(
            normalize_snap_name("/backingfiles/snapshots/snap-000042/").as_deref(),
            Some("snap-000042"),
        );
    }

    #[test]
    fn normalize_rejects_non_snapshot() {
        assert_eq!(normalize_snap_name("etc"), None);
        assert_eq!(normalize_snap_name(""), None);
        assert_eq!(normalize_snap_name(".."), None);
    }

    #[test]
    fn normalize_rejects_traversal() {
        // Basename validation prevents traversal outside SNAPSHOTS_DIR.
        assert_eq!(normalize_snap_name("snap-1/../../etc/passwd"), None);
        assert_eq!(normalize_snap_name("/etc/../snap-1/.."), None);
    }

    /// Backfill reuses stored event-link targets only for missing manifest clips.
    #[cfg(unix)]
    #[test]
    fn backfill_creates_missing_manifest_links_only() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let teslacam = root.path().join("TeslaCam");
        let teslacam = teslacam.as_path();
        let snaps = root.path().join("snapshots");
        let autofs = root.path().join("autofs");
        std::fs::create_dir_all(snaps.join("snap-000005")).unwrap();
        let evt = teslacam.join("SavedClips/2026-07-15_04-59-30");
        std::fs::create_dir_all(&evt).unwrap();

        let clip_target = |snap: &str, stamp: &str| {
            format!(
                "{}/{snap}/mnt/TeslaCam/SavedClips/2026-07-15_04-59-30/{stamp}-front.mp4",
                snaps.display()
            )
        };
        let target_a = clip_target("snap-000005", "2026-07-15_04-50-00");
        symlink(&target_a, evt.join("2026-07-15_04-50-00-front.mp4")).unwrap();
        // Existing entries are not clobbered.
        symlink(
            clip_target("snap-000005", "2026-07-15_04-55-00"),
            evt.join("2026-07-15_04-55-00-front.mp4"),
        )
        .unwrap();
        symlink(
            clip_target("snap-000005", "2026-07-15_04-58-00"),
            evt.join("2026-07-15_04-58-00-front.mp4"),
        )
        .unwrap();
        // Released source snapshots cannot produce new dangling links.
        symlink(
            clip_target("snap-000004", "2026-07-15_04-52-00"),
            evt.join("2026-07-15_04-52-00-front.mp4"),
        )
        .unwrap();

        let day = teslacam.join("RecentClips/2026-07-15");
        std::fs::create_dir_all(&day).unwrap();
        let existing = day.join("2026-07-15_04-55-00-front.mp4");
        symlink("/pre/existing/target.mp4", &existing).unwrap();

        let mut gapfill = HashSet::new();
        gapfill.insert("2026-07-15_04-50-00".to_string());
        gapfill.insert("2026-07-15_04-55-00".to_string());
        gapfill.insert("2026-07-15_04-52-00".to_string());

        assert_eq!(
            backfill_gapfill_links_in(teslacam, &snaps, &autofs, &gapfill),
            (1, 1, 0),
        );

        let made = day.join("2026-07-15_04-50-00-front.mp4");
        assert_eq!(
            std::fs::read_link(&made).unwrap().to_string_lossy(),
            target_a
        );
        assert!(
            std::fs::symlink_metadata(day.join("2026-07-15_04-52-00-front.mp4")).is_err(),
            "must not mint a link into a released snapshot",
        );
        assert_eq!(
            std::fs::read_link(&existing).unwrap().to_string_lossy(),
            "/pre/existing/target.mp4"
        );
        assert!(
            std::fs::symlink_metadata(day.join("2026-07-15_04-58-00-front.mp4")).is_err()
        );
        assert_eq!(
            backfill_gapfill_links_in(teslacam, &snaps, &autofs, &gapfill).0,
            0,
        );
        assert_eq!(
            backfill_gapfill_links_in(teslacam, &snaps, &autofs, &HashSet::new()),
            (0, 0, 0),
        );
    }

    /// Purge classifies deliberately dangling links by stored category target.
    #[cfg(unix)]
    #[test]
    fn purge_event_links_in_removes_only_event_crosslinks() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let recents = root.path();

        // Event-only dates are removed after their legacy links.
        let only_events = recents.join("2026-05-18");
        std::fs::create_dir_all(&only_events).unwrap();
        symlink(
            "/backingfiles/snapshots/snap-000005/mnt/TeslaCam/SentryClips/2026-05-18_17-29-00/2026-05-18_17-29-00-front.mp4",
            only_events.join("2026-05-18_17-29-00-front.mp4"),
        )
        .unwrap();
        symlink(
            "/backingfiles/snapshots/snap-000005/mnt/TeslaCam/SavedClips/2026-05-18_08-12-00/2026-05-18_08-12-00-front.mp4",
            only_events.join("2026-05-18_08-12-00-front.mp4"),
        )
        .unwrap();

        // Continuous links survive alongside removed event links.
        let mixed = recents.join("2026-06-22");
        std::fs::create_dir_all(&mixed).unwrap();
        let continuous = mixed.join("2026-06-22_12-58-00-front.mp4");
        symlink(
            "/backingfiles/snapshots/snap-000068/mnt/TeslaCam/RecentClips/2026-06-22_12-58-00-front.mp4",
            &continuous,
        )
        .unwrap();
        symlink(
            "/backingfiles/snapshots/snap-000068/mnt/TeslaCam/SentryClips/2026-06-22_13-00-00/2026-06-22_13-00-00-front.mp4",
            mixed.join("2026-06-22_13-00-00-front.mp4"),
        )
        .unwrap();

        purge_event_links_in(recents, &HashSet::new()).unwrap();

        assert!(!only_events.exists(), "event-only date folder should be removed");

        assert!(mixed.is_dir(), "driving day should remain");
        let survivors: Vec<String> = std::fs::read_dir(&mixed)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(survivors, vec!["2026-06-22_12-58-00-front.mp4"]);
        assert!(
            std::fs::symlink_metadata(&continuous)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the continuous RecentClips link must be untouched",
        );
    }

    /// Missing roots and regular files are left untouched.
    #[cfg(unix)]
    #[test]
    fn purge_event_links_in_is_safe_on_missing_and_clean_trees() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let recents = root.path().join("RecentClips");

        purge_event_links_in(&recents, &HashSet::new()).unwrap();

        // Regular clips prevent their folder from being pruned.
        let day = recents.join("2026-06-22");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("real.mp4"), b"x").unwrap();
        purge_event_links_in(&recents, &HashSet::new()).unwrap();
        assert!(day.join("real.mp4").exists());
    }

    /// Manifest gap fills survive while ordinary event links are purged.
    #[cfg(unix)]
    #[test]
    fn purge_event_links_in_keeps_gapfill_exempt_links() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let recents = root.path();

        let day = recents.join("2026-07-05");
        std::fs::create_dir_all(&day).unwrap();

        let gap_link = day.join("2026-07-05_16-03-46-front.mp4");
        symlink(
            "/backingfiles/snapshots/snap-000515/mnt/TeslaCam/SentryClips/2026-07-05_16-12-51/2026-07-05_16-03-46-front.mp4",
            &gap_link,
        )
        .unwrap();

        let stray = day.join("2026-07-05_20-00-00-front.mp4");
        symlink(
            "/backingfiles/snapshots/snap-000515/mnt/TeslaCam/SentryClips/2026-07-05_20-00-00/2026-07-05_20-00-00-front.mp4",
            &stray,
        )
        .unwrap();

        let mut gapfill = HashSet::new();
        gapfill.insert("2026-07-05_16-03-46".to_string());

        purge_event_links_in(recents, &gapfill).unwrap();

        assert!(
            std::fs::symlink_metadata(&gap_link).is_ok(),
            "manifest-flagged driving-hole fill must survive the purge",
        );
        assert!(
            std::fs::symlink_metadata(&stray).is_err(),
            "ordinary event cross-link must still be purged",
        );
    }

    #[test]
    fn snap_component_extracts_exact_names_only() {
        let snaps = Path::new("/backingfiles/snapshots");
        let autofs = Path::new("/tmp/snapshots");
        let owned = |t| owned_snap_component(t, snaps, autofs);

        assert_eq!(
            owned("/backingfiles/snapshots/snap-000508/mnt/TeslaCam/x.mp4"),
            Some("snap-000508"),
        );
        assert_eq!(owned("/tmp/snapshots/snap-000042/TeslaTrackMode/l.mp4"), Some("snap-000042"));

        assert_eq!(owned("/backingfiles/snapshots/snap-000508.bak/mnt/x"), None);
        assert_eq!(owned("/mutable/TeslaCam/RecentClips/a.mp4"), None);
        assert_eq!(owned("snap-/x"), None);
        assert_eq!(owned("/backingfiles/snapshots/snap-000508/snap.bin"), None);
        assert_eq!(owned("/backingfiles/snapshots/snap-000508/mnt"), None);
        assert_eq!(owned("/somewhere/snap-000508/mnt/x.mp4"), None);
        assert_eq!(owned("../snap-000123/x.mp4"), None);
        assert_eq!(owned("/mutable/TeslaCam/RecentClips/snap-000123"), None);
        assert_eq!(owned("/backingfiles/snapshots/snap-000508/mnt/../../../etc/passwd"), None);
        assert_eq!(owned("/backingfiles/snapshots//snap-000508/mnt/x.mp4"), None);
    }

    #[cfg(unix)]
    /// Producer-shaped link-farm fixture spanning two snapshots.
    fn build_farm(
        farm: &std::path::Path,
        snaps: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        use std::os::unix::fs::symlink;
        let day = farm.join("RecentClips/2026-06-27");
        let evt = farm.join("SentryClips/2026-06-27_13-16-28");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::create_dir_all(&evt).unwrap();
        let dead508 = day.join("a-front.mp4");
        symlink(format!("{snaps}/snap-000508/mnt/TeslaCam/RecentClips/a-front.mp4"), &dead508).unwrap();
        let live600 = day.join("b-front.mp4");
        symlink(format!("{snaps}/snap-000600/mnt/TeslaCam/RecentClips/b-front.mp4"), &live600).unwrap();
        let evt_dead = evt.join("c-back.mp4");
        symlink(
            format!("{snaps}/snap-000508/mnt/TeslaCam/SentryClips/2026-06-27_13-16-28/c-back.mp4"),
            &evt_dead,
        )
        .unwrap();
        (dead508, live600, evt_dead)
    }

    #[cfg(unix)]
    #[test]
    fn prune_farm_links_matches_target_string_and_prunes_emptied_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let farm = tmp.path();
        let (dead508, live600, evt_dead) = build_farm(farm, "/backingfiles/snapshots");

        let removed = prune_farm_links(farm, &|_, t| t.contains("/snap-000508/"), &|| true, false);
        assert_eq!(removed, (2, false));
        assert!(std::fs::symlink_metadata(&dead508).is_err());
        assert!(std::fs::symlink_metadata(&evt_dead).is_err());
        assert!(std::fs::symlink_metadata(&live600).is_ok(), "non-matching link must survive");
        assert!(!farm.join("SentryClips/2026-06-27_13-16-28").exists());
        assert!(farm.join("SentryClips").exists());
        assert!(farm.join("RecentClips/2026-06-27").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sweep_removes_only_links_into_missing_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let farm = tmp.path().join("TeslaCam");
        let snaps = tmp.path().join("snapshots");
        let autofs = tmp.path().join("autofs");
        std::fs::create_dir_all(snaps.join("snap-000600")).unwrap();
        let (dead508, live600, evt_dead) = build_farm(&farm, snaps.to_str().unwrap());

        // Foreign target shapes survive even if they name a missing snapshot.
        let day = farm.join("RecentClips/2026-06-27");
        let mut spared = Vec::new();
        for (name, target) in [
            ("d-front.mp4", "/somewhere/else/d-front.mp4".to_string()),
            ("e-front.mp4", "/somewhere/snap-000508/mnt/x.mp4".to_string()),
            ("f-front.mp4", "../snap-000508/mnt/x.mp4".to_string()),
            ("g-front.mp4", format!("{}/snap-000508", snaps.display())),
            ("h-front.mp4", format!("{}/snap-000508/mnt/../../x.mp4", snaps.display())),
        ] {
            let p = day.join(name);
            std::os::unix::fs::symlink(&target, &p).unwrap();
            spared.push(p);
        }

        let counted = sweep_dangling_links_in(&farm, &snaps, &autofs, &|| true, true).unwrap();
        assert_eq!(counted, 2, "dry run must count without deleting");
        assert!(std::fs::symlink_metadata(&dead508).is_ok());

        let removed = sweep_dangling_links_in(&farm, &snaps, &autofs, &|| true, false).unwrap();
        assert_eq!(removed, 2);
        assert!(std::fs::symlink_metadata(&dead508).is_err());
        assert!(std::fs::symlink_metadata(&evt_dead).is_err());
        assert!(std::fs::symlink_metadata(&live600).is_ok());
        for p in &spared {
            assert!(std::fs::symlink_metadata(p).is_ok(), "must not delete {}", p.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn sweep_spares_a_link_that_became_valid_during_the_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let farm = tmp.path().join("TeslaCam");
        let snaps = tmp.path().join("snapshots");
        let autofs = tmp.path().join("autofs");
        std::fs::create_dir_all(snaps.join("snap-000600")).unwrap();
        let (dead508, ..) = build_farm(&farm, snaps.to_str().unwrap());

        // Per-link recheck preserves a concurrently reused slot.
        std::fs::create_dir_all(snaps.join("snap-000508")).unwrap();
        assert_eq!(sweep_dangling_links_in(&farm, &snaps, &autofs, &|| true, false).unwrap(), 0);
        assert!(std::fs::symlink_metadata(&dead508).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn release_prune_spares_links_into_a_reused_snapshot_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let farm = tmp.path().join("TeslaCam");
        let snaps = tmp.path().join("snapshots");
        let autofs = tmp.path().join("autofs");
        std::fs::create_dir_all(snaps.join("snap-000600")).unwrap();
        let (dead508, live600, evt_dead) = build_farm(&farm, snaps.to_str().unwrap());
        let dead = |_: &std::path::Path, t: &str| {
            released_link_is_dead(t, "snap-000508", &snaps, &autofs)
        };

        let (removed, aborted) = prune_farm_links(&farm, &dead, &|| true, false);
        assert_eq!((removed, aborted), (2, false));
        assert!(std::fs::symlink_metadata(&dead508).is_err());
        assert!(std::fs::symlink_metadata(&evt_dead).is_err());
        assert!(std::fs::symlink_metadata(&live600).is_ok());

        // Links not yet visited survive mid-walk slot reuse.
        let farm2 = tmp.path().join("TeslaCam2");
        let (dead508, _, evt_dead) = build_farm(&farm2, snaps.to_str().unwrap());
        let recreated = std::cell::Cell::new(false);
        let guard = || {
            let pruned = std::fs::symlink_metadata(&dead508).is_err()
                || std::fs::symlink_metadata(&evt_dead).is_err();
            if pruned && !recreated.replace(true) {
                std::fs::create_dir_all(snaps.join("snap-000508")).unwrap();
            }
            true
        };
        let (removed, aborted) = prune_farm_links(&farm2, &dead, &guard, false);
        assert_eq!((removed, aborted), (1, false), "only the pre-recreate dir may be pruned");
        let survivors = [&dead508, &evt_dead]
            .iter()
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .count();
        assert_eq!(survivors, 1, "the reused slot's remaining link must survive");
    }

    #[cfg(unix)]
    #[test]
    fn sweep_aborts_when_the_mount_guard_flips() {
        let tmp = tempfile::tempdir().unwrap();
        let farm = tmp.path().join("TeslaCam");
        let snaps = tmp.path().join("snapshots");
        let autofs = tmp.path().join("autofs");
        std::fs::create_dir_all(snaps.join("snap-000600")).unwrap();
        let (dead508, ..) = build_farm(&farm, snaps.to_str().unwrap());

        let err = sweep_dangling_links_in(&farm, &snaps, &autofs, &|| false, false).unwrap_err();
        assert!(err.to_string().contains("mount state changed"));
        assert!(std::fs::symlink_metadata(&dead508).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn sweep_aborts_before_the_next_unlink_when_the_mount_drops_mid_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let farm = tmp.path().join("TeslaCam");
        let snaps = tmp.path().join("snapshots");
        let autofs = tmp.path().join("autofs");
        std::fs::create_dir_all(snaps.join("snap-000600")).unwrap();
        let (dead508, live600, evt_dead) = build_farm(&farm, snaps.to_str().unwrap());

        // A per-directory-only guard would wrongly delete this second link.
        let sibling = farm.join("RecentClips/2026-06-27/z-front.mp4");
        std::os::unix::fs::symlink(
            format!("{}/snap-000508/mnt/TeslaCam/RecentClips/z-front.mp4", snaps.display()),
            &sibling,
        )
        .unwrap();

        let dropped = std::cell::Cell::new(false);
        let deleted = |p: &std::path::Path| std::fs::symlink_metadata(p).is_err();
        let guard = || {
            if deleted(&dead508) || deleted(&sibling) || deleted(&evt_dead) {
                dropped.set(true);
            }
            !dropped.get()
        };

        let err = sweep_dangling_links_in(&farm, &snaps, &autofs, &guard, false).unwrap_err();
        assert!(err.to_string().contains("aborted after 1 link(s)"), "{err}");
        let survivors = [&dead508, &sibling, &evt_dead]
            .iter()
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .count();
        assert_eq!(survivors, 2, "the sweep must stop at the first dropped mount");
        assert!(std::fs::symlink_metadata(&live600).is_ok());
    }

    /// Unreadable snapshot state defers backfill rather than implying release.
    #[cfg(unix)]
    #[test]
    fn backfill_defers_sources_it_cannot_verify() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let teslacam = tmp.path();
        let snaps = tmp.path().join("gone-snapshots");
        let autofs = tmp.path().join("autofs");
        let evt = teslacam.join("SavedClips/2026-07-15_04-59-30");
        std::fs::create_dir_all(&evt).unwrap();
        symlink(
            format!(
                "{}/snap-000005/mnt/TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_04-50-00-front.mp4",
                snaps.display()
            ),
            evt.join("2026-07-15_04-50-00-front.mp4"),
        )
        .unwrap();

        let gapfill: HashSet<String> = ["2026-07-15_04-50-00".to_string()].into_iter().collect();
        assert_eq!(
            backfill_gapfill_links_in(teslacam, &snaps, &autofs, &gapfill),
            (0, 0, 1),
        );
        assert!(
            std::fs::symlink_metadata(
                teslacam.join("RecentClips/2026-07-15/2026-07-15_04-50-00-front.mp4")
            )
            .is_err(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn sweep_bails_when_no_snapshots_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let farm = tmp.path().join("TeslaCam");
        let snaps = tmp.path().join("snapshots");
        std::fs::create_dir_all(&snaps).unwrap();
        let (dead508, ..) = build_farm(&farm, snaps.to_str().unwrap());

        // An empty snapshot root may be unmounted; fail closed.
        let autofs = tmp.path().join("autofs");
        assert!(sweep_dangling_links_in(&farm, &snaps, &autofs, &|| true, false).is_err());
        assert!(std::fs::symlink_metadata(&dead508).is_ok());
    }

    /// Snapshot fixture with optional image mtime and TOC.
    fn mk_snap(base: &std::path::Path, name: &str, bin_mtime: Option<u64>, toc: bool) {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(mtime) = bin_mtime {
            let f = std::fs::File::create(dir.join("snap.bin")).unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
            f.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
        }
        if toc {
            std::fs::File::create(dir.join("snap.bin.toc")).unwrap();
        }
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("sentryusb-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Eviction follows image mtime despite non-monotonic slot names and junk.
    #[test]
    fn list_snapshots_by_age_orders_by_mtime_not_name() {
        let base = scratch("age-order");
        mk_snap(&base, "snap-000000", Some(1_785_300_000), true); // middle age
        mk_snap(&base, "snap-000413", Some(1_786_200_000), true); // newest
        mk_snap(&base, "snap-000414", Some(1_784_600_000), true); // oldest, highest name
        mk_snap(&base, "snap-000500", None, false); // no snap.bin → excluded
        mk_snap(&base, "snap-junk", Some(1_000_000_000), false); // non-numeric → excluded
        std::fs::File::create(base.join("snap-000900")).unwrap(); // a FILE → excluded
        mk_snap(&base, "snap-000202", Some(1_785_900_000), true);
        mk_snap(&base, "snap-000201", Some(1_785_900_000), true);

        let order = list_snapshots_by_age_in(&base);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(
            order,
            vec!["snap-000414", "snap-000000", "snap-000201", "snap-000202", "snap-000413"],
        );
    }

    #[test]
    fn pick_next_slot_distinguishes_only_snap_000000_from_empty() {
        let base = scratch("slot-zero");
        assert_eq!(pick_next_snapshot_slot_with(&base, &|_, _| false).unwrap(), (1, None));

        // snap-000000 is a real prior snapshot, not the empty-state sentinel.
        mk_snap(&base, "snap-000000", Some(1_785_000_000), true);
        let (num, toc) = pick_next_snapshot_slot_with(&base, &|_, _| false).unwrap();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(num, 1);
        assert!(
            toc.as_deref().is_some_and(|t| t.ends_with("snap.bin.toc")),
            "previous TOC must be carried: {:?}",
            toc
        );
    }

    #[test]
    fn pick_next_slot_never_wipes_a_mounted_incomplete_slot() {
        // Never reuse an incomplete slot while its loop image may be mounted.
        let base = scratch("slot-mounted");
        mk_snap(&base, "snap-000010", Some(1_785_000_000), true);
        mk_snap(&base, "snap-000011", Some(1_785_000_100), false); // incomplete

        let (num, toc) = pick_next_snapshot_slot_with(&base, &|_, n| n == "snap-000011").unwrap();
        let still_there = base.join("snap-000011").exists();
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(num, 12, "must append past the mounted slot, not reuse it");
        assert!(still_there, "mounted incomplete slot must NOT be wiped");
        assert!(toc.as_deref().is_some_and(|t| t.contains("snap-000010")));
    }

    #[test]
    fn pick_next_slot_ignores_non_dirs_and_reuses_incomplete_max() {
        let base = scratch("slot-reuse");
        mk_snap(&base, "snap-000010", Some(1_785_000_000), true);
        std::fs::File::create(base.join("snap-000999")).unwrap();
        // Reuse an unmounted incomplete slot and retain the prior TOC.
        mk_snap(&base, "snap-000011", Some(1_785_000_100), false);

        let (num, toc) = pick_next_snapshot_slot_with(&base, &|_, _| false).unwrap();
        let dir_gone = !base.join("snap-000011").exists();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(num, 11, "incomplete max slot is reused");
        assert!(dir_gone, "incomplete snapshot dir is wiped");
        assert!(toc.as_deref().is_some_and(|t| t.contains("snap-000010")));
    }
}
