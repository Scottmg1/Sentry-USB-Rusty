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
//!   * deleting `/mutable/TeslaCam/{Saved,Sentry}Clips/...` symlinks
//!     when the user nukes a clip from the car's touchscreen viewer
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

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use tracing::{info, warn};

const SNAPSHOTS_DIR: &str = "/backingfiles/snapshots";
const CAM_DISK: &str = "/backingfiles/cam_disk.bin";
const REBUILD_FLAG: &str = "/mutable/.rebuild_snapshot_symlinks";

const TESLACAM: &str = "/mutable/TeslaCam";

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
    let _ = std::fs::create_dir_all(SNAPSHOTS_DIR);

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
    let cp_result = sentryusb_shell::run_with_timeout(
        Duration::from_secs(600),
        "cp",
        &["--reflink=auto", CAM_DISK, &snap_file],
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

    if !is_new {
        info!("Snapshot {} identical to previous; discarding", snap_name);
        let _ = std::fs::remove_file(&toc_path_tmp);
        let _ = std::fs::remove_file(&snap_file);
        let _ = std::fs::remove_dir_all(&snap_dir);
        return Ok(None);
    }

    // ── cleanup symlinks for clips deleted on the car (bash 313-316) ──
    if let Some(prev) = prev_toc.as_ref() {
        if let Err(e) = cleanup_deleted_clips(prev, &toc_path_tmp) {
            warn!("cleanup_deleted_clips: {}", e);
        }
    }

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

    Ok(Some(snap_name))
}

/// Release (delete) a snapshot.
pub async fn release_snapshot(snap_name: &str) -> Result<()> {
    if snap_name.contains("..") || snap_name.contains('/') {
        bail!("invalid snapshot name");
    }

    let snap_dir = format!("{}/{}", SNAPSHOTS_DIR, snap_name);
    if !Path::new(&snap_dir).exists() {
        bail!("snapshot not found: {}", snap_name);
    }

    let mnt_dir = format!("{}/mnt", snap_dir);
    if Path::new(&mnt_dir).exists() {
        let _ = sentryusb_shell::run("umount", &[&mnt_dir]).await;
    }

    std::fs::remove_dir_all(&snap_dir)?;
    info!("Released snapshot: {}", snap_name);
    Ok(())
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
    let mut max_num: u32 = 0;
    if let Ok(entries) = std::fs::read_dir(SNAPSHOTS_DIR) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num_str) = name.strip_prefix("snap-") {
                if let Ok(num) = num_str.parse::<u32>() {
                    if num > max_num {
                        max_num = num;
                    }
                }
            }
        }
    }

    if max_num == 0 {
        return Ok((1, None));
    }

    let prev_dir = format!("{}/snap-{:06}", SNAPSHOTS_DIR, max_num);
    let prev_toc = format!("{}/snap.bin.toc", prev_dir);
    let prev_bin = format!("{}/snap.bin", prev_dir);

    // Abandoned: no TOC was committed → reuse this slot.
    if !Path::new(&prev_toc).exists() || !Path::new(&prev_bin).exists() {
        let _ = std::fs::remove_dir_all(&prev_dir);
        let next = max_num;
        // Look one further back for a usable previous TOC.
        let backstop = if next > 1 {
            let p = format!("{}/snap-{:06}/snap.bin.toc", SNAPSHOTS_DIR, next - 1);
            if Path::new(&p).exists() { Some(p) } else { None }
        } else {
            None
        };
        return Ok((next, backstop));
    }

    Ok((max_num + 1, Some(prev_toc)))
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

