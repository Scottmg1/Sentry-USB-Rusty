//! Snapshot management API.
//!
//! Snapshots are XFS reflink-backed point-in-time copies of cam_disk
//! that the runtime archiveloop creates on a schedule (default every
//! 58 minutes). They live at `/backingfiles/snapshots/snap-<id>/snap.bin`
//! and consume space on the backingfiles partition.
//!
//! Until the wizard's setup re-run was made data-safe, snapshots were
//! auto-deleted by the runtime's `manage_free_space.sh` and silently
//! wiped by the disk-image setup phase whenever CAM_SIZE changed. With
//! that behavior fixed, users need an explicit way to inspect and
//! delete snapshots when they want to free space (e.g. before growing
//! a drive image past available capacity). This module provides:
//!
//!   * `GET    /api/snapshots`               — list with size/timestamp
//!   * `DELETE /api/snapshots/:id`           — delete one snapshot
//!   * `GET    /api/backingfiles/free-space` — total/used/avail in KB
//!
//! The actual delete shells out to `/root/bin/release_snapshot.sh`
//! (already on disk, used by the runtime free-space manager) so we
//! don't reimplement the careful umount + symlink cleanup it performs.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::router::AppState;

const SNAPSHOTS_DIR: &str = "/backingfiles/snapshots";
const RELEASE_SNAPSHOT_SCRIPT: &str = "/root/bin/release_snapshot.sh";

/// One snapshot entry in the listing response.
#[derive(serde::Serialize)]
struct SnapshotEntry {
    /// `snap-<id>` directory name. Used as the path parameter for delete.
    id: String,
    /// Estimated bytes held only by THIS snapshot — approximately what
    /// deleting it returns. Not an exact `statvfs` delta; see
    /// `sentryusb_gadget::reflink` for what the figure excludes.
    ///
    /// `None` means "not measured yet, or could not be measured" — the UI
    /// must render that as pending/unavailable and NEVER as `0 B`, which
    /// would read as "safe to delete, frees nothing". A measured zero is
    /// legitimate and does render as `0 B`.
    ///
    /// This replaces a `du -sB1` of the snapshot directory. `du` charges
    /// every block a file maps to that file with no notion of XFS reflink
    /// sharing, so each snapshot reported the whole cam-disk block count:
    /// rows of 45-64 GB whose deletion returned ~5 GB, summing to several
    /// times the partition size.
    reclaimable_bytes: Option<u64>,
    /// Unix epoch seconds — directory mtime. Used by the UI to render a
    /// human-friendly date and to sort.
    created_unix: i64,
}

/// How long a measurement stays usable before we re-measure.
const SIZE_MAX_AGE_SECS: u64 = 15 * 60;

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn size_cache() -> &'static std::sync::Mutex<sentryusb_gadget::reflink::SizeCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<sentryusb_gadget::reflink::SizeCache>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(sentryusb_gadget::reflink::SizeCache::new()))
}

static REFRESH_IN_FLIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Clears the in-flight flag even if the measurement panics — otherwise a
/// single panic would wedge sizing for the life of the process.
struct RefreshGuard;
impl Drop for RefreshGuard {
    fn drop(&mut self) {
        REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Measure every snapshot off the request path.
///
/// Walking extent maps costs thousands to millions of records per image,
/// far too much for a request handler — which previously spawned one `du`
/// per snapshot on every page load. At most one refresh runs at a time.
fn spawn_size_refresh(ids: Vec<String>, generation: u64) {
    use std::sync::atomic::Ordering;
    if REFRESH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let _guard = RefreshGuard;
        // Capture the file's identity alongside the size so a directory
        // replaced under the same snapshot id can't inherit the old
        // number. Identity is read AFTER measuring, so a replacement
        // mid-measure yields a value tagged with the new inode and is
        // simply re-measured next time rather than served wrongly.
        let (ok, failed) = sentryusb_gadget::reflink::measure_all_with(&ids, |id| {
            let bin = format!("{}/{}/snap.bin", SNAPSHOTS_DIR, id);
            let p = std::path::Path::new(&bin);
            let bytes = sentryusb_gadget::reflink::exclusive_bytes(p)?;
            Ok((bytes, sentryusb_gadget::reflink::file_identity(p)?))
        });
        for (id, e) in &failed {
            tracing::warn!("could not measure reclaimable size for {}: {}", id, e);
        }
        if let Ok(mut cache) = size_cache().lock() {
            // Publish only if nothing invalidated the cache while we ran —
            // otherwise a delete that landed mid-measurement would be
            // undone by republishing the pre-delete numbers as fresh.
            if !cache.publish(generation, ids, ok, now_secs()) {
                tracing::debug!("discarded a snapshot-size measurement invalidated mid-flight");
            }
        }
    });
}

