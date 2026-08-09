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
    /// Bytes consumed by the snapshot directory (recursive).
    size_bytes: u64,
    /// Unix epoch seconds — directory mtime. Used by the UI to render a
    /// human-friendly date and to sort.
    created_unix: i64,
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

        // mtime as the "created" timestamp — the same clock free-space
        // eviction now orders by (slot names are not time-monotonic, so
        // alphabetic order is deliberately not used anywhere for age).
        let created_unix = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Per-snapshot apparent allocated bytes (st_blocks * 512). This
        // is NOT reflink-aware: each snap.bin is `cp --reflink=always` of
        // cam_disk.bin, so every snap.bin's `st_blocks` is the full
        // cam_disk block count even though those extents are shared with
        // the live image and the other snapshots. Treat this as an upper
        // bound, not "what you'd reclaim by deleting just this snapshot."
        // The aggregate `total_allocated_bytes` below uses df-based math
        // to recover the reflink-exclusive footprint.
        let du_out = sentryusb_shell::run(
            "du", &["-sB1", &path.to_string_lossy()],
        ).await.unwrap_or_default();
        let size_bytes: u64 = du_out
            .split_whitespace()
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        entries.push(SnapshotEntry {
            id: name,
            size_bytes,
            created_unix,
        });
    }

    // Oldest first by mtime. UI may re-sort, but this default matches
    // what users actually want (delete the oldest to free space).
    entries.sort_by_key(|e| e.created_unix);

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
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({"deleted": id}))),
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
