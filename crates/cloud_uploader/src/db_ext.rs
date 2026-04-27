

use anyhow::Result;
use rusqlite::params;

use sentryusb_drives::{DriveStore, types::Route};

pub struct PendingRoute {
    pub file: String,
    pub route: Route,
    pub cloud_route_id: Option<String>,
}

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

#[derive(serde::Serialize, Debug)]
pub struct QueueEntry {
    pub file: String,
    pub date: String,
    pub start_ts: Option<i64>,

    pub estimated_size_bytes: i64,

    pub updated_at: i64,
}

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
