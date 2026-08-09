//! Snapshot management — reflink-backed copy-on-write captures of the
//! cam disk image, plus the bookkeeping that makes those captures
//! browseable from the iOS app and `/mutable/TeslaCam/`.
//!
//! Ports the bash logic of `Sentry-USB/run/make_snapshot.sh` end-to-end.
//! Earlier the Rust impl only did the `cp --reflink` and skipped:
//!
//!   * fsck of the snapshot image (so `nofsck` had no meaning)
//!   * waiting for autofs to be active before symlinking through it
//!   * generating + diffing a TOC of clip filenames so identical
//!     snapshots get discarded instead of accumulating
//!   * the explicit `<snapdir>/mnt` symlink that lets per-clip symlinks
//!     resolve before the first autofs trigger
//!   * walking RecentClips / SavedClips / SentryClips / TeslaTrackMode
//!     and creating per-clip + per-event symlinks under
//!     `/mutable/TeslaCam/...` (this is the bit drive-map and the
//!     iOS app actually read)
//!   * rebuilding the lot when `/mutable/.rebuild_snapshot_symlinks`
//!     is set (post-setup-re-run recovery)
//!
//! Without the symlink work, `archiveloop` logs
//!   `[drive-map] RecentClips directory not found at /mutable/TeslaCam/RecentClips, skipping`
//! every cycle and the iOS app sees an empty timeline.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use tracing::{info, warn};

/// Filesystem carrying the snapshots; the sweep refuses to judge links
/// unless this is genuinely mounted.
const BACKINGFILES: &str = "/backingfiles";
const SNAPSHOTS_DIR: &str = "/backingfiles/snapshots";

/// Where autofs mounts a snapshot's image. `<SNAPSHOTS_DIR>/snap-NNNNNN/mnt`
/// is a symlink here, and TrackMode links point at it directly.
const AUTOFS_SNAPSHOTS: &str = "/tmp/snapshots";
const CAM_DISK: &str = "/backingfiles/cam_disk.bin";
const REBUILD_FLAG: &str = "/mutable/.rebuild_snapshot_symlinks";

/// Persistent marker gating the one-time purge of legacy Saved/Sentry
/// cross-links out of `RecentClips/`. Present ⇒ the sweep already ran on
/// this device; we never re-run it (the linker no longer creates such
/// links, so there is nothing new to clean).
const PURGE_MARKER: &str = "/mutable/.recentclips_events_purged";

const TESLACAM: &str = "/mutable/TeslaCam";

/// Manifest of Saved/Sentry event clips that fill a genuine RecentClips
/// recording hole: driving pre-roll the drive-map processor spliced into
/// a drive (routes table), plus interior-hole clips a user save moved out
/// of RecentClips regardless of driving state (the processor's ungated
/// scan). One `YYYY-MM-DD_HH-MM-SS` timestamp per line. The drives crate
/// rewrites it on every process pass (self-healing). We consult it here
/// so those clips — and ONLY those — are cross-linked back into
/// RecentClips (for continuous playback) and exempted from the purge.
/// Everything else stays out of RecentClips, so Chad's dedup (parked
/// events no longer flood the Recent tab) is preserved — a manifest clip
/// can never double-list, its minute is missing from RecentClips by
/// construction.
const GAPFILL_MANIFEST: &str = "/mutable/.gapfill_recent_links";

/// Marker holding the manifest content [`backfill_gapfill_links`] last
/// applied. `make_links_for_snapshot` only runs when a snapshot is
/// CREATED, so a stamp added to the manifest later (the processor
/// rediscovering an old hole) never gets its RecentClips link if the
/// footage survives only in already-linked snapshots. The backfill pass
/// closes that: it re-runs once each time the manifest content differs
/// from this marker, then goes quiet.
const GAPFILL_APPLIED_MARKER: &str = "/mutable/.gapfill_links_applied";

/// Load the gap-fill manifest into a set of clip timestamps. Missing or
/// unreadable ⇒ empty set (no cross-links, no exemptions — exactly the
/// pre-manifest behaviour), so this is safe on boards that never had a
/// drive-data gap.
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

/// The `YYYY-MM-DD_HH-MM-SS` timestamp prefix of a clip filename, shared
/// by every camera angle of the same minute. Matching on this (not the
/// full basename) means one manifest entry cross-links all cameras of
/// that minute together.
fn clip_stamp(name: &str) -> Option<&str> {
    if name.len() >= 19 && looks_like_dated_clip(name) {
        Some(&name[..19])
    } else {
        None
    }
}

