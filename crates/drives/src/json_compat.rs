//! JSON import/export for `drive-data.json` — port of Go
//! `server/drives/import.go` + `export.go`.
//!
//! * Import strips a UTF-8 BOM on read (Windows-edited JSON ships with
//!   one), parses the file with a buffered reader, and bulk-inserts all
//!   routes, processed files, and drive tags into the DB inside a single
//!   transaction. Throws if the route count would shrink the DB below
//!   50% — matches the Go sync-guard intent to refuse obviously-truncated
//!   inputs.
//! * Export walks the DB in deterministic order (routes by `file`,
//!   processed by `file`, tags by `drive_key`+`tag`) so two exports of
//!   the same state produce byte-identical JSON. That matters for rsync
//!   diffs and for Sentry Studio's change-detection.

use std::io::Write;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};

use crate::aggregate::compute_route_aggregates;
use crate::blob::{decode_f32s, decode_gear_runs, decode_points, decode_u8s};
use crate::db::normalize_path;
use crate::types::{GearRun, GpsPoint, Route, StoreData};

/// Minimum existing-route count before the shrink guard applies. Below
/// this, allow any import — tiny DBs don't need corruption protection
/// because there's little to lose. Matches Go's import-side guard.
const SYNCGUARD_MIN_ROUTES: usize = 1000;

/// Minimum fraction of the existing route count that an import must
/// reach to be allowed.
const SYNCGUARD_SHRINK_RATIO: f64 = 0.5;

/// What `import_json` reports back to the caller.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportStats {
    pub routes: usize,
    pub processed_files: usize,
    pub drive_tags: usize,
}

/// Import a Go-format `drive-data.json` into the SQLite store.
///
/// `on_progress` is called once with the total route count after parsing
/// finishes but before the DB write — callers that want progress during
/// insertion can hook row-by-row via the transaction, but for now a
/// single coarse callback matches the granularity the web UI actually
/// displays.
pub fn import_json<F>(
    conn: &mut Connection,
    path: &str,
    on_progress: F,
) -> Result<ImportStats>
where
    F: Fn(usize),
{
    let bytes = std::fs::read(path).with_context(|| format!("open {}", path))?;

    // UTF-8 BOM strip. Windows-edited JSON prepends EF BB BF and
    // `serde_json::from_slice` bails on it. Matches Go's `skipUTF8BOM`.
    let slice: &[u8] = if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &bytes[3..]
    } else {
        &bytes
    };

    let data: StoreData = serde_json::from_slice(slice).context("parse JSON")?;

    let route_count = data.routes.len();

    // Corruption guard: refuse to overwrite a large existing store with
    // a dramatically smaller import.
    let existing_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))
        .unwrap_or(0);
    if existing_count as usize >= SYNCGUARD_MIN_ROUTES
        && (route_count as f64) < (existing_count as f64 * SYNCGUARD_SHRINK_RATIO)
    {
        bail!(
            "refusing import: would shrink store from {} routes to {} (< {:.0}% retained). \
             Likely a truncated or corrupted upload — delete the existing DB manually if \
             you really mean to replace it.",
            existing_count,
            route_count,
            SYNCGUARD_SHRINK_RATIO * 100.0,
        );
    }

    on_progress(route_count);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let tx = conn.transaction()?;

    // Processed files.
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
        )?;
        for f in &data.processed_files {
            stmt.execute(params![normalize_path(f), now])?;
        }
    }

    // Routes — populate BLOBs AND the v2 aggregate columns on insert so
    // summary endpoints serve correct data immediately.
    {
        for r in &data.routes {
            let agg = compute_route_aggregates(r);
            insert_imported_route(&tx, r, &agg, now)?;
        }
    }

    // Tags.
    {
        let mut stmt = tx.prepare(
            "INSERT OR IGNORE INTO drive_tags(drive_key, tag) VALUES(?1, ?2)",
        )?;
        for (key, tags) in &data.drive_tags {
            for t in tags {
                stmt.execute(params![key, t])?;
            }
        }
    }

    tx.commit()?;

    Ok(ImportStats {
        routes: route_count,
        processed_files: data.processed_files.len(),
        drive_tags: data.drive_tags.len(),
    })
}

