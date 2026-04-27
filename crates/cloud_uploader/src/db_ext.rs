//! Cloud-uploader's SQL access into the existing `routes` table. Lives in
//! this crate (not in `sentryusb-drives`) so the drive crate stays free
//! of cloud-specific concerns.
//!
//! All access goes through `DriveStore::with_locked_conn(...)` so we
//! share WAL serialization with `add_route` / `save` / etc. Keep work
//! inside the closure short — long-running queries block all other
//! drive-store I/O.

use anyhow::Result;
use rusqlite::params;

use sentryusb_drives::{DriveStore, types::Route};

/// One pending row to upload. `route` is the full `Route` (deserialized
/// from the per-clip `routes` SQLite columns), `cloud_route_id` is the
/// cached SHA-256 hex (NULL on legacy rows; uploader fills + writes back
/// before encrypt).
pub struct PendingRoute {
    pub file: String,
    pub route: Route,
    pub cloud_route_id: Option<String>,
}

/// Select up to `limit` routes whose `cloud_uploaded_at IS NULL`.
/// The partial index `idx_routes_cloud_pending` covers this query —
/// steady-state cost is just the matching tail, not a full scan.
pub fn select_pending(store: &DriveStore, limit: i64) -> Result<Vec<PendingRoute>> {
    let files: Vec<(String, Option<String>)> = store.with_locked_conn(|conn| -> Result<_> {
        let mut stmt = conn.prepare(
            "SELECT file, cloud_route_id FROM routes \
             WHERE cloud_uploaded_at IS NULL \
             ORDER BY start_ts ASC LIMIT ?1",
        )?;
        let iter = stmt.query_map(params![limit], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
            ))
        })?;
        let mut out = Vec::new();
        for r in iter {
            out.push(r?);
        }
        Ok(out)
    })?;

    // The `Route` itself is reassembled by an existing DriveStore method
    // that decodes the BLOB columns back into the typed shape. We avoid
    // duplicating that decode here — fetch by file batch.
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let file_refs: Vec<&str> = files.iter().map(|(f, _)| f.as_str()).collect();
    let routes: Vec<Route> = store
        .with_routes_by_files(&file_refs, |rs| rs.iter().cloned().collect::<Vec<_>>())?;

    // Both lists are file-keyed — pair by index assuming
    // `with_routes_by_files` preserves input order. If the DriveStore
    // implementation drifts and stops preserving order, this will mis-pair
    // — keep an eye on that contract.
    let mut out = Vec::with_capacity(routes.len());
    for ((file, cached_route_id), route) in files.into_iter().zip(routes.into_iter()) {
        // Sanity: the route's stored `file` field should match.
        if route.file != file {
            tracing::warn!(
                "select_pending: order skew, sql=`{}` route.file=`{}`",
                file,
                route.file
            );
            continue;
        }
        out.push(PendingRoute {
            file,
            route,
            cloud_route_id: cached_route_id,
        });
    }
    Ok(out)
}

/// Persist the cached lowercase-hex SHA-256 for a route. Called by the
/// uploader the first time a route is encrypted on a Pi running v3+ so
/// we never re-derive (locks in stability if path normalization ever
/// changes).
pub fn cache_route_id(store: &DriveStore, file: &str, route_id: &str) -> Result<()> {
    store.with_locked_conn(|conn| -> Result<_> {
        conn.execute(
            "UPDATE routes SET cloud_route_id = ?1 WHERE file = ?2",
            params![route_id, file],
        )?;
        Ok(())
    })
}

/// Stamp `cloud_uploaded_at` on a successful upload (`stored` or
/// `duplicate` — both mean the cloud has the row).
pub fn mark_uploaded(store: &DriveStore, file: &str, ts_unix: i64) -> Result<()> {
    store.with_locked_conn(|conn| -> Result<_> {
        conn.execute(
            "UPDATE routes SET cloud_uploaded_at = ?1 WHERE file = ?2",
            params![ts_unix, file],
        )?;
        Ok(())
    })
}

/// Sentinel value stamped on routes that are permanently rejected by the
/// cloud (`rejected_too_large`). Negative — distinct from any real unix
/// timestamp — so the `select_pending` `IS NULL` filter naturally skips
/// these without needing a separate column or another index.
pub const PERMANENT_SKIP_SENTINEL: i64 = -1;

/// Mark a route as permanently un-uploadable. Called when the server
/// returns `rejected_too_large` for a route — the size will not change
/// across retries, so re-attempting every sweep wastes encrypt cycles
/// and rate-limit budget.
pub fn mark_permanent_skip(store: &DriveStore, file: &str) -> Result<()> {
    store.with_locked_conn(|conn| -> Result<_> {
        conn.execute(
            "UPDATE routes SET cloud_uploaded_at = ?1 WHERE file = ?2",
            params![PERMANENT_SKIP_SENTINEL, file],
        )?;
        Ok(())
    })
}

/// Count of rows with `cloud_uploaded_at IS NULL`. Cheap thanks to the
/// partial index. Used by `/api/cloud/status`.
pub fn pending_count(store: &DriveStore) -> i64 {
    store
        .with_locked_conn(|conn| {
            conn.query_row(
                "SELECT count(*) FROM routes WHERE cloud_uploaded_at IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
        })
}

/// One pending-queue row for `/api/cloud/queue`. Cheap to build —
/// `points_blob` is NOT materialized; we only need the size estimate
/// from the route's stored columns.
#[derive(serde::Serialize, Debug)]
pub struct QueueEntry {
    pub file: String,
    pub date: String,
    pub start_ts: Option<i64>,
    /// Approximation of the encrypted upload size based on
    /// `length(points_blob)` + a small fixed overhead. The real
    /// size is only known after `encrypt::encrypt_route`, but this is
    /// close enough for a "what's queued + how big" UI panel.
    pub estimated_size_bytes: i64,
    /// Wall-clock unix seconds when the row was last updated locally
    /// (i.e., when processing finished and the route was inserted).
    pub updated_at: i64,
}

/// List up to `limit` pending routes for the UI. Sorted oldest-first
/// (same order the uploader will pick them up).
pub fn pending_queue(store: &DriveStore, limit: i64) -> Result<Vec<QueueEntry>> {
    store.with_locked_conn(|conn| -> Result<_> {
        let mut stmt = conn.prepare(
            "SELECT file, date_dir, start_ts, \
                    coalesce(length(points_blob), 0) + \
                    coalesce(length(gear_states_blob), 0) + \
                    coalesce(length(ap_states_blob), 0) + \
                    coalesce(length(speeds_blob), 0) + \
                    coalesce(length(accel_blob), 0) + 256 AS est_bytes, \
                    updated_at \
             FROM routes \
             WHERE cloud_uploaded_at IS NULL \
             ORDER BY start_ts ASC, file ASC LIMIT ?1",
        )?;
        let iter = stmt.query_map(params![limit], |row| {
            Ok(QueueEntry {
                file: row.get(0)?,
                date: row.get(1)?,
                start_ts: row.get(2)?,
                estimated_size_bytes: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for r in iter {
            out.push(r?);
        }
        Ok(out)
    })
}