/// Create a snapshot of the cam disk plus all the symlink/TOC work the
/// car-touchscreen + drive-map UI need.
///
/// `skip_fsck` corresponds to the `nofsck` arg the bash wrapper used to
/// pass after a reboot to avoid running fsck twice in quick succession.
///
/// Returns `Some(name)` on a fresh snapshot, `None` when the new snapshot
/// is byte-equivalent to the previous one (in which case we delete the
/// reflink to avoid accumulating identical copies).
pub async fn make_snapshot(skip_fsck: bool) -> Result<Option<String>> {
    // Serializes against the sweep and the other producers; nothing below
    // re-acquires it.
    let _lock = lock_snapshots_dir()?;

    if !Path::new(CAM_DISK).exists() {
        bail!("cam disk image not found at {}", CAM_DISK);
    }

    // ── pick the next snap-NNNNNN slot ────────────────────────────────
    // If the previous snapshot has no `.toc` it was abandoned mid-flight
    // — wipe it and reuse the slot.
    let (snap_num, prev_toc) = pick_next_snapshot_slot()?;
    let snap_name = format!("snap-{:06}", snap_num);
    let snap_dir = format!("{}/{}", SNAPSHOTS_DIR, snap_name);
    let snap_file = format!("{}/snap.bin", snap_dir);
    let snap_mnt = format!("/tmp/snapshots/{}", snap_name);
    let snap_mnt_link = format!("{}/mnt", snap_dir);

    std::fs::create_dir_all(&snap_dir)?;
    info!("Taking snapshot of cam disk in {}", snap_dir);

    // ── reflink copy (bash line 313) ──────────────────────────────────
    // `--reflink=auto` so non-XFS backingfiles (rare — setup wizard XFS
    // verify usually catches this) still works at the cost of a full copy.
    // Low I/O priority: the copy runs while the car may be writing dashcam
    // footage to the same disk through the gadget; at default priority it
    // can stall those writes past the car's SCSI timeout. Best-effort lowest
    // (-c2 -n7) rather than idle (-c3) so the copy still makes progress under
    // continuous sentry writes (needs the bfq scheduler to have effect).
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

    // ── optional fsck on the loop-mounted partition (bash 281-289) ────
    if !skip_fsck {
        if let Err(e) = fsck_snapshot(&snap_file).await {
            warn!("fsck on {} failed (non-fatal): {}", snap_file, e);
        }
    }

    // ── 32-bit Bookworm timestamp fix (bash 292-299) ──────────────────
    if cfg!(target_pointer_width = "32") {
        let _ = apply_bookworm_32bit_timestamp_fix(&snap_file).await;
    }

    // ── wait for autofs (bash 301-305) ────────────────────────────────
    // Symlinks we're about to create resolve through /tmp/snapshots/...
    // which is the autofs mount root. autofs needs to be active before
    // we touch the path or `find` below would just see an empty dir.
    wait_for_autofs().await;

    info!("Took snapshot {}", snap_name);

    // ── generate TOC for the freshly mounted snapshot (bash 309) ──────
    // Touch the autofs path first so the disk image is mounted before
    // `find` traverses it.
    let _ = sentryusb_shell::run("ls", &[&format!("{}/", snap_mnt)]).await;

    let toc_path = format!("{}.toc", snap_file);
    let toc_path_tmp = format!("{}_", toc_path);
    if let Err(e) = generate_toc(&snap_mnt, &toc_path_tmp).await {
        warn!("toc generation failed for {}: {}", snap_mnt, e);
    }

    // ── diff against previous snapshot's TOC (bash 310-311) ───────────
    // If nothing new is in this snapshot vs. the prior one, this is a
    // duplicate — release it and return None so callers don't think
    // they got a fresh snapshot.
    let is_new = match prev_toc.as_ref() {
        Some(prev) => toc_has_additions(prev, &toc_path_tmp).unwrap_or(true),
        None => true,
    };

    let is_duplicate = run_link_maintenance_before_duplicate_check(is_new, || {
        // Covers manifest stamps whose footage survives only in snapshots
        // linked before the stamp existed. This must precede the duplicate
        // return: an idle cam disk can still have a newly changed manifest.
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

    // The car's firmware auto-deletes Sentry events when the cam disk
    // fills, which is indistinguishable from a user deletion via the
    // touchscreen viewer. We used to mirror those deletions into the
    // snapshot symlinks; that was wrong — it threw away the very events
    // snapshots exist to preserve. Don't sync deletions either way.

    // ── Pre-create the <snapdir>/mnt symlink (bash 317) ───────────────
    // make_links_for_snapshot links each clip with a target like
    // <snapdir>/mnt/TeslaCam/...  ; if the symlink doesn't exist yet
    // those per-clip symlinks resolve to nothing until autofs gets
    // poked, which is fragile. Create it explicitly.
    if !Path::new(&snap_mnt_link).exists() {
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink(&snap_mnt, &snap_mnt_link);
    }

    // ── build /mutable/TeslaCam/... symlinks (bash 318) ───────────────
    if let Err(e) = make_links_for_snapshot(&snap_mnt, &snap_mnt_link) {
        warn!("make_links_for_snapshot failed: {}", e);
    }

    // ── commit the TOC (bash 319) ─────────────────────────────────────
    let _ = std::fs::rename(&toc_path_tmp, &toc_path);

    // ── rebuild-all if the flag file is present (bash 336-339) ────────
    if Path::new(REBUILD_FLAG).exists() {
        if let Err(e) = rebuild_all_snapshot_links() {
            warn!("rebuild_all_snapshot_links: {}", e);
        }
        let _ = std::fs::remove_file(REBUILD_FLAG);
    }

    // ── one-time purge of legacy Saved/Sentry cross-links from RecentClips ─
    // Self-guarded by a persistent marker so it runs once per device after
    // the update that stopped creating them, then never again.
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

/// Normalize a snapshot identifier to its bare `snap-NNNNNN` name.
///
/// Callers pass either a bare name (`snap-000001`, e.g. from autofs) or a
/// full path under the snapshots dir (`/backingfiles/snapshots/snap-000001`,
/// e.g. the WebUI delete handler and `make_snapshot.sh`'s discard path). We
/// take the final path component so every form works. Taking the basename
/// also neutralizes any `..` traversal in the input — only the last
/// component is ever used, then appended to `SNAPSHOTS_DIR`.
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

/// Body of [`release_snapshot`] for callers already holding the snapshots-dir
/// lock — taking it a second time in-process would deadlock against itself.
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
    // Parity with bash release_snapshot.sh: drop every /mutable/TeslaCam symlink
    // whose stored target points into this snapshot, then prune event/date dirs the
    // removal emptied. Skipping it leaks broken links into the pending count.
    let pruned = prune_links_into(&name);
    if pruned > 0 {
        info!("Pruned {} TeslaCam link(s) into {}", pruned, name);
    }
    info!("Released snapshot: {}", name);
    Ok(())
}

/// Remove `/mutable/TeslaCam` symlinks targeting `snap_name`. Matches the
/// stored target string only — never resolves a link, so autofs is never
/// triggered.
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

/// Release-prune predicate. Producer-shaped targets naming `snap_name` only —
/// a bare substring match would also delete foreign links — and only while
/// that snapshot is provably gone, so links into a reused slot survive.
fn released_link_is_dead(target: &str, snap_name: &str, snapshots: &Path, autofs: &Path) -> bool {
    owned_snap_component(target, snapshots, autofs) == Some(snap_name)
        && matches!(snapshot_state(snapshots, snap_name), SnapState::Gone)
}

/// Guard both prune paths re-check: the snapshots filesystem must stay
/// mounted for the whole walk, or every remaining link looks dead.
fn backingfiles_mounted() -> bool {
    is_mounted(Path::new(BACKINGFILES))
}

/// Walk the farm (depth-capped), delete symlinks whose stored target `dead` matches,
/// then remove dirs the deletion emptied, never the top-level category dirs.
/// `guard` re-checked per dir AND immediately before each unlink aborts the
/// walk outright; returns `(removed, aborted)`.
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
                        // `dead` reads the snapshots dir, so an unmount since the
                        // last check turns a healthy link into a false positive.
                        // Re-prove immediately before acting on this one link.
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
                // depth >= 1 here means `path` sits at farm depth >= 2
                // (date/event dirs); rmdir fails harmlessly if non-empty.
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

/// Re-read the link and re-run `dead` right before unlinking, so a link a
/// concurrent relink turned live survives. Residual window: the unlink is
/// path-based, so a relink between this readlink and it still loses.
fn unlink_if_still_dead(link: &Path, target: &str, dead: &dyn Fn(&Path, &str) -> bool) -> bool {
    let Ok(fresh) = std::fs::read_link(link) else { return false };
    let fresh = fresh.to_string_lossy().to_string();
    fresh == target && dead(link, &fresh) && std::fs::remove_file(link).is_ok()
}

/// Delete farm symlinks whose target's `snap-NNNNNN` component no longer exists
/// under the snapshots dir. String + directory-existence checks only; the link is
/// never resolved. Returns how many were (with `dry_run`, would be) removed.
pub fn sweep_dangling_links(dry_run: bool) -> Result<usize> {
    // A stale snap-* dir left on the root fs under an unmounted (or
    // mid-remount, see storage_repair) /backingfiles would satisfy every
    // later check while the real targets are gone. Prove the mount first.
    if !is_mounted(Path::new(BACKINGFILES)) {
        bail!("{} is not a mount point — sweep skipped", BACKINGFILES);
    }
    // Same flock the bash and Rust producers hold, so no producer can relink
    // between this walk's re-verify and its unlink.
    let _lock = lock_snapshots_dir()?;
    // A mounted archive overlay reads the farm as its lower dir; changing
    // the lower under an active overlay is undefined. Skip and retry later.
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

/// [`sweep_dangling_links`] over explicit roots (testable). `guard` is
/// re-checked throughout the walk; a false reading aborts it.
fn sweep_dangling_links_in(
    farm: &Path,
    snapshots: &Path,
    autofs: &Path,
    guard: &dyn Fn() -> bool,
    dry_run: bool,
) -> Result<usize> {
    // Refuse to judge links when no snapshots are visible at all: an
    // unmounted /backingfiles would make every farm link look dead.
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
        // Only links this codebase minted, and only when their snapshot is
        // provably gone — an unreadable snapshots dir proves nothing.
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

/// Classify `snapshots/<name>`. I/O errors other than NotFound — and a
/// NotFound under an unreadable snapshots dir — are Unknown, never Gone.
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

/// Snapshot name of a target in the two shapes this file's linkers mint:
/// `<snapshots>/snap-NNNNNN/mnt/<file…>` and `<autofs>/snap-NNNNNN/<file…>`.
/// Anything else yields None so the sweep leaves it alone.
fn owned_snap_component<'a>(target: &'a str, snapshots: &Path, autofs: &Path) -> Option<&'a str> {
    // Absolute and lexically clean only: `..`, `.` or `//` can walk a
    // producer-looking prefix back out to an unrelated file.
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
        // Retargeted links always go through the snapshot's `mnt` symlink.
        return (parts.next() == Some("mnt") && parts.next().is_some()).then_some(name);
    }
    let mut parts = under(autofs)?.split('/');
    let name = parts.next().filter(is_snap)?;
    parts.next().is_some().then_some(name)
}

/// True when `path` is a mount point. A readable /proc/mounts is the whole
/// answer — no entry means NOT mounted. The st_dev comparison runs only
/// when /proc/mounts is unreadable.
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
        // symlink_metadata, not metadata: a symlinked path lands on another
        // device and would read as mounted while nothing is mounted here.
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

/// Advisory flock on the snapshots dir — the same lock `flock(1)` takes for
/// bash make_snapshot.sh / manage_free_space.sh, so every producer and the
/// sweep serialize on one lock. Bounded wait, then bails without acting.
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

/// Snapshot names ordered by ACTUAL AGE — `snap.bin` mtime ascending,
/// name as tie-break — for eviction. Slot names are NOT time-monotonic
/// in the field (a reflash can leave a stale high-numbered snapshot
/// above a restarted sequence — real device: snap-000414 from Jul 9
/// over snap-000413 from Aug 8), so releasing in name order can delete
/// newer footage while sparing the genuinely oldest snapshot.
///
/// Only physical `snap-<numeric>` directories that contain a readable
/// `snap.bin` qualify: dirs without one hold no reclaimable footage,
/// and an unreadable mtime fails closed (excluded) rather than being
/// nominated for deletion with epoch-zero age.
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
            // file_type() reads the dirent and does NOT follow symlinks:
            // a planted `snap-NNNNNN` symlink pointing elsewhere must
            // never become an eviction candidate.
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

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

/// Find the next free `snap-NNNNNN` slot. If the previous snapshot
/// looks abandoned (no `.toc` file, snap.bin missing), reuse its
/// number — bash matches this behaviour around line 295-300.
///
/// Returns `(snap_num, Option<previous_toc_path>)`. The previous TOC
/// is `None` on a brand-new install (no completed snapshots yet).
fn pick_next_snapshot_slot() -> Result<(u32, Option<String>)> {
    pick_next_snapshot_slot_in(Path::new(SNAPSHOTS_DIR))
}

/// True when anything is mounted inside `<base>/<name>` or at that
/// snapshot's autofs mount (`/tmp/snapshots/<name>`). Prefix match, not
/// the exact-mountpoint [`is_mounted`]: the loop image mounts *under*
/// the snapshot dir, so an exact compare would miss it.
fn snapshot_slot_has_mounts(base: &Path, name: &str) -> bool {
    let under = format!("{}/{}/", base.to_string_lossy(), name);
    let autofs = format!("/tmp/snapshots/{}", name);
    let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
        // Can't prove it's unmounted → assume it is mounted and skip the
        // destructive reuse. Losing one slot number is free; wiping a
        // live mount is not.
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
    // Option, not a 0 sentinel: "only snap-000000 exists" used to be
    // indistinguishable from "no snapshots at all", which skipped the
    // identical-snapshot TOC compare against a real snap-000000 (bash
    // numbering starts at 0, so bash-era stores hit this).
    let mut max_num: Option<u32> = None;
    if let Ok(entries) = std::fs::read_dir(base) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            // Physical numeric dirs only — `snap-000508.bak` files or
            // stray non-directories must not drive slot allocation.
            let num = name
                .strip_prefix("snap-")
                .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|s| s.parse::<u32>().ok());
            if let Some(num) = num
                && entry.path().is_dir()
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

    // Abandoned: no TOC was committed → reuse this slot.
    if !prev_toc.exists() || !prev_bin.exists() {
        // ...unless something is still mounted under it (a stuck autofs
        // mount, or a crash mid-snapshot while the loop image is live).
        // `remove_dir_all` would race a live mount and could tear down
        // footage the archive is still reading, so append past it
        // instead — parity with make_snapshot.sh's guard.
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

/// fsck the snapshot's filesystem partition via a temporary loop device.
/// Mirrors bash lines 281-289. Failures are logged but non-fatal —
/// `archive-clips` will still run; we'd rather lose strict verification
/// of one snapshot than abort the whole archive cycle.
async fn fsck_snapshot(snap_file: &str) -> Result<()> {
    let loop_dev = losetup_find_show(snap_file).await?;
    let part = format!("{}p1", loop_dev);

    // `-p` works for both vfat and exfat. Output goes to stderr; we
    // surface a non-zero exit but don't propagate it.
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(120),
        "fsck",
        &[&part, "--", "-p"],
    )
    .await;

    let _ = sentryusb_shell::run("losetup", &["-d", &loop_dev]).await;
    Ok(())
}

/// Wrapper around `losetup -f -P --show <file>` with a small retry
/// loop, mirroring `losetup_find_show` in
/// `Sentry-USB/setup/pi/envsetup.sh:232-254`. Some kernels race on
/// the partition probe and return a device that isn't ready yet.
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

/// Wait for autofs to be active before we hand it work. Capped at
/// 30 retries (~30s) so a misconfigured system doesn't hang archive
/// indefinitely.
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

/// Run `find <root> -type f -printf '%s %P\n'` and write the result to
/// `out_path`. Format is `<size> <relative-path>` per line, matching the
/// bash TOC produced at line 309.
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
    // Just single-quote: snap paths are well-known and don't contain quotes.
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Returns true if `new_toc` has any line that isn't in `old_toc`.
/// Mirrors the bash `diff old new | grep -qe '^>'` check at line 310.
/// Lines are `<size> <path>`, compared whole: a clip that merely grew
/// (same name, new size — e.g. it was still being written during the
/// previous snapshot) must count as new, otherwise the fuller copy gets
/// discarded as a duplicate.
fn toc_has_additions(old_toc: &str, new_toc: &str) -> Result<bool> {
    let old = std::fs::read_to_string(old_toc).unwrap_or_default();
    let new = std::fs::read_to_string(new_toc)?;
    let old_set: std::collections::HashSet<&str> =
        old.lines().filter(|l| !l.is_empty()).collect();
    Ok(new
        .lines()
        .any(|line| !line.is_empty() && !old_set.contains(line)))
}

/// Build `/mutable/TeslaCam/{RecentClips,SavedClips,SentryClips,TeslaTrackMode}`
/// symlinks pointing into the snapshot mount.
///
/// `cur_mnt` is `/tmp/snapshots/snap-NNNNNN` (autofs path used during
/// initial scan). `final_mnt` is `<snapdir>/mnt` — the symlink to the
/// autofs path. We retarget per-clip symlinks to use `final_mnt` so they
/// keep working even if the autofs path is unmounted later.
fn make_links_for_snapshot(cur_mnt: &str, final_mnt: &str) -> Result<()> {
    let saved = format!("{}/SavedClips", TESLACAM);
    let sentry = format!("{}/SentryClips", TESLACAM);
    let track = format!("{}/TeslaTrackMode", TESLACAM);
    let _ = std::fs::create_dir_all(&saved);
    let _ = std::fs::create_dir_all(&sentry);

    // Timestamps of event clips that fill a genuine driving hole — these
    // (and only these) also get cross-linked into RecentClips below so the
    // drive plays back continuously. Empty on boards with no gap.
    let gapfill = load_gapfill_stamps();

    info!("Making links for {}, retargeted to {}", cur_mnt, final_mnt);

    // RecentClips: flat directory; date-bucket each file under YYYY-MM-DD.
    let recents_root = format!("{}/TeslaCam/RecentClips", cur_mnt);
    if let Ok(entries) = std::fs::read_dir(&recents_root) {
        for entry in entries.flatten() {
            link_clip_into_recents(&entry.path(), cur_mnt, final_mnt);
        }
    }

    // SavedClips: nested event folders.
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
                    // Event clips are linked into their SavedClips event
                    // folder ONLY — deliberately NOT cross-linked into
                    // RecentClips, so the Recent tab stays limited to genuine
                    // continuous footage instead of double-listing events.
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
                    // Exception: a clip the drive-map flagged as filling a
                    // driving hole IS cross-linked into RecentClips, so the
                    // drive's video is continuous. Scoped to the manifest —
                    // parked-event clips never qualify.
                    maybe_gapfill_recent_link(&clip.path(), cur_mnt, final_mnt, &gapfill);
                }
            }
        }
    }

    // SentryClips: nested event folders, same shape as SavedClips.
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
                    // SentryClips event folder ONLY, never RecentClips (see
                    // the SavedClips loop above for the rationale).
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
                    // Scoped RecentClips cross-link for driving-hole fills
                    // (see the SavedClips loop for the rationale).
                    maybe_gapfill_recent_link(&clip.path(), cur_mnt, final_mnt, &gapfill);
                }
            }
        }
    }

    // TrackMode: flat directory, NO retarget (matches bash line 102).
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

