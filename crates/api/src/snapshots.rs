//! Snapshot management API.
//!
//! XFS reflink snapshots live at
//! `/backingfiles/snapshots/snap-<id>/snap.bin`. This module provides:
//!
//!   * `GET    /api/snapshots`               — list with size/timestamp
//!   * `DELETE /api/snapshots/:id`           — delete one snapshot
//!   * `GET    /api/backingfiles/free-space` — total/used/avail in KB
//!
//! Deletion uses `/root/bin/release_snapshot.sh` when available so it shares
//! the runtime's unmount and symlink-cleanup behavior.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::router::AppState;

const SNAPSHOTS_DIR: &str = "/backingfiles/snapshots";
const RELEASE_SNAPSHOT_SCRIPT: &str = "/root/bin/release_snapshot.sh";
/// The live image retains shared extents independently of snapshots.
const CAM_DISK: &str = "/backingfiles/cam_disk.bin";

/// One snapshot entry in the listing response.
#[derive(serde::Serialize)]
struct SnapshotEntry {
    /// `snap-<id>` directory name. Used as the path parameter for delete.
    id: String,
    /// Estimated bytes freed by deleting THIS snapshot **and every older
    /// one**, not this snapshot alone.
    ///
    /// Reflinked blocks are freed only when their final holder is deleted, so
    /// reclaim is meaningful for an oldest-first prefix rather than one file.
    /// The final row should equal the independently derived total footprint.
    ///
    /// `None` means "not measured yet, or could not be measured" — render
    /// as pending/unavailable, NEVER as `0 B`. A measured zero is
    /// legitimate (a run that frees nothing) and does render as `0 B`.
    cumulative_reclaim_bytes: Option<u64>,
    /// Number of older snapshots in the reclaim prefix.
    older_count: usize,
    /// Unix epoch seconds from `snap.bin` mtime.
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

/// Clears the in-flight flag even if measurement panics.
struct RefreshGuard;
impl Drop for RefreshGuard {
    fn drop(&mut self) {
        REFRESH_IN_FLIGHT.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Measure every snapshot off the request path.
///
/// Extent-map walks run off the request path, one refresh at a time.
/// `ids` must be OLDEST-FIRST: the cumulative figure for a snapshot is
/// "delete this and everything older", so the prefix order is the meaning.
fn spawn_size_refresh(ids: Vec<String>, generation: u64) {
    use std::sync::atomic::Ordering;
    if REFRESH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let _guard = RefreshGuard;

        // One missing extent map invalidates every later cumulative value.
        let mut maps: Vec<Vec<sentryusb_gadget::reflink::PhysicalRange>> =
            Vec::with_capacity(ids.len());
        let mut idents = Vec::with_capacity(ids.len());
        let mut failed: Option<(String, String)> = None;
        for id in &ids {
            let bin = format!("{}/{}/snap.bin", SNAPSHOTS_DIR, id);
            let p = std::path::Path::new(&bin);
            match sentryusb_gadget::reflink::extent_map(p)
                .and_then(|m| Ok((m, sentryusb_gadget::reflink::file_identity(p)?)))
            {
                Ok((m, ident)) => {
                    maps.push(m);
                    idents.push(ident);
                }
                Err(e) => {
                    failed = Some((id.clone(), e.to_string()));
                    break;
                }
            }
        }

        // Extents retained by the live image are not reclaimable.
        let external = if failed.is_none() {
            match sentryusb_gadget::reflink::extent_map(std::path::Path::new(CAM_DISK)) {
                Ok(m) => Some(m),
                Err(e) => {
                    failed = Some((CAM_DISK.to_string(), e.to_string()));
                    None
                }
            }
        } else {
            None
        };

        let entries = match (failed, external) {
            (None, Some(external)) => {
                let curve = sentryusb_gadget::reflink::cumulative_reclaim(&maps, &external);
                ids.iter()
                    .zip(curve)
                    .zip(idents)
                    .map(|((id, bytes), ident)| (id.clone(), (bytes, ident)))
                    .collect()
            }
            (failed, _) => {
                if let Some((id, e)) = failed {
                    tracing::warn!("cumulative snapshot sizing failed at {}: {}", id, e);
                }
                Vec::new()
            }
        };

        if let Ok(mut cache) = size_cache().lock() {
            // Do not republish values invalidated by a concurrent deletion.
            if !cache.publish(generation, ids, entries, now_secs()) {
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

        // Use `snap.bin` mtime because autofs metadata updates the directory
        // mtime when an old snapshot is viewed.
        let created_unix = std::fs::symlink_metadata(path.join("snap.bin"))
            .or_else(|_| entry.metadata())
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        entries.push(SnapshotEntry {
            id: name,
            // Filled from the extent-map cache below.
            cumulative_reclaim_bytes: None,
            older_count: 0,
            created_unix,
        });
    }

    // Cumulative reclaim values are defined against oldest-first order.
    entries.sort_by_key(|e| e.created_unix);
    for (i, e) in entries.iter_mut().enumerate() {
        e.older_count = i;
    }

    // A stale value may describe a different snapshot set, so omit it and refresh.
    let now = now_secs();
    let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
    let (current, computed_at, generation) = match size_cache().lock() {
        Ok(cache) => {
            let current = cache.is_current_for(&ids, now, SIZE_MAX_AGE_SECS);
            if current {
                for e in entries.iter_mut() {
                    // Verify the file is still the one that was measured.
                    let bin = format!("{}/{}/snap.bin", SNAPSHOTS_DIR, e.id);
                    e.cumulative_reclaim_bytes =
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
    // Derive pending state from the cache to avoid racing worker publication.
    // Failed measurements still count as completed attempts.
    let pending = !current && !entries.is_empty();

    // `du` double-counts reflinked files. Derive the aggregate exclusive
    // footprint as filesystem-used bytes minus non-snapshot file usage.
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

    // Cross-check FIEMAP attribution against the independent df-minus-du total.
    if let Some(last) = entries.last().and_then(|e| e.cumulative_reclaim_bytes) {
        let hi = total_allocated_bytes.max(last);
        let lo = total_allocated_bytes.min(last);
        // Allow metadata, journal, and in-flight-write noise.
        if hi > 0 && (hi - lo) * 10 > hi * 3 {
            tracing::warn!(
                "snapshot accounting disagreement: cumulative curve ends at {} bytes but \
                 df-based total is {} — one of these is wrong",
                last,
                total_allocated_bytes
            );
        }
    }

    (StatusCode::OK, Json(serde_json::json!({
        "snapshots": entries,
        "total_allocated_bytes": total_allocated_bytes,
        // Distinguish an active measurement from unavailable data.
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

    // Run teardown out of process so its blocking lock wait cannot exhaust API
    // workers. Pass the validated bare ID expected by the installed script;
    // partially installed systems fall back to removing the snapshot directory.
    let script_exists = std::path::Path::new(RELEASE_SNAPSHOT_SCRIPT).exists();
    let result = if script_exists {
        sentryusb_shell::run(RELEASE_SNAPSHOT_SCRIPT, &[id.as_str()]).await
    } else {
        sentryusb_shell::run("rm", &["-rf", &path]).await
    };

    match result {
        Ok(_) => {
            // Deleting one holder changes every cumulative reclaim value.
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
