use anyhow::Result;
use rusqlite::params;

use sentryusb_drives::aggregate_telemetry::window_for_route_file;
use sentryusb_drives::{DriveStore, types::Route, types::TempSample};

pub struct PendingRoute {
    pub file: String,
    pub route: Route,
    pub cloud_route_id: Option<String>,
}

pub fn select_pending(store: &DriveStore, limit: i64) -> Result<Vec<PendingRoute>> {
    let files: Vec<(String, Option<String>)> = store.with_read_conn(|conn| -> Result<_> {
        // `start_ts` is always NULL (insert_or_update_route binds it NULL),
        // so it can't actually order anything — without a tiebreaker the
        // upload order is undefined. `file ASC` makes it deterministic, and
        // since Tesla clip paths embed the timestamp it's also chronological
        // (oldest-first), which is the intent. Matches `pending_queue`.
        let mut stmt = conn.prepare(
            "SELECT file, cloud_route_id FROM routes \
             WHERE cloud_uploaded_at IS NULL \
             ORDER BY file ASC LIMIT ?1",
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

    if files.is_empty() {
        return Ok(Vec::new());
    }
    let file_refs: Vec<&str> = files.iter().map(|(f, _)| f.as_str()).collect();
    let routes: Vec<Route> = store
        .with_routes_by_files(&file_refs, |rs| rs.iter().cloned().collect::<Vec<_>>())?;

    let mut out = Vec::with_capacity(routes.len());
    for ((file, cached_route_id), route) in files.into_iter().zip(routes.into_iter()) {

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

/// Temperature samples inside a clip's 60s window, for the cloud blob.
/// Empty when the filename has no parseable timestamp, BLE telemetry
/// never ran, or no sample landed in the window.
pub fn temp_samples_for_route(store: &DriveStore, file: &str) -> Vec<TempSample> {
    let Some((start, end)) = window_for_route_file(file) else {
        return Vec::new();
    };
    store
        .with_read_conn(|conn| -> Result<Vec<TempSample>> {
            let mut stmt = conn.prepare_cached(
                "SELECT ts, interior_temp_c, exterior_temp_c FROM telemetry_samples \
                 WHERE ts BETWEEN ?1 AND ?2 \
                   AND (interior_temp_c IS NOT NULL OR exterior_temp_c IS NOT NULL) \
                 ORDER BY ts ASC",
            )?;
            let rows = stmt.query_map(params![start, end], |r| {
                Ok(TempSample {
                    t: r.get(0)?,
                    i: r.get(1)?,
                    e: r.get(2)?,
                })
            })?;
            let mut out = Vec::new();
            for r in rows {
                out.push(r?);
            }
            Ok(out)
        })
        .unwrap_or_default()
}

pub fn cache_route_id(store: &DriveStore, file: &str, route_id: &str) -> Result<()> {
    store.with_locked_conn(|conn| -> Result<_> {
        conn.execute(
            "UPDATE routes SET cloud_route_id = ?1 WHERE file = ?2",
            params![route_id, file],
        )?;
        Ok(())
    })
}

pub fn mark_uploaded(store: &DriveStore, file: &str, ts_unix: i64) -> Result<()> {
    store.with_locked_conn(|conn| -> Result<_> {
        conn.execute(
            "UPDATE routes SET cloud_uploaded_at = ?1 WHERE file = ?2",
            params![ts_unix, file],
        )?;
        Ok(())
    })
}

pub const PERMANENT_SKIP_SENTINEL: i64 = -1;

pub fn mark_permanent_skip(store: &DriveStore, file: &str) -> Result<()> {
    store.with_locked_conn(|conn| -> Result<_> {
        conn.execute(
            "UPDATE routes SET cloud_uploaded_at = ?1 WHERE file = ?2",
            params![PERMANENT_SKIP_SENTINEL, file],
        )?;
        Ok(())
    })
}

/// Clear `cloud_uploaded_at` for routes whose BLE telemetry rollup is
/// populated locally but was uploaded before the BLE columns were added
/// to the encrypted blob. The next sweep re-encrypts and re-uploads them.
/// Returns the number of rows reset.
///
/// `battery_pct_start` is the canonical signal because it's the most
/// reliably populated BLE column (every clip whose 60s window crossed at
/// least one sample has it). Skip-sentinel rows (`= -1`) are never
/// reset — those were rejected by the cloud for size and re-uploading
/// won't help.
pub fn backfill_ble_reupload(store: &DriveStore) -> Result<i64> {
    store.with_locked_conn(|conn| -> Result<_> {
        let changed = conn.execute(
            "UPDATE routes SET cloud_uploaded_at = NULL \
             WHERE cloud_uploaded_at IS NOT NULL \
               AND cloud_uploaded_at > 0 \
               AND battery_pct_start IS NOT NULL",
            [],
        )?;
        Ok(changed as i64)
    })
}

pub fn pending_count(store: &DriveStore) -> i64 {
    store
        .with_read_conn(|conn| {
            conn.query_row(
                "SELECT count(*) FROM routes \
                 WHERE cloud_uploaded_at IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
        })
}

/// Returns `(uploaded_count, last_upload_unix_seconds)`. The `> 0` filter
/// excludes the [`PERMANENT_SKIP_SENTINEL`] (`-1`) value.
pub fn upload_summary(store: &DriveStore) -> (i64, Option<i64>) {
    store.with_read_conn(|conn| {
        conn.query_row(
            "SELECT count(*), max(cloud_uploaded_at) FROM routes \
             WHERE cloud_uploaded_at > 0",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .unwrap_or((0, None))
    })
}

#[derive(serde::Serialize, Debug)]
pub struct QueueEntry {
    pub file: String,
    pub date: String,
    pub start_ts: Option<i64>,

    pub estimated_size_bytes: i64,

    pub updated_at: i64,
}

pub fn pending_queue(store: &DriveStore, limit: i64) -> Result<Vec<QueueEntry>> {
    store.with_read_conn(|conn| -> Result<_> {
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