/// `linksnapshotfiletorecents` (bash lines 25-43). Drops a per-clip
/// symlink under `/mutable/TeslaCam/RecentClips/<YYYY-MM-DD>/`.
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

/// Cross-link an event clip into `RecentClips/<date>/` IFF its timestamp
/// is in the gap-fill manifest — the drive-map flagged it as filling a
/// genuine driving hole. Same link shape as [`link_clip_into_recents`]
/// (retargeted symlink under the day bucket), so the Viewer and drive
/// player treat it as continuous footage. No-op for every other event
/// clip, which keeps parked-event footage out of RecentClips.
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

/// Backfill RecentClips cross-links for manifest stamps whose clips only
/// exist in already-linked snapshots (see [`GAPFILL_APPLIED_MARKER`]).
/// Walks the existing `/mutable/TeslaCam/{SavedClips,SentryClips}` link
/// tree — no snapshot mounts touched — and creates any missing
/// `RecentClips/<date>/` link for a manifest clip, pointing at the event
/// link's own stored target (single-level `read_link`, so the retargeted
/// `<snapdir>/mnt` path is inherited without resolving through autofs).
/// Runs once per manifest change; no-op otherwise. Never overwrites an
/// existing RecentClips entry.
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
    // Written AFTER the walk so a crash mid-pass re-runs it next snapshot.
    // Withheld while any source was undeterminable: the manifest is the
    // only retry trigger, so marking now would drop it permanently.
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

