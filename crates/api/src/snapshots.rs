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
/// The live image. Its extents are the main "external" holder that keeps
/// snapshot blocks alive regardless of deletions.
const CAM_DISK: &str = "/backingfiles/cam_disk.bin";

/// One snapshot entry in the listing response.
#[derive(serde::Serialize)]
struct SnapshotEntry {
    /// `snap-<id>` directory name. Used as the path parameter for delete.
    id: String,
    /// Estimated bytes freed by deleting THIS snapshot **and every older
    /// one**, not this snapshot alone.
    ///
    /// Per-snapshot figures are useless on this workload. Hourly snapshots
    /// of a slowly-changing disk share ~99% of their extents with
    /// neighbours, so almost nothing belongs to exactly one snapshot and
    /// every row measures near zero — while the set collectively holds
    /// hundreds of GB. A block is only released when its LAST holder is
    /// deleted, so reclaim is a property of a RUN of snapshots, and
    /// oldest-first prefixes are the runs users actually delete.
    ///
    /// The final row therefore equals the whole snapshot footprint, which
    /// `total_allocated_bytes` derives independently via `df`. Those two
    /// agreeing is the cross-check that the earlier per-snapshot metric
    /// lacked: it summed to ~400 KB against a 217 GB total and nothing
    /// caught it.
    ///
    /// `None` means "not measured yet, or could not be measured" — render
    /// as pending/unavailable, NEVER as `0 B`. A measured zero is
    /// legitimate (a run that frees nothing) and does render as `0 B`.
    cumulative_reclaim_bytes: Option<u64>,
    /// How many snapshots are older than this one, so the UI can say
    /// "this and N older" without depending on its display sort order.
    older_count: usize,
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
/// `ids` must be OLDEST-FIRST: the cumulative figure for a snapshot is
/// "delete this and everything older", so the prefix order is the meaning.
fn spawn_size_refresh(ids: Vec<String>, generation: u64) {
    use std::sync::atomic::Ordering;
    if REFRESH_IN_FLIGHT.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let _guard = RefreshGuard;

        // Collect every snapshot's physical extent map. A snapshot that
        // cannot be mapped poisons the whole computation — a cumulative
        // figure that silently omitted one member of the run would claim
        // the wrong reclaim for every row after it — so the entire attempt
        // is published as "no data" rather than a partial curve.
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

        // Blocks the live image (or any other non-snapshot file) still
        // references never come back, no matter how many snapshots go.
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
            // Publish only if nothing invalidated the cache while we ran —
            // otherwise a delete that landed mid-measurement would be
            // undone by republishing the pre-delete numbers as fresh.
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
            cumulative_reclaim_bytes: None,
            older_count: 0,
            created_unix,
        });
    }

    // Oldest first by mtime. UI may re-sort, but this default matches
    // what users actually want (delete the oldest to free space) — and the
    // cumulative figures are DEFINED against this order, so it is set
    // before the cache fill below.
    entries.sort_by_key(|e| e.created_unix);
    for (i, e) in entries.iter_mut().enumerate() {
        e.older_count = i;
    }

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

    // Cross-check: the curve's final value and `total_allocated_bytes`
    // measure the same thing ("delete every snapshot") by two unrelated
    // routes — FIEMAP extent attribution vs df-minus-du. Disagreement
    // beyond metadata noise means one of them is wrong. The previous
    // per-snapshot metric had no such invariant, which is how it shipped
    // summing to ~400 KB against a 217 GB caption with nothing tripping.
    if let Some(last) = entries.last().and_then(|e| e.cumulative_reclaim_bytes) {
        let hi = total_allocated_bytes.max(last);
        let lo = total_allocated_bytes.min(last);
        // Metadata, the journal and in-flight writes justify some gap;
        // an order-of-magnitude one they do not.
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