/// GET /api/snapshots
///
/// Returns the list of snapshot directories under `/backingfiles/snapshots/`.
/// Sorted oldest-first so callers can default to that ordering — the
/// user typically wants to delete the oldest to free space.
pub async fn list_snapshots(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut entries: Vec<SnapshotEntry> = Vec::new();

    let dir = match std::fs::read_dir(SNAPSHOTS_DIR) {
        Ok(d) => d,
        Err(_) => {
            // Directory missing entirely is fine — no snapshots yet.
            return (StatusCode::OK, Json(serde_json::json!({
                "snapshots": entries,
            })));
        }
    };

    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("snap-") {
            continue;
        }
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        // `snap.bin` mtime as the "created" timestamp — the SAME clock
        // free-space eviction and the dashboard date range order by, so
        // "oldest" in this list is the snapshot cleanup would actually
        // drop. Deliberately not the DIRECTORY mtime: autofs writes
        // `snap.bin.opts` into the snapshot dir the first time the image
        // is mounted (run/auto.sentryusb), which stamps the directory
        // with "now" and made a merely-viewed old snapshot look new.
        // Falls back to the directory only when snap.bin is unreadable.
        let created_unix = std::fs::symlink_metadata(path.join("snap.bin"))
            .or_else(|_| entry.metadata())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        entries.push(SnapshotEntry {
            id: name,
            // Filled from the cache below — measuring here would put an
            // extent-map walk per snapshot on the request path.
            reclaimable_bytes: None,
            created_unix,
        });
    }

    // Oldest first by mtime. UI may re-sort, but this default matches
    // what users actually want (delete the oldest to free space).
    entries.sort_by_key(|e| e.created_unix);

    // Fill from the cache, and kick off a refresh when it is missing or
    // stale. A stale figure is worse than none here: it was computed
    // against a different set of snapshots, and deleting any one of them
    // changes every other row's number.
    let now = now_secs();
    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    let (current, computed_at, generation) = match size_cache().lock() {
        Ok(cache) => {
            let current = cache.is_current_for(&ids, now, SIZE_MAX_AGE_SECS);
            if current {
                for e in entries.iter_mut() {
                    // Verify the file is still the one that was measured.
                    let bin = format!("{}/{}/snap.bin", SNAPSHOTS_DIR, e.id);
                    e.reclaimable_bytes =
                        sentryusb_gadget::reflink::file_identity(std::path::Path::new(&bin))
                            .ok()
                            .and_then(|ident| cache.get_if_same(&e.id, ident));
                }
            }
            (current, cache.computed_at(), cache.generation())
        }
        Err(_) => (false, None, 0),
    };
    if !current && !ids.is_empty() {
        spawn_size_refresh(ids, generation);
    }
    // Derived from cache state, NOT from the in-flight flag. Reading that
    // flag here would lose wakeups: the worker can publish and clear it
    // between the cache read above and this line, yielding null sizes with
    // `sizes_pending: false` — the UI would then render "size unavailable"
    // and stop polling although the values had just landed.
    //
    // An all-failure measurement doesn't poll forever either, because the
    // cache treats it as a completed attempt.
    let pending = !current && !entries.is_empty();

    // Reflink-aware aggregate: bytes that would be freed if every snapshot
    // were deleted. `du` is NOT reflink-aware — it dedupes hard links by
    // inode, but each snap.bin is a separate inode whose extents are shared
    // with cam_disk.bin via `cp --reflink=always`. Each snap.bin's
    // `st_blocks` therefore reports the full cam_disk.bin block count, and
    // `du -sB1 /backingfiles/snapshots` (even as a single tree walk) sums
    // those per-file counts — producing N × cam_disk_size, far larger than
    // the partition.
    //
    // Compute the true reflink-exclusive footprint as:
    //     df_used(/backingfiles)  −  du(--exclude=snapshots /backingfiles/)
    // i.e. partition-level used bytes (which counts each allocated extent
    // once, regardless of how many files reference it) minus the apparent
    // footprint of non-snapshot content. Deleting all snapshots leaves only
    // the non-snapshot files (chiefly cam_disk.bin), whose blocks XFS
    // retains, so `df` settles to that du value afterwards — the difference
    // is what the snapshots collectively hold exclusively.
    let total_allocated_bytes: u64 = if entries.is_empty() {
        0
    } else {
        let df_out = sentryusb_shell::run(
            "df", &["--output=used", "--block-size=1", "/backingfiles/"],
        ).await.unwrap_or_default();
        let used_bytes: u64 = df_out
            .lines()
            .last()
            .and_then(|l| l.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let non_snap_out = sentryusb_shell::run(
            "du", &["-sB1", "--exclude=snapshots", "/backingfiles/"],
        ).await.unwrap_or_default();
        let non_snap_bytes: u64 = non_snap_out
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        used_bytes.saturating_sub(non_snap_bytes)
    };

    (StatusCode::OK, Json(serde_json::json!({
        "snapshots": entries,
        "total_allocated_bytes": total_allocated_bytes,
        // Unix seconds the per-row figures were measured, and whether a
        // measurement is running now. The UI needs both to distinguish
        // "measuring" from "could not measure" — neither may render as 0.
        "sizes_computed_at": computed_at,
        "sizes_pending": pending,
    })))
}