/// [`backfill_gapfill_links`] over explicit roots (testable). Returns
/// `(links created, dead-target sources skipped, sources needing retry)`.
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
    /// Source link's target snapshot is gone — creating the cross-link
    /// would mint a permanently dangling entry.
    DeadTarget,
    /// Undeterminable source or a failed symlink: leave the manifest
    /// unapplied so a later pass reprocesses this entry.
    Retry,
    Skipped,
}

/// Create the `RecentClips/<date>/` link for one event-tree entry IFF its
/// stamp is in the manifest and no RecentClips entry exists yet. Same
/// link shape as [`maybe_gapfill_recent_link`], but sourced from the
/// already-built event link instead of a snapshot mount.
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
    // Never clobber: an existing entry is either genuine continuous
    // footage or a cross-link a snapshot pass already created.
    if std::fs::symlink_metadata(&link).is_ok() {
        return BackfillOutcome::Skipped;
    }
    // The event entry is itself a symlink into <snapdir>/mnt/...; reuse
    // its stored target. A plain file (shouldn't occur in this tree)
    // falls back to linking its own path.
    let target = match std::fs::read_link(clip) {
        Ok(t) => t.to_string_lossy().to_string(),
        Err(_) => clip.to_string_lossy().to_string(),
    };
    // The source link may itself be dangling (its snapshot already
    // released); copying its target would mint a born-dead link. Checked
    // by snapshot-dir name only — never resolved, so autofs stays idle.
    match owned_snap_component(&target, snapshots, autofs).map(|s| snapshot_state(snapshots, s)) {
        Some(SnapState::Gone) => return BackfillOutcome::DeadTarget,
        // A transient unmount or metadata error is not proof of release.
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

/// One-time cleanup mirroring the bash `purge_event_links_from_recentclips`.
///
/// Earlier versions cross-linked every Saved/Sentry event clip into
/// `RecentClips/<date>/` in addition to its own event folder, so the Recent
/// tab double-listed events and looked like it held weeks of continuous
/// footage. This removes those stray links: genuine continuous-footage links
/// point at `.../RecentClips/...`, event cross-links point at
/// `.../SavedClips/...` or `.../SentryClips/...`, so we discriminate on the
/// symlink's stored target via a single-level `read_link` (never resolving
/// through the autofs snapshot mount). Date folders left empty afterwards
/// (days that held only events) are pruned. Idempotent: a second run finds
/// nothing to remove.
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
                // A driving-hole fill is a legitimate RecentClips entry even
                // though it targets an event folder — keep it (it's what
                // makes the drive play back continuously). Everything else
                // targeting Saved/Sentry is a stray cross-link.
                if let Some(stamp) = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(clip_stamp)
                {
                    if gapfill.contains(stamp) {
                        continue;
                    }
                }
                // Only symlinks are candidates. Read the stored target
                // without resolving it (symlink_metadata avoids following).
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
        // Drop the date folder if it's now empty (held only events).
        let now_empty = std::fs::read_dir(&date_dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if now_empty {
            let _ = std::fs::remove_dir(&date_dir);
        }
    }
    Ok(())
}

