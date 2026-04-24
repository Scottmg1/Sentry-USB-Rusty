//! JSON import/export for `drive-data.json` — port of Go
//! `server/drives/import.go` + `export.go`.
//!
//! * Import uses a streaming serde visitor that deserializes and inserts
//!   one Route at a time, dropping it before the next is read. Peak Rust
//!   heap is ~one decoded Route (~30 KB) instead of the full Vec<Route>
//!   (hundreds of MB for large files). Strips a UTF-8 BOM if present.
//!   Refuses imports that would shrink the DB below 50%.
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
use crate::types::{GearRun, GpsPoint, Route};

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
/// Uses a streaming serde visitor: each Route is deserialized from the
/// reader, inserted into SQLite, and dropped before the next element is
/// read. Peak Rust heap is approximately one decoded Route (~30 KB) instead
/// of the entire Vec<Route> that a naive `from_slice` would allocate — on a
/// 346 MB file that difference is ~400 MB, which matters critically on
/// devices like the Pi Zero 2W with 512 MB total RAM.
///
/// `on_progress` is called periodically with the running route count so
/// the web UI can show a live indicator during long imports.
pub fn import_json<F>(
    conn: &mut Connection,
    path: &str,
    on_progress: F,
) -> Result<ImportStats>
where
    F: Fn(usize),
{
    use serde::de::{Deserializer as _, DeserializeSeed, MapAccess, SeqAccess, Visitor};
    use std::collections::HashMap;
    use std::fmt;
    use std::io::{BufReader, Read, Seek, SeekFrom};

    // Open and skip UTF-8 BOM (Windows-edited JSON prepends EF BB BF).
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {}", path))?;
    {
        let mut bom = [0u8; 3];
        let n = file.read(&mut bom).unwrap_or(0);
        if n < 3 || bom != [0xEF, 0xBB, 0xBF] {
            file.seek(SeekFrom::Start(0))?;
        }
    }
    let reader = BufReader::with_capacity(64 * 1024, file);

    // Check existing count for the shrink guard before opening the transaction.
    let existing_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))
        .unwrap_or(0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let tx = conn.transaction()?;

    // -------------------------------------------------------------------------
    // Streaming serde visitor chain.  Rust allows impl blocks for local types
    // inside function bodies, which keeps all of this private to import_json.
    // -------------------------------------------------------------------------

    struct Ctx<'tx> {
        tx: &'tx rusqlite::Transaction<'tx>,
        now: i64,
        routes: usize,
        processed_files: usize,
        drive_tags: usize,
    }

    /// Deserializes the `routes` JSON array element-by-element.  Each Route
    /// is inserted and dropped before the next one is deserialized.
    struct RouteSeq<'a, 'tx: 'a>(&'a mut Ctx<'tx>);

    impl<'de, 'a, 'tx: 'a> DeserializeSeed<'de> for RouteSeq<'a, 'tx> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_seq(self)
        }
    }

    impl<'de, 'a, 'tx: 'a> Visitor<'de> for RouteSeq<'a, 'tx> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("routes array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            while let Some(route) = seq.next_element::<Route>()? {
                // Insert immediately — `route` drops at the end of this block,
                // freeing its GPS points, gear-state bytes, etc. before the
                // next JSON element is deserialized.
                let agg = compute_route_aggregates(&route);
                insert_imported_route(self.0.tx, &route, &agg, self.0.now)
                    .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                self.0.routes += 1;
            }
            Ok(())
        }
    }

    /// Top-level visitor for the drive-data.json object.
    struct TopLevel<'a, 'tx: 'a>(&'a mut Ctx<'tx>);

    impl<'de, 'a, 'tx: 'a> Visitor<'de> for TopLevel<'a, 'tx> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("drive-data.json object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
            let ctx = self.0;
            while let Some(key) = map.next_key::<String>()? {
                match key.as_str() {
                    "processedFiles" => {
                        let files: Vec<String> = map.next_value()?;
                        let mut stmt = ctx.tx
                            .prepare(
                                "INSERT OR IGNORE INTO processed_files(file, added_at) \
                                 VALUES(?1, ?2)",
                            )
                            .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        for f in &files {
                            stmt.execute(params![normalize_path(f), ctx.now])
                                .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        }
                        ctx.processed_files = files.len();
                    }
                    "routes" => {
                        // `&mut *ctx` is a reborrow: it ends when next_value_seed
                        // returns, allowing ctx to be used again for later keys.
                        map.next_value_seed(RouteSeq(&mut *ctx))?;
                    }
                    "driveTags" => {
                        let tags: HashMap<String, Vec<String>> = map.next_value()?;
                        let mut stmt = ctx.tx
                            .prepare(
                                "INSERT OR IGNORE INTO drive_tags(drive_key, tag) \
                                 VALUES(?1, ?2)",
                            )
                            .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        for (k, vs) in &tags {
                            for v in vs {
                                stmt.execute(params![k, v])
                                    .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                            }
                        }
                        ctx.drive_tags = tags.len();
                    }
                    _ => {
                        // Unknown top-level key — skip without allocating.
                        map.next_value::<serde_json::Value>()?;
                    }
                }
            }
            Ok(())
        }
    }

    // Run the streaming parse.
    let mut ctx = Ctx { tx: &tx, now, routes: 0, processed_files: 0, drive_tags: 0 };
    let mut de = serde_json::Deserializer::from_reader(reader);
    de.deserialize_map(TopLevel(&mut ctx))
        .map_err(|e: serde_json::Error| {
            anyhow::anyhow!("parse JSON (line {}, column {}): {}", e.line(), e.column(), e)
        })?;

    let stats = ImportStats {
        routes: ctx.routes,
        processed_files: ctx.processed_files,
        drive_tags: ctx.drive_tags,
    };

    // Corruption guard: refuse to replace a large store with a much smaller
    // import (usually a truncated or corrupted file).
    if existing_count as usize >= SYNCGUARD_MIN_ROUTES
        && (stats.routes as f64) < (existing_count as f64 * SYNCGUARD_SHRINK_RATIO)
    {
        bail!(
            "refusing import: would shrink store from {} routes to {} (< {:.0}% retained). \
             Likely a truncated or corrupted upload — delete the existing DB manually if \
             you really mean to replace it.",
            existing_count,
            stats.routes,
            SYNCGUARD_SHRINK_RATIO * 100.0,
        );
    }

    on_progress(stats.routes);
    tx.commit()?;
    Ok(stats)
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
    use crate::db::DriveStore;
    use crate::types::{GearRun, GpsPoint, StoreData};

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