/// DELETE /api/snapshots/:id
///
/// Calls `release_snapshot.sh` to umount the snap.bin loop image and
/// remove the directory + dangling /mutable/TeslaCam symlinks. The
/// id must be a `snap-*` name; reject anything else to prevent
/// arbitrary path traversal.
pub async fn delete_snapshot(
    State(_s): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !id.starts_with("snap-") || id.contains('/') || id.contains("..") {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "Invalid snapshot id (expected snap-<digits>)",
        );
    }

    let path = format!("{}/{}", SNAPSHOTS_DIR, id);
    if !std::path::Path::new(&path).is_dir() {
        return crate::json_error(StatusCode::NOT_FOUND, "Snapshot not found");
    }

    // Invoke the release CLI directly, NOT the `release_snapshot.sh` shim
    // and NOT the old `rm -rf` fallback. That fallback bypassed the
    // unmount and loop-device checks entirely — it removed the directory
    // whether or not the image was mounted, so the footage went and the
    // space did not — and a partially-installed device is exactly where
    // it was reachable. Calling the binary skips the shim's existence
    // problem without reintroducing an unsafe path: if the binary is
    // missing the delete fails loudly, which is the correct outcome.
    //
    // Deliberately a SUBPROCESS rather than awaiting
    // `sentryusb_gadget::snapshot::release_snapshot` in-process. That
    // function polls the snapshots flock with a blocking sleep for up to
    // 30s and then does synchronous recursive deletion and link pruning.
    // On a runtime with N workers, N concurrent DELETEs would occupy every
    // worker blocking on the lock, leaving none to drive the request that
    // actually holds it — a self-inflicted deadlock until they time out.
    // The subprocess also keeps a teardown panic from taking the whole
    // API down, since release builds abort on panic.
    // Prefer the on-disk script so we share the runtime's careful
    // umount + symlink cleanup logic. Fall back to a plain rm only if
    // the script is missing (possible on a partially-installed system).
    //
    // Pass the bare `id`, NOT the full path: the Rust-installed
    // `release_snapshot.sh` is a thin shim that forwards "$@" to
    // `sentryusb snapshot release`, which expects a `snap-NNNNNN` name.
    // The id is already validated above, and `release_snapshot` now also
    // accepts a full path, so this is robust across both the thin-wrapper
    // and full-script installs.
    let script_exists = std::path::Path::new(RELEASE_SNAPSHOT_SCRIPT).exists();
    let result = if script_exists {
        sentryusb_shell::run(RELEASE_SNAPSHOT_SCRIPT, &[id.as_str()]).await
    } else {
        sentryusb_shell::run("rm", &["-rf", &path]).await
    };

    match result {
        Ok(_) => {
            // Every other row's number just changed: an extent shared only
            // with this snapshot is now exclusive to its neighbour. Drop
            // the whole cache rather than just this key.
            if let Ok(mut cache) = size_cache().lock() {
                cache.invalidate();
            }
            (StatusCode::OK, Json(serde_json::json!({"deleted": id})))
        }
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to delete snapshot: {}", e),
        ),
    }
}

/// GET /api/backingfiles/free-space
///
/// Returns total/used/available bytes for the backingfiles partition.
/// Used by the snapshot management UI's space gauge and by the wizard
/// pre-flight to render context alongside any size-rejection error.
pub async fn get_free_space(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let df = sentryusb_shell::run(
        "df", &["--output=size,used,avail", "--block-size=1", "/backingfiles/"],
    ).await;

    let (total, used, avail) = match df {
        Ok(out) => {
            let line = out.lines().last().unwrap_or("");
            let mut it = line.split_whitespace();
            let total: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let used: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            let avail: u64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            (total, used, avail)
        }
        Err(_) => (0, 0, 0),
    };

    (StatusCode::OK, Json(serde_json::json!({
        "total_bytes": total,
        "used_bytes": used,
        "available_bytes": avail,
        "mounted": total > 0,
    })))
}