/// Returns true if `new_toc` has any path that isn't in `old_toc`.
/// Mirrors the bash `diff old new | grep -qe '^>'` check at line 310.
fn toc_has_additions(old_toc: &str, new_toc: &str) -> Result<bool> {
    let old = std::fs::read_to_string(old_toc).unwrap_or_default();
    let new = std::fs::read_to_string(new_toc)?;
    let old_set: std::collections::HashSet<&str> = old
        .lines()
        .map(|l| l.split_once(' ').map(|x| x.1).unwrap_or(""))
        .filter(|s| !s.is_empty())
        .collect();
    for line in new.lines() {
        if let Some((_, path)) = line.split_once(' ') {
            if !path.is_empty() && !old_set.contains(path) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Remove `/mutable/TeslaCam/{Saved,Sentry}Clips/...` symlinks for clips
/// the user removed via the car's touchscreen viewer.
///
/// RecentClips intentionally NOT cleaned up here — that's how the bash
/// version works (line 137 comment). RecentClips refresh on the next
/// archive cycle anyway.
fn cleanup_deleted_clips(old_toc: &str, new_toc: &str) -> Result<()> {
    let old = std::fs::read_to_string(old_toc).unwrap_or_default();
    let new = std::fs::read_to_string(new_toc)?;

    let new_set: std::collections::HashSet<&str> = new
        .lines()
        .map(|l| l.split_once(' ').map(|x| x.1).unwrap_or(""))
        .filter(|s| !s.is_empty())
        .collect();

    let mut count = 0usize;
    for line in old.lines() {
        let relpath = match line.split_once(' ') {
            Some((_, p)) if !p.is_empty() => p,
            _ => continue,
        };
        if new_set.contains(relpath) {
            continue;
        }
        if !(relpath.starts_with("TeslaCam/SavedClips/")
            || relpath.starts_with("TeslaCam/SentryClips/"))
        {
            continue;
        }
        let target = format!("/mutable/{}", relpath);
        let target_path = Path::new(&target);
        if target_path.symlink_metadata().is_ok() {
            // is_ok() catches both symlink-to-file and dangling symlink.
            let _ = std::fs::remove_file(&target);
            count += 1;
            if let Some(parent) = target_path.parent() {
                let _ = std::fs::remove_dir(parent); // empty-only
            }
        }
    }

    if count > 0 {
        info!("Removed {} symlink(s) for clips deleted from car's viewer", count);
    }
    Ok(())
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
                    link_clip_into_recents(&clip.path(), cur_mnt, final_mnt);
                    let target = retarget_path(&clip.path(), cur_mnt, final_mnt);
                    let link = format!(
                        "{}/{}",
                        evt_dest,
                        clip.file_name().to_string_lossy()
                    );
                    let _ = std::fs::remove_file(&link);
                    #[cfg(unix)]
                    let _ = std::os::unix::fs::symlink(&target, &link);
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
                    link_clip_into_recents(&clip.path(), cur_mnt, final_mnt);
                    let target = retarget_path(&clip.path(), cur_mnt, final_mnt);
                    let link = format!(
                        "{}/{}",
                        evt_dest,
                        clip.file_name().to_string_lossy()
                    );
                    let _ = std::fs::remove_file(&link);
                    #[cfg(unix)]
                    let _ = std::os::unix::fs::symlink(&target, &link);
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
    let target = retarget_path(file, cur_mnt, final_mnt);
    let link = format!("{}/{}", recents, filename);
    let _ = std::fs::remove_file(&link);
    #[cfg(unix)]
    let _ = std::os::unix::fs::symlink(&target, &link);
}

/// Replace `cur_mnt` prefix with `final_mnt` so the symlink target
/// references the stable `<snapdir>/mnt` path rather than the autofs
/// `/tmp/snapshots/...` mount which can come and go.
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
        "find {} -newerat 20380101 | xargs -r touch",
        shell_escape(&tmpmnt)
    );
    let _ = sentryusb_shell::run("bash", &["-c", &cmd]).await;
    let _ = sentryusb_shell::run("umount", &[&tmpmnt]).await;
    let _ = sentryusb_shell::run("rmdir", &[&tmpmnt]).await;
    Ok(())
}

