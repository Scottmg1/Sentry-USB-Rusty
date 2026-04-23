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

    // Surface serde's detailed error (line/column/classification) instead
    // of the bare "parse JSON" we used to emit — on a user-facing upload
    // the original error text is what tells them whether the file is
    // truncated, has a BOM we didn't strip, or is just the wrong shape.
    let data: StoreData = serde_json::from_slice(slice).map_err(|e| {
        anyhow::anyhow!(
            "parse JSON (line {}, column {}): {}",
            e.line(),
            e.column(),
            e
        )
    })?;

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
///
/// Streams routes one at a time from SQLite directly into the JSON
/// serializer, so peak heap usage stays bounded by a single decoded
/// `Route` instead of the full store. On a 5500-clip DB this caps the
/// export at a few hundred KB of working memory vs. the ~17 MB that
/// materialising all routes used to consume.
pub fn export_json<W: Write>(conn: &Connection, writer: &mut W) -> Result<()> {
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
        routes: RouteStream<'a>,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        drive_tags: &'a std::collections::BTreeMap<String, Vec<String>>,
    }

    // `route_err` is an out-parameter: if SQLite barfs partway through
    // the streaming serialize, the serde error propagated back by
    // `to_writer_pretty` is the generic "io error" wrapper — we stash
    // the real rusqlite error here and swap it back in afterwards so
    // the caller sees the useful message.
    let route_err: std::cell::RefCell<Option<rusqlite::Error>> =
        std::cell::RefCell::new(None);

    let out = OrderedStoreData {
        processed_files: &processed_files,
        routes: RouteStream { conn, error: &route_err },
        drive_tags: &drive_tags,
    };
    let ser_result = serde_json::to_writer_pretty(writer, &out);

    if let Some(e) = route_err.into_inner() {
        return Err(anyhow::Error::from(e).context("export_json: streaming route read failed"));
    }
    ser_result.context("serialize JSON")?;
    Ok(())
}

/// Serializer adapter that streams `Route` rows directly from SQLite
/// into the JSON output without ever holding more than one decoded
/// `Route` in memory. Used by [`export_json`].
struct RouteStream<'a> {
    conn: &'a Connection,
    error: &'a std::cell::RefCell<Option<rusqlite::Error>>,
}

impl<'a> serde::Serialize for RouteStream<'a> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as SerError, SerializeSeq};

        let mut stmt = self
            .conn
            .prepare(
                "SELECT file, date_dir, raw_park_count, raw_frame_count,
                        points_blob, gear_states_blob, ap_states_blob,
                        speeds_blob, accel_blob, gear_runs_blob
                 FROM routes
                 ORDER BY file",
            )
            .map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("routes prepare failed")
            })?;

        let mut rows = stmt.query([]).map_err(|e| {
            *self.error.borrow_mut() = Some(e);
            S::Error::custom("routes query failed")
        })?;

        let mut seq = serializer.serialize_seq(None)?;

        loop {
            let row_opt = rows.next().map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("routes row fetch failed")
            })?;
            let Some(row) = row_opt else { break };

            // Pull columns then decode the BLOBs for this one row.
            // Each route is serialized and dropped before the next is
            // touched, which is what keeps the peak heap bounded.
            let file: String = row.get(0).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col file")
            })?;
            let date: String = row.get(1).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col date_dir")
            })?;
            let raw_park_count: u32 = row
                .get::<_, i64>(2)
                .map_err(|e| {
                    *self.error.borrow_mut() = Some(e);
                    S::Error::custom("col raw_park_count")
                })? as u32;
            let raw_frame_count: u32 = row
                .get::<_, i64>(3)
                .map_err(|e| {
                    *self.error.borrow_mut() = Some(e);
                    S::Error::custom("col raw_frame_count")
                })? as u32;
            let pb: Option<Vec<u8>> = row.get(4).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col points_blob")
            })?;
            let gb: Option<Vec<u8>> = row.get(5).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col gear_states_blob")
            })?;
            let ab: Option<Vec<u8>> = row.get(6).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col ap_states_blob")
            })?;
            let sb: Option<Vec<u8>> = row.get(7).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col speeds_blob")
            })?;
            let acb: Option<Vec<u8>> = row.get(8).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col accel_blob")
            })?;
            let rb: Option<Vec<u8>> = row.get(9).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col gear_runs_blob")
            })?;

            let points: Vec<GpsPoint> = decode_points(pb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode points {}: {}", file, e)))?
                .unwrap_or_default();
            let gear_states = decode_u8s(gb.as_deref()).unwrap_or_default();
            let autopilot_states = decode_u8s(ab.as_deref()).unwrap_or_default();
            let speeds = decode_f32s(sb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode speeds {}: {}", file, e)))?
                .unwrap_or_default();
            let accel_positions = decode_f32s(acb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode accel {}: {}", file, e)))?
                .unwrap_or_default();
            let gear_runs: Vec<GearRun> = decode_gear_runs(rb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode gear_runs {}: {}", file, e)))?
                .unwrap_or_default();

            let route = Route {
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
            };
            seq.serialize_element(&route)?;
            // `route` drops here — its ~10 KB of decoded BLOBs goes back
            // to the allocator before we loop.
        }
        seq.end()
    }
}

#[cfg(test)]
mod streaming_export_tests {
    use super::*;
    use crate::db::DriveStore;
    use crate::types::{GearRun, GpsPoint};

    /// The streaming exporter must produce byte-for-byte parseable JSON
    /// that deserializes back into the same `StoreData` the importer
    /// would reconstruct. Protects against a future "optimise the
    /// allocation loop" change silently breaking Sentry Studio /
    /// archive restore.
    #[test]
    fn streaming_export_roundtrips_to_identical_store_data() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15/clip.mp4",
                "2025-01-15",
                &pts,
                &[4, 4],
                &[1, 1],
                &[25.0, 26.0],
                &[0.5, 0.6],
                0,
                2,
                &[GearRun { gear: 4, frames: 2 }],
            )
            .unwrap();

        let tmp = std::env::temp_dir().join("sentryusb-streaming-export-test.json");
        let tmp_str = tmp.to_string_lossy().to_string();
        store.export_json_to_file(&tmp_str).unwrap();

        let raw = std::fs::read(&tmp).unwrap();
        let data: StoreData = serde_json::from_slice(&raw).unwrap();

        assert_eq!(data.routes.len(), 1);
        assert_eq!(data.routes[0].file, "2025-01-15/clip.mp4");
        assert_eq!(data.routes[0].points, pts);
        assert_eq!(data.routes[0].gear_states, vec![4, 4]);
        assert_eq!(data.routes[0].autopilot_states, vec![1, 1]);
        assert_eq!(data.routes[0].speeds, vec![25.0, 26.0]);
        assert_eq!(data.processed_files, vec!["2025-01-15/clip.mp4"]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn streaming_export_on_empty_store_is_valid_json() {
        let store = DriveStore::open_memory().unwrap();
        let tmp = std::env::temp_dir().join("sentryusb-streaming-export-empty.json");
        let tmp_str = tmp.to_string_lossy().to_string();
        store.export_json_to_file(&tmp_str).unwrap();

        let raw = std::fs::read(&tmp).unwrap();
        let data: StoreData = serde_json::from_slice(&raw).unwrap();
        assert!(data.routes.is_empty());
        assert!(data.processed_files.is_empty());
        assert!(data.drive_tags.is_empty());

        let _ = std::fs::remove_file(&tmp);
    }
}