/// Replace `cur_mnt` prefix with `final_mnt` so the symlink target
/// references the stable `<snapdir>/mnt` path rather than the autofs
/// `/tmp/snapshots/...` mount which can come and go.
#[cfg(unix)]
fn retarget_path(file: &Path, cur_mnt: &str, final_mnt: &str) -> String {
    let s = file.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_prefix(cur_mnt) {
        format!("{}{}", final_mnt, stripped)
    } else {
        s
    }
}

/// Match bash regex `^[0-9]{4}-[0-9]{2}-[0-9]{2}.*` (line 32).
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

/// Walk every completed snapshot (one with a `.toc`) and rebuild the
/// `/mutable/TeslaCam/...` symlinks for any whose links have gone
/// missing. Mirrors bash function `rebuild_all_snapshot_links`
/// (lines 163-222).
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

        // Verify the snapshot can mount before we ask make_links to walk it.
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

/// Check whether any symlink under `/mutable/TeslaCam/` already points
/// at this snapshot. Used to skip rebuilds for snapshots that are
/// already linked. Mirrors bash `find -lname "*/${snapname}/*"`
/// (line 195).
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

/// On 32-bit Bookworm (Pi Zero/Zero2/Pi3 + 32-bit userspace) the exFAT
/// driver mis-handles atimes past Y2038, leaving snapshots unfsck-able.
/// Mount the snapshot RW, find files newer-than-2038, touch them to
/// "now", then unmount. Bash lines 292-299.
async fn apply_bookworm_32bit_timestamp_fix(snap_file: &str) -> Result<()> {
    // Bookworm = Debian VERSION_ID="12".
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
        // autofs and a correct WebUI call pass the bare id.
        assert_eq!(normalize_snap_name("snap-000001").as_deref(), Some("snap-000001"));
    }

    #[test]
    fn normalize_accepts_full_path() {
        // The regression: the WebUI delete handler (and make_snapshot.sh's
        // discard path) pass a full path. The old `contains('/')` guard
        // rejected this outright, so deletes failed via the thin-wrapper
        // `release_snapshot.sh` → `sentryusb snapshot release "$@"` route.
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
        // basename takes only the final component, so traversal can't
        // escape SNAPSHOTS_DIR — the final segment isn't a `snap-` name.
        assert_eq!(normalize_snap_name("snap-1/../../etc/passwd"), None);
        assert_eq!(normalize_snap_name("/etc/../snap-1/.."), None);
    }

    /// Backfill creates a RecentClips link for a manifest clip that only
    /// exists in the (already-linked) event tree, reusing the event
    /// link's stored target; non-manifest clips are ignored and existing
    /// RecentClips entries are never clobbered. The pass reads link
    /// strings, never the files behind them.
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
        // In the manifest but already cross-linked → must not be clobbered.
        symlink(
            clip_target("snap-000005", "2026-07-15_04-55-00"),
            evt.join("2026-07-15_04-55-00-front.mp4"),
        )
        .unwrap();
        // Not in the manifest → must stay out of RecentClips.
        symlink(
            clip_target("snap-000005", "2026-07-15_04-58-00"),
            evt.join("2026-07-15_04-58-00-front.mp4"),
        )
        .unwrap();
        // In the manifest, but its snapshot is already released: copying
        // this target would mint a born-dead RecentClips link.
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

        // The missing manifest clip gained a link with the event link's target.
        let made = day.join("2026-07-15_04-50-00-front.mp4");
        assert_eq!(
            std::fs::read_link(&made).unwrap().to_string_lossy(),
            target_a
        );
        // The released-snapshot source was skipped, not propagated.
        assert!(
            std::fs::symlink_metadata(day.join("2026-07-15_04-52-00-front.mp4")).is_err(),
            "must not mint a link into a released snapshot",
        );
        // Existing entry untouched.
        assert_eq!(
            std::fs::read_link(&existing).unwrap().to_string_lossy(),
            "/pre/existing/target.mp4"
        );
        // Non-manifest clip not cross-linked.
        assert!(
            std::fs::symlink_metadata(day.join("2026-07-15_04-58-00-front.mp4")).is_err()
        );
        // Idempotent: second run creates nothing.
        assert_eq!(
            backfill_gapfill_links_in(teslacam, &snaps, &autofs, &gapfill).0,
            0,
        );
        // Empty manifest: no-op even with clips present.
        assert_eq!(
            backfill_gapfill_links_in(teslacam, &snaps, &autofs, &HashSet::new()),
            (0, 0, 0),
        );
    }

    /// The purge keys off each symlink's stored target: links into
    /// `.../SavedClips/...` or `.../SentryClips/...` are the stray event
    /// cross-links to delete; `.../RecentClips/...` links are genuine
    /// continuous footage to keep. Targets are dangling on purpose — the
    /// sweep reads the link string, never the file behind it.
    #[cfg(unix)]
    #[test]
    fn purge_event_links_in_removes_only_event_crosslinks() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let recents = root.path();

        // A day that was ONLY events (like the user's May 18): a Sentry
        // cross-link + a Saved cross-link, both should be removed and the
        // now-empty date folder pruned.
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

        // A driving day: a genuine continuous RecentClips link (must survive)
        // plus a stray Sentry cross-link (must be removed).
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

        // Event-only day pruned entirely.
        assert!(!only_events.exists(), "event-only date folder should be removed");

        // Driving day kept, with ONLY the continuous link remaining.
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

    /// Missing root is a no-op; regular (non-symlink) files are never
    /// touched, and a clean tree is left alone (idempotent).
    #[cfg(unix)]
    #[test]
    fn purge_event_links_in_is_safe_on_missing_and_clean_trees() {
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let recents = root.path().join("RecentClips");

        // Missing root: returns Ok with nothing to do.
        purge_event_links_in(&recents, &HashSet::new()).unwrap();

        // A real (non-symlink) clip file is left alone, and its folder
        // survives because it isn't empty.
        let day = recents.join("2026-06-22");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("real.mp4"), b"x").unwrap();
        purge_event_links_in(&recents, &HashSet::new()).unwrap();
        assert!(day.join("real.mp4").exists());
    }

    /// A gap-fill cross-link (event target, but manifest-flagged as filling
    /// a driving hole) must SURVIVE the purge, while an ordinary event
    /// cross-link on the same day is still removed. This is what keeps the
    /// drive's video continuous without re-flooding the Recent tab.
    #[cfg(unix)]
    #[test]
    fn purge_event_links_in_keeps_gapfill_exempt_links() {
        use std::os::unix::fs::symlink;
        use tempfile::TempDir;

        let root = TempDir::new().unwrap();
        let recents = root.path();

        let day = recents.join("2026-07-05");
        std::fs::create_dir_all(&day).unwrap();

        // Driving-hole fill: targets SentryClips but is in the manifest.
        let gap_link = day.join("2026-07-05_16-03-46-front.mp4");
        symlink(
            "/backingfiles/snapshots/snap-000515/mnt/TeslaCam/SentryClips/2026-07-05_16-12-51/2026-07-05_16-03-46-front.mp4",
            &gap_link,
        )
        .unwrap();

        // Ordinary event cross-link the same day: NOT in the manifest.
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

        // The two shapes the linkers actually mint.
        assert_eq!(
            owned("/backingfiles/snapshots/snap-000508/mnt/TeslaCam/x.mp4"),
            Some("snap-000508"),
        );
        assert_eq!(owned("/tmp/snapshots/snap-000042/TeslaTrackMode/l.mp4"), Some("snap-000042"));

        assert_eq!(owned("/backingfiles/snapshots/snap-000508.bak/mnt/x"), None);
        assert_eq!(owned("/mutable/TeslaCam/RecentClips/a.mp4"), None);
        assert_eq!(owned("snap-/x"), None);
        // Right root, wrong shape: no `mnt` level, or nothing under it.
        assert_eq!(owned("/backingfiles/snapshots/snap-000508/snap.bin"), None);
        assert_eq!(owned("/backingfiles/snapshots/snap-000508/mnt"), None);
        // Foreign roots and relative targets that merely contain a snap name.
        assert_eq!(owned("/somewhere/snap-000508/mnt/x.mp4"), None);
        assert_eq!(owned("../snap-000123/x.mp4"), None);
        assert_eq!(owned("/mutable/TeslaCam/RecentClips/snap-000123"), None);
        // Traversal back out of a producer-owned prefix.
        assert_eq!(owned("/backingfiles/snapshots/snap-000508/mnt/../../../etc/passwd"), None);
        assert_eq!(owned("/backingfiles/snapshots//snap-000508/mnt/x.mp4"), None);
    }

    #[cfg(unix)]
    /// Farm with two links into snap-000508 and one into snap-000600, all
    /// rooted at `snaps` so the sweep sees genuine producer-owned targets.
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
        // Emptied event dir pruned; category dirs and non-empty day dir kept.
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

        // Targets that name a missing snapshot but are NOT shapes this
        // codebase mints: all must survive untouched.
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

        // A concurrent snapshot reusing slot 508 recreates the dir; the
        // per-link re-check must then leave its links alone.
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

        // Slot 508 stays released: both its links go, the other survives.
        let (removed, aborted) = prune_farm_links(&farm, &dead, &|| true, false);
        assert_eq!((removed, aborted), (2, false));
        assert!(std::fs::symlink_metadata(&dead508).is_err());
        assert!(std::fs::symlink_metadata(&evt_dead).is_err());
        assert!(std::fs::symlink_metadata(&live600).is_ok());

        // A new snapshot reuses slot 508 partway through the walk; every link
        // the prune has not yet reached must survive.
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

        // Second dead link in the SAME directory as dead508: a guard checked
        // only once per directory would delete both.
        let sibling = farm.join("RecentClips/2026-06-27/z-front.mp4");
        std::os::unix::fs::symlink(
            format!("{}/snap-000508/mnt/TeslaCam/RecentClips/z-front.mp4", snaps.display()),
            &sibling,
        )
        .unwrap();

        // /backingfiles goes away the instant the first unlink lands.
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

    /// An unreadable snapshots dir makes every source undeterminable: the
    /// backfill must defer them, never treat them as released.
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

        // Empty snapshots dir looks like an unmounted /backingfiles: bail,
        // delete nothing.
        let autofs = tmp.path().join("autofs");
        assert!(sweep_dangling_links_in(&farm, &snaps, &autofs, &|| true, false).is_err());
        assert!(std::fs::symlink_metadata(&dead508).is_ok());
    }

    /// Build `<base>/snap-<name>` with an optional snap.bin (at the given
    /// unix mtime) and optional TOC.
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

    /// Real-device fixture: slot names are not time-monotonic (stale
    /// snap-000414 from Jul 9 above snap-000413 from Aug 8). Eviction
    /// order must follow snap.bin mtimes, with name as tie-break, and
    /// exclude junk (no snap.bin, non-numeric suffix, plain files).
    #[test]
    fn list_snapshots_by_age_orders_by_mtime_not_name() {
        let base = scratch("age-order");
        mk_snap(&base, "snap-000000", Some(1_785_300_000), true); // middle age
        mk_snap(&base, "snap-000413", Some(1_786_200_000), true); // newest
        mk_snap(&base, "snap-000414", Some(1_784_600_000), true); // oldest, highest name
        mk_snap(&base, "snap-000500", None, false); // no snap.bin → excluded
        mk_snap(&base, "snap-junk", Some(1_000_000_000), false); // non-numeric → excluded
        std::fs::File::create(base.join("snap-000900")).unwrap(); // a FILE → excluded
        // Equal-mtime pair: name breaks the tie.
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
        // Empty dir → fresh start at 1, no previous TOC.
        assert_eq!(pick_next_snapshot_slot_with(&base, &|_, _| false).unwrap(), (1, None));

        // Only a COMPLETE snap-000000 (bash numbering starts at 0): the
        // next slot is 1 and its TOC must be carried for the
        // identical-snapshot compare — this used to be conflated with
        // the empty case and skipped the compare.
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
        // A crash mid-snapshot (or a stuck autofs mount) leaves the
        // highest slot incomplete WITH the loop image still mounted
        // under it. Reusing it means remove_dir_all racing a live mount
        // while the archive may still be reading footage from it, so
        // append past it instead — parity with make_snapshot.sh.
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
        // Stray FILE with a higher number must not drive allocation.
        std::fs::File::create(base.join("snap-000999")).unwrap();
        // Incomplete (no TOC) highest dir → wiped and its slot reused,
        // with the previous complete snapshot's TOC as backstop.
        mk_snap(&base, "snap-000011", Some(1_785_000_100), false);

        let (num, toc) = pick_next_snapshot_slot_with(&base, &|_, _| false).unwrap();
        let dir_gone = !base.join("snap-000011").exists();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(num, 11, "incomplete max slot is reused");
        assert!(dir_gone, "incomplete snapshot dir is wiped");
        assert!(toc.as_deref().is_some_and(|t| t.contains("snap-000010")));
    }
}