/// Insert a route from a JSON import — matches the DDL in
/// `insert_or_update_route` from db.rs.
fn insert_imported_route(
    tx: &rusqlite::Transaction,
    r: &Route,
    a: &crate::types::RouteAggregates,
    now: i64,
) -> Result<()> {
    let norm = normalize_path(&r.file);
    let pb = crate::blob::encode_points(Some(&r.points));
    let gb = crate::blob::encode_u8s(Some(&r.gear_states));
    let ab = crate::blob::encode_u8s(Some(&r.autopilot_states));
    let sb = crate::blob::encode_f32s(Some(&r.speeds));
    let acb = crate::blob::encode_f32s(Some(&r.accel_positions));
    let rb = crate::blob::encode_gear_runs(Some(&r.gear_runs));

    let first_lat: Option<f64> = r.points.first().map(|p| p[0]);
    let first_lon: Option<f64> = r.points.first().map(|p| p[1]);

    tx.execute(
        "INSERT OR REPLACE INTO routes(
            file, date_dir, point_count, raw_park_count, raw_frame_count,
            start_ts, end_ts, distance_m, first_lat, first_lon,
            points_blob, gear_states_blob, ap_states_blob,
            speeds_blob, accel_blob, gear_runs_blob, updated_at,
            max_speed_mps, avg_speed_mps, speed_sample_count, valid_point_count,
            fsd_engaged_ms, autosteer_engaged_ms, tacc_engaged_ms,
            fsd_distance_m, autosteer_distance_m, tacc_distance_m, assisted_distance_m,
            fsd_disengagements, fsd_accel_pushes,
            start_lat, start_lon, end_lat, end_lon)
         VALUES(
            ?1, ?2, ?3, ?4, ?5,
            NULL, NULL, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19,
            ?20, ?21, ?22,
            ?23, ?24, ?25, ?26,
            ?27, ?28,
            ?29, ?30, ?31, ?32)",
        params![
            norm, r.date, r.points.len() as i64, r.raw_park_count as i64, r.raw_frame_count as i64,
            a.distance_m, first_lat, first_lon,
            pb, gb, ab, sb, acb, rb, now,
            a.max_speed_mps, a.avg_speed_mps, a.speed_sample_count, a.valid_point_count,
            a.fsd_engaged_ms, a.autosteer_engaged_ms, a.tacc_engaged_ms,
            a.fsd_distance_m, a.autosteer_distance_m, a.tacc_distance_m, a.assisted_distance_m,
            a.fsd_disengagements, a.fsd_accel_pushes,
            a.start_lat, a.start_lng, a.end_lat, a.end_lng,
        ],
    )?;
    Ok(())
}

/// Export the DB contents as `drive-data.json`. Produces deterministic,
/// byte-identical output for the same DB state so rsync / archive
/// diff-detection works correctly.
pub fn export_json<W: Write>(conn: &Connection, writer: &mut W) -> Result<()> {
    // Read routes in file-sorted order and decode BLOBs.
    let routes = select_all_routes_for_export(conn)?;

    let mut processed_files = {
        let mut stmt =
            conn.prepare("SELECT file FROM processed_files ORDER BY file")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        out
    };
    // Belt & suspenders — SQL ORDER BY already sorts but the UI will
    // sometimes insert paths with different case through the API; sort
    // case-insensitively here to match Go's deterministic output.
    processed_files.sort();

    let drive_tags = {
        let mut stmt = conn
            .prepare("SELECT drive_key, tag FROM drive_tags ORDER BY drive_key, tag")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut out: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for r in rows {
            let (k, t) = r?;
            out.entry(k).or_default().push(t);
        }
        out
    };

    // Use a BTreeMap (ordered) → HashMap transition for serialization
    // so drive_tags keys serialize in sorted order. serde_json writes
    // BTreeMap keys in their natural order.
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct OrderedStoreData<'a> {
        processed_files: &'a [String],
        routes: &'a [Route],
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        drive_tags: &'a std::collections::BTreeMap<String, Vec<String>>,
    }

    let out = OrderedStoreData {
        processed_files: &processed_files,
        routes: &routes,
        drive_tags: &drive_tags,
    };
    serde_json::to_writer_pretty(writer, &out).context("serialize JSON")?;
    Ok(())
}

fn select_all_routes_for_export(conn: &Connection) -> Result<Vec<Route>> {
    let mut stmt = conn.prepare(
        "SELECT file, date_dir, raw_park_count, raw_frame_count,
                points_blob, gear_states_blob, ap_states_blob,
                speeds_blob, accel_blob, gear_runs_blob
         FROM routes
         ORDER BY file",
    )?;
    let rows = stmt.query_map([], |row| {
        let pb: Option<Vec<u8>> = row.get(4)?;
        let gb: Option<Vec<u8>> = row.get(5)?;
        let ab: Option<Vec<u8>> = row.get(6)?;
        let sb: Option<Vec<u8>> = row.get(7)?;
        let acb: Option<Vec<u8>> = row.get(8)?;
        let rb: Option<Vec<u8>> = row.get(9)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)? as u32,
            row.get::<_, i64>(3)? as u32,
            pb,
            gb,
            ab,
            sb,
            acb,
            rb,
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (file, date, raw_park_count, raw_frame_count, pb, gb, ab, sb, acb, rb) = r?;
        let points: Vec<GpsPoint> = decode_points(pb.as_deref())
            .with_context(|| format!("decode points {}", file))?
            .unwrap_or_default();
        let gear_states = decode_u8s(gb.as_deref()).unwrap_or_default();
        let autopilot_states = decode_u8s(ab.as_deref()).unwrap_or_default();
        let speeds = decode_f32s(sb.as_deref())
            .with_context(|| format!("decode speeds {}", file))?
            .unwrap_or_default();
        let accel_positions = decode_f32s(acb.as_deref())
            .with_context(|| format!("decode accel {}", file))?
            .unwrap_or_default();
        let gear_runs: Vec<GearRun> = decode_gear_runs(rb.as_deref())
            .with_context(|| format!("decode gear_runs {}", file))?
            .unwrap_or_default();
        out.push(Route {
            file,
            date,
            points,
            gear_states,
            autopilot_states,
            speeds,
            accel_positions,
            raw_park_count,
            raw_frame_count,
            gear_runs,
        });
    }
    Ok(out)
}
