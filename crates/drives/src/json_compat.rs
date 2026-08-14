//! JSON import/export for `drive-data.json`.
//!
//! * Import uses a streaming serde visitor that deserializes and inserts
//!   one Route at a time, dropping it before the next is read. Peak Rust
//!   heap is ~one decoded Route (~30 KB) instead of the full Vec<Route>
//!   (hundreds of MB for large files). Strips a UTF-8 BOM if present.
//!   Refuses imports that would shrink the DB below 50%.
//! * Export walks the DB in deterministic order (routes by `file`,
//!   processed by `file`, tags by `drive_key`+`tag`, telemetry by `ts`)
//!   so two exports of the same state produce byte-identical JSON. That
//!   matters for rsync diffs and for Sentry Studio's change-detection.
//! * The export is the full user-data backup: alongside the original
//!   `processedFiles`/`routes`/`driveTags` sections it carries
//!   `telemetrySamples` (raw BLE sampler rows — charging history and the
//!   drive temp/battery/tire chart series are derived from these and
//!   have no other persistence), `chargeTags`, and `chargeCosts`.
//!   Device-local state (`meta`, `charge_uploads`, `mutable_dirty`) is
//!   deliberately excluded — restoring another install's sync cursors
//!   would corrupt cloud sync. Output is compact (not pretty-printed);
//!   telemetry runs to hundreds of thousands of rows.

use std::io::Write;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use tracing::{debug, info, warn};

use crate::aggregate::compute_route_aggregates;
use crate::blob::{decode_f32s, decode_flag_runs, decode_gear_runs, decode_points, decode_u8s};
use crate::db::normalize_path;
use crate::types::{FlagRun, GearRun, GpsPoint, Route};

/// Minimum existing-route count before the shrink guard applies. Below
/// this, allow any import — tiny DBs don't need corruption protection
/// because there's little to lose.
const SYNCGUARD_MIN_ROUTES: usize = 1000;

/// Minimum fraction of the existing route count that an import must
/// reach to be allowed.
const SYNCGUARD_SHRINK_RATIO: f64 = 0.5;

/// What `import_json` reports back to the caller.
///
/// The telemetry/charge fields are `#[serde(default)]` so import-history
/// records persisted before the full-DB export still deserialize.
#[derive(Debug, Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportStats {
    pub routes: usize,
    pub processed_files: usize,
    pub drive_tags: usize,
    #[serde(default)]
    pub telemetry_samples: usize,
    #[serde(default)]
    pub charge_tags: usize,
    #[serde(default)]
    pub charge_costs: usize,
}

/// Per-route problems discovered during a JSON import. The route is still
/// inserted in every case — these counters exist so operators can see *why*
/// a drive might not appear in the UI even though the import "succeeded".
/// Persisted to the `meta` table after each import for post-mortem queries.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportDiagnostics {
    /// Total routes seen in the JSON (matches `ImportStats.routes` on success).
    pub seen: usize,
    /// Routes whose `points` array was empty — they insert with NULL lat/lon
    /// and zero aggregates, but still appear in `/api/drives/routes`.
    pub empty_points: usize,
    /// Routes whose `date` field was empty/whitespace.
    pub empty_date: usize,
    /// Routes whose normalized `file` matched another route earlier in the
    /// same JSON — second occurrence overwrites the first via INSERT OR REPLACE.
    pub duplicate_files_in_json: usize,
    /// First few file paths flagged by any of the above categories. Capped to
    /// `EXAMPLE_LIMIT` so a pathological import doesn't blow up the meta row.
    pub bad_examples: Vec<String>,
}

/// Cap on `ImportDiagnostics::bad_examples` length.
const EXAMPLE_LIMIT: usize = 20;

impl ImportDiagnostics {
    fn record_example(&mut self, file: &str) {
        if self.bad_examples.len() < EXAMPLE_LIMIT {
            self.bad_examples.push(file.to_string());
        }
    }

    /// True when at least one category fired — operators should look at logs.
    pub fn has_problems(&self) -> bool {
        self.empty_points > 0 || self.empty_date > 0 || self.duplicate_files_in_json > 0
    }
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
) -> Result<(ImportStats, ImportDiagnostics)>
where
    F: Fn(usize),
{
    use serde::de::{Deserializer as _, DeserializeSeed, MapAccess, SeqAccess, Visitor};
    use std::collections::{HashMap, HashSet};
    use std::fmt;
    use std::io::{BufReader, Read, Seek, SeekFrom};

    // File size is useful in logs to spot zero-byte or truncated uploads.
    let file_size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

    // Open and skip UTF-8 BOM (Windows-edited JSON prepends EF BB BF).
    let mut file = std::fs::File::open(path)
        .with_context(|| format!("open {}", path))?;
    {
        let mut bom = [0u8; 3];
        match file.read(&mut bom) {
            Ok(n) => {
                if n < 3 || bom != [0xEF, 0xBB, 0xBF] {
                    file.seek(SeekFrom::Start(0))?;
                }
            }
            Err(e) => {
                warn!("import_json: BOM probe read failed on {}: {}", path, e);
                file.seek(SeekFrom::Start(0))?;
            }
        }
    }
    let reader = BufReader::with_capacity(64 * 1024, file);

    // Check existing count for the shrink guard before opening the transaction.
    let existing_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))
        .unwrap_or(0);

    info!(
        "import_json: starting path={} size_bytes={} existing_routes={}",
        path, file_size, existing_count
    );

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
        telemetry_samples: usize,
        charge_tags: usize,
        charge_costs: usize,
        on_progress: &'tx dyn Fn(usize),
        diag: &'tx mut ImportDiagnostics,
        seen_files: &'tx mut HashSet<String>,
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
                let agg = compute_route_aggregates(&route);

                // Per-route diagnostics — record problems but still insert.
                self.0.diag.seen += 1;
                let norm = normalize_path(&route.file);
                let mut flagged = false;
                if route.points.is_empty() {
                    self.0.diag.empty_points += 1;
                    flagged = true;
                }
                if route.date.trim().is_empty() {
                    self.0.diag.empty_date += 1;
                    flagged = true;
                }
                if !self.0.seen_files.insert(norm.clone()) {
                    self.0.diag.duplicate_files_in_json += 1;
                    flagged = true;
                }
                if flagged {
                    self.0.diag.record_example(&route.file);
                }
                debug!(
                    "import: route file={} points={} date={}",
                    route.file,
                    route.points.len(),
                    route.date
                );

                insert_imported_route(self.0.tx, &route, &agg, self.0.now)
                    .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                self.0.routes += 1;
                if self.0.routes % 50 == 0 {
                    (self.0.on_progress)(self.0.routes);
                }
            }
            Ok(())
        }
    }

    /// Deserializes the `telemetrySamples` JSON array element-by-element,
    /// same streaming pattern as `RouteSeq` — a year of samples is
    /// hundreds of thousands of rows and must never materialize at once.
    struct SampleSeq<'a, 'tx: 'a>(&'a mut Ctx<'tx>);

    impl<'de, 'a, 'tx: 'a> DeserializeSeed<'de> for SampleSeq<'a, 'tx> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_seq(self)
        }
    }

    impl<'de, 'a, 'tx: 'a> Visitor<'de> for SampleSeq<'a, 'tx> {
        type Value = ();
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("telemetrySamples array")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
            let mut stmt = self
                .0
                .tx
                .prepare(&format!(
                    "INSERT OR IGNORE INTO telemetry_samples ({}) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, \
                             ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25)",
                    TELEMETRY_COLUMNS
                ))
                .map_err(|e| serde::de::Error::custom(e.to_string()))?;
            while let Some(s) = seq.next_element::<TelemetrySampleJson>()? {
                stmt.execute(params![
                    s.ts, s.source, s.battery_pct, s.battery_temp_c, s.interior_temp_c,
                    s.exterior_temp_c, s.hvac_on,
                    s.tire_fl_psi, s.tire_fr_psi, s.tire_rl_psi, s.tire_rr_psi,
                    s.odometer_mi, s.software_version, s.location_name,
                    s.charger_power_kw, s.charger_actual_current_a, s.charger_voltage_v,
                    s.charge_rate_mph, s.charge_energy_added_kwh, s.charge_limit_soc,
                    s.battery_range_mi, s.latitude, s.longitude,
                    s.charge_minutes_to_full, s.charging_state,
                ])
                .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                self.0.telemetry_samples += 1;
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
                    "telemetrySamples" => {
                        map.next_value_seed(SampleSeq(&mut *ctx))?;
                    }
                    "chargeTags" => {
                        let tags: HashMap<i64, Vec<String>> = map.next_value()?;
                        let mut stmt = ctx.tx
                            .prepare(
                                "INSERT OR IGNORE INTO charge_tags(session_ts, tag) \
                                 VALUES(?1, ?2)",
                            )
                            .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        for (k, vs) in &tags {
                            for v in vs {
                                stmt.execute(params![k, v])
                                    .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                                ctx.charge_tags += 1;
                            }
                        }
                    }
                    "chargeCosts" => {
                        let costs: HashMap<i64, ChargeCostJson> = map.next_value()?;
                        let mut stmt = ctx.tx
                            .prepare(
                                "INSERT OR IGNORE INTO charge_costs(session_ts, amount, currency) \
                                 VALUES(?1, ?2, ?3)",
                            )
                            .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                        for (k, c) in &costs {
                            stmt.execute(params![k, c.amount, c.currency])
                                .map_err(|e| serde::de::Error::custom(e.to_string()))?;
                            ctx.charge_costs += 1;
                        }
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
    let mut diag = ImportDiagnostics::default();
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut ctx = Ctx {
        tx: &tx,
        now,
        routes: 0,
        processed_files: 0,
        drive_tags: 0,
        telemetry_samples: 0,
        charge_tags: 0,
        charge_costs: 0,
        on_progress: &on_progress,
        diag: &mut diag,
        seen_files: &mut seen_files,
    };
    let mut de = serde_json::Deserializer::from_reader(reader);
    de.deserialize_map(TopLevel(&mut ctx))
        .map_err(|e: serde_json::Error| {
            anyhow::anyhow!("parse JSON (line {}, column {}): {}", e.line(), e.column(), e)
        })?;

    let stats = ImportStats {
        routes: ctx.routes,
        processed_files: ctx.processed_files,
        drive_tags: ctx.drive_tags,
        telemetry_samples: ctx.telemetry_samples,
        charge_tags: ctx.charge_tags,
        charge_costs: ctx.charge_costs,
    };

    // Corruption guard: refuse to replace a large store with a much smaller
    // import (usually a truncated or corrupted file).
    if existing_count as usize >= SYNCGUARD_MIN_ROUTES
        && (stats.routes as f64) < (existing_count as f64 * SYNCGUARD_SHRINK_RATIO)
    {
        warn!(
            "import_json: shrink guard refused import — existing={} new={} threshold={:.0}%",
            existing_count,
            stats.routes,
            SYNCGUARD_SHRINK_RATIO * 100.0,
        );
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

    // Aggregate summary at INFO so a healthy import emits one line.
    info!(
        "import_json: complete path={} routes={} processed_files={} drive_tags={} \
         telemetry_samples={} charge_tags={} charge_costs={} \
         empty_points={} empty_date={} duplicate_files_in_json={}",
        path,
        stats.routes,
        stats.processed_files,
        stats.drive_tags,
        stats.telemetry_samples,
        stats.charge_tags,
        stats.charge_costs,
        diag.empty_points,
        diag.empty_date,
        diag.duplicate_files_in_json,
    );

    // One WARN per non-zero category, with up to EXAMPLE_LIMIT example file
    // paths so operators can grep for the offending entries.
    if diag.empty_points > 0 {
        warn!(
            "import_json: {} route(s) had empty points array — they will not appear as drives. Examples: {:?}",
            diag.empty_points, diag.bad_examples
        );
    }
    if diag.empty_date > 0 {
        warn!(
            "import_json: {} route(s) had empty date field. Examples: {:?}",
            diag.empty_date, diag.bad_examples
        );
    }
    if diag.duplicate_files_in_json > 0 {
        warn!(
            "import_json: {} route(s) had duplicate file paths within the JSON — second occurrence overwrote the first. Examples: {:?}",
            diag.duplicate_files_in_json, diag.bad_examples
        );
    }

    Ok((stats, diag))
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
    // Absent flagRuns (pre-flags Sentry-Drive exports, Tessie imports)
    // stores NULL — the summon detector must see "no evidence", not an
    // empty-but-present run list.
    let fb = if r.flag_runs.is_empty() {
        None
    } else {
        crate::blob::encode_flag_runs(Some(&r.flag_runs))
    };
    // v20 IMU channels: NULL-for-absent, like flag_runs.
    let axb = if r.accel_x.is_empty() {
        None
    } else {
        crate::blob::encode_f32s(Some(&r.accel_x))
    };
    let ayb = if r.accel_y.is_empty() {
        None
    } else {
        crate::blob::encode_f32s(Some(&r.accel_y))
    };

    let first_lat: Option<f64> = r.points.first().map(|p| p[0]);
    let first_lon: Option<f64> = r.points.first().map(|p| p[1]);

    // Merge semantics for re-imported rows (ON CONFLICT DO UPDATE, not
    // OR REPLACE — row replacement wiped every column the INSERT didn't
    // carry):
    //   * clip content + aggregates: the import wins (fresh recompute);
    //   * BLE telemetry rollups: COALESCE — a Rusty round-trip carries
    //     them and wins, but a Sentry-Drive re-process exports them null
    //     and must not blank what the Pi recorded;
    //   * cloud bookkeeping (cloud_uploaded_at / cloud_route_id): never
    //     touched — resetting it re-uploaded the whole library to the
    //     cloud as duplicates after every backfill import. New rows
    //     (e.g. Tessie/Teslascope history arriving via Sentry-Drive)
    //     insert with it NULL, i.e. queued for upload.
    tx.execute(
        "INSERT INTO routes(
            file, date_dir, point_count, raw_park_count, raw_frame_count,
            start_ts, end_ts, distance_m, first_lat, first_lon,
            points_blob, gear_states_blob, ap_states_blob,
            speeds_blob, accel_blob, gear_runs_blob, updated_at,
            max_speed_mps, avg_speed_mps, speed_sample_count, valid_point_count,
            fsd_engaged_ms, autosteer_engaged_ms, tacc_engaged_ms,
            fsd_distance_m, autosteer_distance_m, tacc_distance_m, assisted_distance_m,
            fsd_disengagements, fsd_accel_pushes,
            start_lat, start_lon, end_lat, end_lon,
            source, external_signature, tessie_autopilot_percent,
            battery_pct_start, battery_pct_end,
            interior_temp_min, interior_temp_max, exterior_temp_avg,
            hvac_runtime_s,
            tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
            odometer_mi_start, odometer_mi_end,
            location_name_start, location_name_end,
            fsd_pend_ms_end, park_ms_start, fsd_at_end, fsd_accel_pushes_early,
            ap_at_start,
            flag_runs_blob, sei_speed_abs_max,
            safety_hard_brake_ms, safety_hard_brake_events,
            safety_aggr_turn_ms, safety_aggr_turn_events,
            safety_speeding_ms, safety_moving_ms, safety_manual_moving_ms,
            safety_brake_any_ms, safety_turn_any_ms,
            accel_x_blob, accel_y_blob)
         VALUES(
            ?1, ?2, ?3, ?4, ?5,
            NULL, NULL, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19,
            ?20, ?21, ?22,
            ?23, ?24, ?25, ?26,
            ?27, ?28,
            ?29, ?30, ?31, ?32,
            ?33, ?34, ?35,
            ?36, ?37, ?38, ?39, ?40, ?41,
            ?42, ?43, ?44, ?45,
            ?46, ?47, ?48, ?49,
            ?50, ?51, ?52, ?53, ?54,
            ?55, ?56,
            ?57, ?58, ?59, ?60, ?61, ?62, ?63, ?64, ?65,
            ?66, ?67)
         ON CONFLICT(file) DO UPDATE SET
            date_dir            = excluded.date_dir,
            point_count         = excluded.point_count,
            raw_park_count      = excluded.raw_park_count,
            raw_frame_count     = excluded.raw_frame_count,
            distance_m          = excluded.distance_m,
            first_lat           = excluded.first_lat,
            first_lon           = excluded.first_lon,
            points_blob         = excluded.points_blob,
            gear_states_blob    = excluded.gear_states_blob,
            ap_states_blob      = excluded.ap_states_blob,
            speeds_blob         = excluded.speeds_blob,
            accel_blob          = excluded.accel_blob,
            gear_runs_blob      = excluded.gear_runs_blob,
            updated_at          = excluded.updated_at,
            max_speed_mps       = excluded.max_speed_mps,
            avg_speed_mps       = excluded.avg_speed_mps,
            speed_sample_count  = excluded.speed_sample_count,
            valid_point_count   = excluded.valid_point_count,
            fsd_engaged_ms      = excluded.fsd_engaged_ms,
            autosteer_engaged_ms= excluded.autosteer_engaged_ms,
            tacc_engaged_ms     = excluded.tacc_engaged_ms,
            fsd_distance_m      = excluded.fsd_distance_m,
            autosteer_distance_m= excluded.autosteer_distance_m,
            tacc_distance_m     = excluded.tacc_distance_m,
            assisted_distance_m = excluded.assisted_distance_m,
            fsd_disengagements  = excluded.fsd_disengagements,
            fsd_accel_pushes    = excluded.fsd_accel_pushes,
            start_lat           = excluded.start_lat,
            start_lon           = excluded.start_lon,
            end_lat             = excluded.end_lat,
            end_lon             = excluded.end_lon,
            source              = excluded.source,
            external_signature  = excluded.external_signature,
            tessie_autopilot_percent = excluded.tessie_autopilot_percent,
            battery_pct_start   = COALESCE(excluded.battery_pct_start, battery_pct_start),
            battery_pct_end     = COALESCE(excluded.battery_pct_end, battery_pct_end),
            interior_temp_min   = COALESCE(excluded.interior_temp_min, interior_temp_min),
            interior_temp_max   = COALESCE(excluded.interior_temp_max, interior_temp_max),
            exterior_temp_avg   = COALESCE(excluded.exterior_temp_avg, exterior_temp_avg),
            hvac_runtime_s      = COALESCE(excluded.hvac_runtime_s, hvac_runtime_s),
            tire_fl_psi         = COALESCE(excluded.tire_fl_psi, tire_fl_psi),
            tire_fr_psi         = COALESCE(excluded.tire_fr_psi, tire_fr_psi),
            tire_rl_psi         = COALESCE(excluded.tire_rl_psi, tire_rl_psi),
            tire_rr_psi         = COALESCE(excluded.tire_rr_psi, tire_rr_psi),
            odometer_mi_start   = COALESCE(excluded.odometer_mi_start, odometer_mi_start),
            odometer_mi_end     = COALESCE(excluded.odometer_mi_end, odometer_mi_end),
            location_name_start = COALESCE(excluded.location_name_start, location_name_start),
            location_name_end   = COALESCE(excluded.location_name_end, location_name_end),
            fsd_pend_ms_end     = excluded.fsd_pend_ms_end,
            park_ms_start       = excluded.park_ms_start,
            fsd_at_end          = excluded.fsd_at_end,
            fsd_accel_pushes_early = excluded.fsd_accel_pushes_early,
            ap_at_start         = excluded.ap_at_start,
            flag_runs_blob      = excluded.flag_runs_blob,
            sei_speed_abs_max   = excluded.sei_speed_abs_max,
            safety_hard_brake_ms     = excluded.safety_hard_brake_ms,
            safety_hard_brake_events = excluded.safety_hard_brake_events,
            safety_aggr_turn_ms      = excluded.safety_aggr_turn_ms,
            safety_aggr_turn_events  = excluded.safety_aggr_turn_events,
            safety_speeding_ms       = excluded.safety_speeding_ms,
            safety_moving_ms         = excluded.safety_moving_ms,
            safety_manual_moving_ms  = excluded.safety_manual_moving_ms,
            safety_brake_any_ms      = excluded.safety_brake_any_ms,
            safety_turn_any_ms       = excluded.safety_turn_any_ms,
            accel_x_blob        = COALESCE(excluded.accel_x_blob, accel_x_blob),
            accel_y_blob        = COALESCE(excluded.accel_y_blob, accel_y_blob)",
        params![
            norm, r.date, r.points.len() as i64, r.raw_park_count as i64, r.raw_frame_count as i64,
            a.distance_m, first_lat, first_lon,
            pb, gb, ab, sb, acb, rb, now,
            a.max_speed_mps, a.avg_speed_mps, a.speed_sample_count, a.valid_point_count,
            a.fsd_engaged_ms, a.autosteer_engaged_ms, a.tacc_engaged_ms,
            a.fsd_distance_m, a.autosteer_distance_m, a.tacc_distance_m, a.assisted_distance_m,
            a.fsd_disengagements, a.fsd_accel_pushes,
            a.start_lat, a.start_lng, a.end_lat, a.end_lng,
            r.source, r.external_signature, r.tessie_autopilot_percent,
            r.battery_pct_start, r.battery_pct_end,
            r.interior_temp_min, r.interior_temp_max, r.exterior_temp_avg,
            r.hvac_runtime_s,
            r.tire_fl_psi, r.tire_fr_psi, r.tire_rl_psi, r.tire_rr_psi,
            r.odometer_mi_start, r.odometer_mi_end,
            r.location_name_start, r.location_name_end,
            a.fsd_pend_ms_end, a.park_ms_start, a.fsd_at_end as i64, a.fsd_accel_pushes_early,
            a.ap_at_start,
            fb, a.sei_speed_abs_max,
            a.safety_hard_brake_ms, a.safety_hard_brake_events,
            a.safety_aggr_turn_ms, a.safety_aggr_turn_events,
            a.safety_speeding_ms, a.safety_moving_ms, a.safety_manual_moving_ms,
            a.safety_brake_any_ms, a.safety_turn_any_ms,
            axb, ayb,
        ],
    )?;
    Ok(())
}

/// One raw `telemetry_samples` row in the export. Every column the BLE
/// sampler writes rides through, so a restore rebuilds charging history
/// and the per-drive chart series exactly — those are derived live from
/// this table and have no other persistence. Nulls are omitted, which
/// shrinks the sparse body-controller rows to ~40 bytes each.
///
/// Field order here, the SELECT in `TelemetrySampleStream`, and the
/// INSERT in `insert_imported_sample` must stay in sync — the round-trip
/// test covers every column to catch drift.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TelemetrySampleJson {
    ts: i64,
    #[serde(default)]
    source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery_temp_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    interior_temp_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    exterior_temp_c: Option<f64>,
    /// 0/1 as stored — kept integer so the round-trip is exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hvac_on: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tire_fl_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tire_fr_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tire_rl_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tire_rr_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    odometer_mi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    software_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    location_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charger_power_kw: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charger_actual_current_a: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charger_voltage_v: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charge_rate_mph: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charge_energy_added_kwh: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charge_limit_soc: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    battery_range_mi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    longitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charge_minutes_to_full: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    charging_state: Option<String>,
}

/// Column list shared by the export SELECT and the import INSERT —
/// ordinal positions match `TelemetrySampleJson` field order.
const TELEMETRY_COLUMNS: &str =
    "ts, source, battery_pct, battery_temp_c, interior_temp_c, exterior_temp_c, hvac_on, \
     tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi, \
     odometer_mi, software_version, location_name, \
     charger_power_kw, charger_actual_current_a, charger_voltage_v, \
     charge_rate_mph, charge_energy_added_kwh, charge_limit_soc, battery_range_mi, \
     latitude, longitude, charge_minutes_to_full, charging_state";

/// Manual cost override for one charge session in the export.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChargeCostJson {
    amount: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    currency: Option<String>,
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
    // Belt & suspenders — SQL ORDER BY (BINARY) already sorts; this
    // byte-wise re-sort just pins deterministic output even if the
    // query ever changes.
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

    // Charge-session tags and manual cost overrides — small tables,
    // collected up front like drive_tags. BTreeMap keys (the numeric
    // session timestamps) serialize in sorted order for determinism.
    let charge_tags = {
        let mut stmt = conn
            .prepare("SELECT session_ts, tag FROM charge_tags ORDER BY session_ts, tag")?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        let mut out: std::collections::BTreeMap<i64, Vec<String>> =
            std::collections::BTreeMap::new();
        for r in rows {
            let (k, t) = r?;
            out.entry(k).or_default().push(t);
        }
        out
    };
    let charge_costs = {
        let mut stmt = conn
            .prepare("SELECT session_ts, amount, currency FROM charge_costs ORDER BY session_ts")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                ChargeCostJson { amount: row.get(1)?, currency: row.get(2)? },
            ))
        })?;
        let mut out: std::collections::BTreeMap<i64, ChargeCostJson> =
            std::collections::BTreeMap::new();
        for r in rows {
            let (k, c) = r?;
            out.insert(k, c);
        }
        out
    };

    // The sample stream can't know it's empty until it runs, so probe the
    // count up front — an empty table omits the key entirely, keeping
    // pre-telemetry exports byte-stable.
    let telemetry_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM telemetry_samples", [], |r| r.get(0))?;

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
        #[serde(skip_serializing_if = "Option::is_none")]
        telemetry_samples: Option<TelemetrySampleStream<'a>>,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        charge_tags: &'a std::collections::BTreeMap<i64, Vec<String>>,
        #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        charge_costs: &'a std::collections::BTreeMap<i64, ChargeCostJson>,
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
        telemetry_samples: (telemetry_count > 0)
            .then_some(TelemetrySampleStream { conn, error: &route_err }),
        charge_tags: &charge_tags,
        charge_costs: &charge_costs,
    };
    // Compact (not pretty) on purpose: telemetry runs to hundreds of
    // thousands of rows and pretty-printing would roughly double the
    // file size and the export time on the Pi.
    let ser_result = serde_json::to_writer(writer, &out);

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
                        speeds_blob, accel_blob, gear_runs_blob,
                        source, external_signature, tessie_autopilot_percent,
                        battery_pct_start, battery_pct_end,
                        interior_temp_min, interior_temp_max, exterior_temp_avg,
                        hvac_runtime_s,
                        tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
                        odometer_mi_start, odometer_mi_end,
                        location_name_start, location_name_end,
                        flag_runs_blob, accel_x_blob, accel_y_blob
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
            let source: Option<String> = row.get(10).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col source")
            })?;
            let external_signature: Option<String> = row.get(11).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col external_signature")
            })?;
            let tessie_autopilot_percent: Option<f64> = row.get(12).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tessie_autopilot_percent")
            })?;
            // BLE telemetry rollup — Option<f64>/Option<String>, all
            // NULL on pre-v6 rows or clips whose 60s window had no
            // samples. Per-column .get to keep the streaming-row error
            // shape consistent.
            let battery_pct_start: Option<f64> = row.get(13).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col battery_pct_start")
            })?;
            let battery_pct_end: Option<f64> = row.get(14).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col battery_pct_end")
            })?;
            let interior_temp_min: Option<f64> = row.get(15).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col interior_temp_min")
            })?;
            let interior_temp_max: Option<f64> = row.get(16).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col interior_temp_max")
            })?;
            let exterior_temp_avg: Option<f64> = row.get(17).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col exterior_temp_avg")
            })?;
            let hvac_runtime_s: Option<i64> = row.get(18).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col hvac_runtime_s")
            })?;
            let tire_fl_psi: Option<f64> = row.get(19).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_fl_psi")
            })?;
            let tire_fr_psi: Option<f64> = row.get(20).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_fr_psi")
            })?;
            let tire_rl_psi: Option<f64> = row.get(21).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_rl_psi")
            })?;
            let tire_rr_psi: Option<f64> = row.get(22).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col tire_rr_psi")
            })?;
            let odometer_mi_start: Option<f64> = row.get(23).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col odometer_mi_start")
            })?;
            let odometer_mi_end: Option<f64> = row.get(24).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col odometer_mi_end")
            })?;
            let location_name_start: Option<String> = row.get(25).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col location_name_start")
            })?;
            let location_name_end: Option<String> = row.get(26).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col location_name_end")
            })?;
            let fb: Option<Vec<u8>> = row.get(27).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col flag_runs_blob")
            })?;
            let axb: Option<Vec<u8>> = row.get(28).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col accel_x_blob")
            })?;
            let ayb: Option<Vec<u8>> = row.get(29).map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("col accel_y_blob")
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
            let flag_runs: Vec<FlagRun> = decode_flag_runs(fb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode flag_runs {}: {}", file, e)))?
                .unwrap_or_default();
            let accel_x = decode_f32s(axb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode accel_x {}: {}", file, e)))?
                .unwrap_or_default();
            let accel_y = decode_f32s(ayb.as_deref())
                .map_err(|e| S::Error::custom(format!("decode accel_y {}: {}", file, e)))?
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
                flag_runs,
                accel_x,
                accel_y,
                source,
                external_signature,
                tessie_autopilot_percent,
                battery_pct_start,
                battery_pct_end,
                interior_temp_min,
                interior_temp_max,
                exterior_temp_avg,
                hvac_runtime_s,
                tire_fl_psi,
                tire_fr_psi,
                tire_rl_psi,
                tire_rr_psi,
                odometer_mi_start,
                odometer_mi_end,
                location_name_start,
                location_name_end,
        temp_samples: Vec::new(),
            };
            seq.serialize_element(&route)?;
            // `route` drops here — its ~10 KB of decoded BLOBs goes back
            // to the allocator before we loop.
        }
        seq.end()
    }
}

/// Streams `telemetry_samples` rows into the JSON output one at a time,
/// same pattern as [`RouteStream`] — at 60s sampling cadence the table
/// reaches hundreds of thousands of rows, so materializing it would
/// dwarf the rest of the export's working memory.
struct TelemetrySampleStream<'a> {
    conn: &'a Connection,
    error: &'a std::cell::RefCell<Option<rusqlite::Error>>,
}

impl<'a> serde::Serialize for TelemetrySampleStream<'a> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as SerError, SerializeSeq};

        let mut stmt = self
            .conn
            .prepare(&format!(
                "SELECT {} FROM telemetry_samples ORDER BY ts",
                TELEMETRY_COLUMNS
            ))
            .map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("telemetry prepare failed")
            })?;

        let mut rows = stmt.query([]).map_err(|e| {
            *self.error.borrow_mut() = Some(e);
            S::Error::custom("telemetry query failed")
        })?;

        let mut seq = serializer.serialize_seq(None)?;
        loop {
            let row_opt = rows.next().map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("telemetry row fetch failed")
            })?;
            let Some(row) = row_opt else { break };

            let sample = (|| -> rusqlite::Result<TelemetrySampleJson> {
                Ok(TelemetrySampleJson {
                    ts: row.get(0)?,
                    source: row.get(1)?,
                    battery_pct: row.get(2)?,
                    battery_temp_c: row.get(3)?,
                    interior_temp_c: row.get(4)?,
                    exterior_temp_c: row.get(5)?,
                    hvac_on: row.get(6)?,
                    tire_fl_psi: row.get(7)?,
                    tire_fr_psi: row.get(8)?,
                    tire_rl_psi: row.get(9)?,
                    tire_rr_psi: row.get(10)?,
                    odometer_mi: row.get(11)?,
                    software_version: row.get(12)?,
                    location_name: row.get(13)?,
                    charger_power_kw: row.get(14)?,
                    charger_actual_current_a: row.get(15)?,
                    charger_voltage_v: row.get(16)?,
                    charge_rate_mph: row.get(17)?,
                    charge_energy_added_kwh: row.get(18)?,
                    charge_limit_soc: row.get(19)?,
                    battery_range_mi: row.get(20)?,
                    latitude: row.get(21)?,
                    longitude: row.get(22)?,
                    charge_minutes_to_full: row.get(23)?,
                    charging_state: row.get(24)?,
                })
            })()
            .map_err(|e| {
                *self.error.borrow_mut() = Some(e);
                S::Error::custom("telemetry column read failed")
            })?;

            seq.serialize_element(&sample)?;
        }
        seq.end()
    }
}

#[cfg(test)]
mod streaming_export_tests {
    use super::{export_json, import_json};
    use crate::db::DriveStore;
    use crate::types::{FlagRun, GearRun, GpsPoint, StoreData};

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
                &[
                    FlagRun { flags: 3, frames: 1, max_mps: Some(2.7) },
                    FlagRun { flags: 0, frames: 1, max_mps: None },
                ],
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
        assert_eq!(
            data.routes[0].flag_runs,
            vec![
                FlagRun { flags: 3, frames: 1, max_mps: Some(2.7) },
                FlagRun { flags: 0, frames: 1, max_mps: None },
            ],
        );
        // Wire shape matches Sentry-Drive's flagRuns exactly: per-run
        // maxMps present when known, the KEY absent (not null) when not.
        let raw_str = String::from_utf8(raw.clone()).unwrap();
        assert!(
            raw_str.contains(
                r#""flagRuns":[{"flags":3,"frames":1,"maxMps":2.7},{"flags":0,"frames":1}]"#
            ),
            "export json: {raw_str}"
        );
        assert_eq!(data.processed_files, vec!["2025-01-15/clip.mp4"]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn flag_runs_round_trip_import_then_export() {
        // A Sentry-Drive-authored drive-data.json with flagRuns must
        // import into SQLite and re-export byte-equal on that field.
        // Route 1 mixes a modern run (maxMps, current Drive extraction)
        // with a legacy one (no maxMps) — both per-run states must
        // survive, absence staying an ABSENT key, not null. A route
        // WITHOUT flagRuns must stay absent too (unverifiable), not
        // become an empty array.
        let json = r#"{"processedFiles":["2026-07-15/2026-07-15_20-50-43-front.mp4","2026-07-15/2026-07-15_20-51-44-front.mp4"],"routes":[
            {"file":"2026-07-15/2026-07-15_20-50-43-front.mp4","date":"2026-07-15","points":[[37.0,-122.0],[37.0001,-122.0]],"gearStates":"AQE=","autopilotStates":"AAA=","speeds":[1.5,2.7],"accelPositions":[0.0,0.0],"rawParkCount":53,"rawFrameCount":553,"gearRuns":[{"gear":1,"frames":500},{"gear":0,"frames":53}],"flagRuns":[{"flags":0,"frames":471,"maxMps":2.7},{"flags":3,"frames":82}]},
            {"file":"2026-07-15/2026-07-15_20-51-44-front.mp4","date":"2026-07-15","points":[[37.0,-122.0],[37.0001,-122.0]],"gearStates":"AQE=","autopilotStates":"AAA=","speeds":[1.5,2.7],"accelPositions":[0.0,0.0],"rawParkCount":0,"rawFrameCount":600,"gearRuns":[{"gear":1,"frames":600}]}
        ],"driveTags":{}}"#;
        let path = std::env::temp_dir().join(format!(
            "sentryusb-flagruns-roundtrip-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, json).unwrap();

        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        let mut out: Vec<u8> = Vec::new();
        export_json(&conn, &mut out).unwrap();
        let data: crate::types::StoreData = serde_json::from_slice(&out).unwrap();
        assert_eq!(data.routes.len(), 2);
        assert_eq!(
            data.routes[0].flag_runs,
            vec![
                FlagRun { flags: 0, frames: 471, max_mps: Some(2.7) },
                FlagRun { flags: 3, frames: 82, max_mps: None },
            ],
        );
        assert!(
            data.routes[1].flag_runs.is_empty(),
            "route without flagRuns must stay unverifiable"
        );
        let out_str = String::from_utf8(out).unwrap();
        assert!(
            out_str.contains(
                r#""flagRuns":[{"flags":0,"frames":471,"maxMps":2.7},{"flags":3,"frames":82}]"#
            ),
            "export: {out_str}"
        );
        // Absent stays absent for the second route — count the key.
        assert_eq!(out_str.matches(r#""flagRuns""#).count(), 1);

        let _ = std::fs::remove_file(&path);
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

    /// Covers the on-disk path that opens a fresh read-only SQLite handle
    /// rather than locking the writer mutex (the production path used by
    /// `/api/drives/data/export-for-sync`). The `:memory:` tests above
    /// fall through to the locked-connection branch since you can't open
    /// a second handle on an in-memory DB.
    #[test]
    fn streaming_export_from_on_disk_db_uses_readonly_handle() {
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let db_path = std::env::temp_dir()
            .join(format!("sentryusb-readonly-{}-{}.db", pid, nanos));
        let json_path = std::env::temp_dir()
            .join(format!("sentryusb-readonly-{}-{}.json", pid, nanos));
        let db_str = db_path.to_string_lossy().to_string();
        let json_str = json_path.to_string_lossy().to_string();

        let store = DriveStore::open(&db_str).unwrap();
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
                &[
                    FlagRun { flags: 3, frames: 1, max_mps: Some(2.7) },
                    FlagRun { flags: 0, frames: 1, max_mps: None },
                ],
            )
            .unwrap();
        store.export_json_to_file(&json_str).unwrap();

        let raw = std::fs::read(&json_path).unwrap();
        let data: StoreData = serde_json::from_slice(&raw).unwrap();
        assert_eq!(data.routes.len(), 1);
        assert_eq!(data.routes[0].file, "2025-01-15/clip.mp4");
        assert_eq!(data.routes[0].points, pts);
        assert_eq!(data.processed_files, vec!["2025-01-15/clip.mp4"]);

        drop(store);
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{}-wal", db_str));
        let _ = std::fs::remove_file(format!("{}-shm", db_str));
        let _ = std::fs::remove_file(&json_path);
    }
}

#[cfg(test)]
mod import_diagnostics_tests {
    use super::*;
    use rusqlite::Connection;

    /// JSON helper — assembles a `drive-data.json`-shaped payload for tests.
    fn build_json(routes_json: &str) -> String {
        format!(
            r#"{{"processedFiles": [], "routes": [{}], "driveTags": {{}}}}"#,
            routes_json
        )
    }

    /// Returns a minimal route literal with overridable file + points.
    fn route_lit(file: &str, points_json: &str, date: &str) -> String {
        format!(
            r#"{{"file":"{}","date":"{}","points":{},"gearStates":[],"autopilotStates":[],"speeds":[],"accelPositions":[],"rawParkCount":0,"rawFrameCount":0}}"#,
            file, date, points_json
        )
    }

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn diagnostics_flag_empty_points() {
        let json = build_json(&format!(
            "{},{}",
            route_lit("2025-01-15_12-00-00/clip.mp4", "[]", "2025-01-15"),
            route_lit("2025-01-15_12-05-00/clip.mp4", "[[37.0,-122.0]]", "2025-01-15"),
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-empty-points.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        assert_eq!(stats.routes, 2);
        assert_eq!(diag.empty_points, 1);
        assert_eq!(diag.empty_date, 0);
        assert_eq!(diag.duplicate_files_in_json, 0);
        assert!(diag.bad_examples.iter().any(|e| e.contains("12-00-00")));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn diagnostics_flag_duplicate_file_paths_after_normalization() {
        // Two routes whose `file` differs only in slash style. The second
        // overwrites the first via INSERT OR REPLACE — we want this counted.
        let json = build_json(&format!(
            "{},{}",
            route_lit(
                "2025-01-15_12-00-00\\\\clip.mp4",
                "[[37.0,-122.0]]",
                "2025-01-15"
            ),
            route_lit(
                "2025-01-15_12-00-00/clip.mp4",
                "[[37.1,-122.1]]",
                "2025-01-15"
            ),
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-dup-paths.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (_stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        assert_eq!(diag.duplicate_files_in_json, 1);
        assert!(diag.has_problems());

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn diagnostics_clean_import_has_no_problems() {
        let json = build_json(&route_lit(
            "2025-01-15_12-00-00/clip.mp4",
            "[[37.0,-122.0],[37.1,-122.1]]",
            "2025-01-15",
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-clean.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        assert_eq!(stats.routes, 1);
        assert_eq!(diag.seen, 1);
        assert!(!diag.has_problems(), "clean import should not flag problems");

        let _ = std::fs::remove_file(&path);
    }

    /// Telemetry fields in the JSON (as the exporter writes them) must land
    /// in the routes columns — they used to be dropped on import, so a
    /// backup restore lost every BLE telemetry badge.
    #[test]
    fn reimport_preserves_cloud_state_and_ble_rollups() {
        // Onboarding flow: a Pi that has been recording natively (rows
        // with BLE rollups + cloud-upload bookkeeping) later imports a
        // Sentry-Drive backfill of the SAME clips. The import must
        // refresh the clip content but NOT blank the Pi-recorded BLE
        // columns (Sentry-Drive's own processing exports them null) and
        // NOT reset cloud_uploaded_at (which would re-upload the whole
        // library to the cloud as duplicates).
        let native = r#"{"file":"2025-01-15_12-00-00/clip.mp4","date":"2025-01-15","points":[[37.0,-122.0],[37.1,-122.1]],"batteryPctStart":80.5,"batteryPctEnd":79.0,"tireFlPsi":42.5}"#;
        let path = std::env::temp_dir().join("sentryusb-diag-reimport-a.json");
        std::fs::write(&path, build_json(native)).unwrap();
        let mut conn = fresh_conn();
        import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();
        conn.execute(
            "UPDATE routes SET cloud_uploaded_at = 1234, cloud_route_id = 'deadbeef'",
            [],
        )
        .unwrap();

        // Same clip processed fresh elsewhere: richer points, no BLE.
        let reimport = r#"{"file":"2025-01-15_12-00-00/clip.mp4","date":"2025-01-15","points":[[37.0,-122.0],[37.05,-122.05],[37.1,-122.1]]}"#;
        let path2 = std::env::temp_dir().join("sentryusb-diag-reimport-b.json");
        std::fs::write(&path2, build_json(reimport)).unwrap();
        import_json(&mut conn, path2.to_str().unwrap(), |_| {}).unwrap();

        let (pc, bs, fl, cu, cr): (i64, Option<f64>, Option<f64>, Option<i64>, Option<String>) =
            conn.query_row(
                "SELECT point_count, battery_pct_start, tire_fl_psi, \
                        cloud_uploaded_at, cloud_route_id \
                 FROM routes WHERE file = '2025-01-15_12-00-00/clip.mp4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(pc, 3, "content must refresh from the re-import");
        assert_eq!(bs, Some(80.5), "Pi-recorded BLE must survive a null re-import");
        assert_eq!(fl, Some(42.5), "TPMS must survive a null re-import");
        assert_eq!(cu, Some(1234), "cloud bookkeeping must survive (no re-upload sweep)");
        assert_eq!(cr.as_deref(), Some("deadbeef"));

        // ...but a re-import that CARRIES telemetry (a Rusty round-trip)
        // still wins over the stored values.
        let richer = r#"{"file":"2025-01-15_12-00-00/clip.mp4","date":"2025-01-15","points":[[37.0,-122.0],[37.1,-122.1]],"batteryPctStart":70.0}"#;
        let path3 = std::env::temp_dir().join("sentryusb-diag-reimport-c.json");
        std::fs::write(&path3, build_json(richer)).unwrap();
        import_json(&mut conn, path3.to_str().unwrap(), |_| {}).unwrap();
        let bs2: Option<f64> = conn
            .query_row(
                "SELECT battery_pct_start FROM routes WHERE file = '2025-01-15_12-00-00/clip.mp4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bs2, Some(70.0), "non-null incoming telemetry must overwrite");

        // Third-party history riding in the same import (a Tessie or
        // Teslascope drive the Pi has never seen) must land as a NEW row
        // with empty cloud bookkeeping — i.e. queued for cloud upload.
        // That's the onboarding funnel: Sentry-Drive imports the service
        // data, Rusty merges it, the uploader ships it to the cloud.
        let tessie = r#"{"file":"tessie/2025-01-10/2025-01-10_09-00-00-front-tessie-1.mp4","date":"2025-01-10","points":[[37.2,-122.2],[37.3,-122.3]],"source":"tessie"}"#;
        let path4 = std::env::temp_dir().join("sentryusb-diag-reimport-d.json");
        std::fs::write(&path4, build_json(tessie)).unwrap();
        import_json(&mut conn, path4.to_str().unwrap(), |_| {}).unwrap();
        let (src, cu_new): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT source, cloud_uploaded_at FROM routes \
                 WHERE file = 'tessie/2025-01-10/2025-01-10_09-00-00-front-tessie-1.mp4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(src.as_deref(), Some("tessie"));
        assert_eq!(cu_new, None, "new third-party rows must be pending for cloud upload");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&path2);
        let _ = std::fs::remove_file(&path3);
        let _ = std::fs::remove_file(&path4);
    }

    #[test]
    fn import_preserves_telemetry_columns() {
        let route = r#"{"file":"2025-01-15_12-00-00/clip.mp4","date":"2025-01-15","points":[[37.0,-122.0],[37.1,-122.1]],"batteryPctStart":80.5,"batteryPctEnd":79.0,"hvacRuntimeS":30,"odometerMiStart":1000.5,"odometerMiEnd":1001.0,"locationNameStart":"Home St","tireFlPsi":42.5}"#;
        let json = build_json(route);
        let path = std::env::temp_dir().join("sentryusb-diag-telemetry.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (stats, _diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();
        assert_eq!(stats.routes, 1);

        let (bs, be, hvac, odo_s, odo_e, loc, fl): (
            Option<f64>, Option<f64>, Option<i64>, Option<f64>, Option<f64>,
            Option<String>, Option<f64>,
        ) = conn
            .query_row(
                "SELECT battery_pct_start, battery_pct_end, hvac_runtime_s, \
                        odometer_mi_start, odometer_mi_end, location_name_start, tire_fl_psi \
                 FROM routes WHERE file = '2025-01-15_12-00-00/clip.mp4'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
            )
            .unwrap();
        assert_eq!(bs, Some(80.5));
        assert_eq!(be, Some(79.0));
        assert_eq!(hvac, Some(30));
        assert_eq!(odo_s, Some(1000.5));
        assert_eq!(odo_e, Some(1001.0));
        assert_eq!(loc.as_deref(), Some("Home St"));
        assert_eq!(fl, Some(42.5));

        let _ = std::fs::remove_file(&path);
    }

    /// The grouper's timestamp filter (grouper.rs `parse_file_timestamp`) is
    /// the most likely cause of "missing drives on import": a route whose
    /// filename lacks YYYY-MM-DD_HH-MM-SS lands in the routes table but
    /// never appears as a drive. We verify the route IS imported (so logs
    /// + import-history reflect it) — the grouper-side warn is exercised
    /// by integration tests.
    #[test]
    fn route_with_unparseable_filename_still_imports() {
        let json = build_json(&route_lit(
            "weird-file-no-timestamp.mp4",
            "[[37.0,-122.0],[37.1,-122.1]]",
            "2025-01-15",
        ));
        let path = std::env::temp_dir().join("sentryusb-diag-unparseable.json");
        std::fs::write(&path, &json).unwrap();

        let mut conn = fresh_conn();
        let (stats, diag) = import_json(&mut conn, path.to_str().unwrap(), |_| {}).unwrap();

        // import_json itself doesn't validate filenames — that's the
        // grouper's job. The route IS in the DB.
        assert_eq!(stats.routes, 1);
        assert_eq!(diag.empty_points, 0);

        // Confirm the row landed in the routes table.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod full_db_export_tests {
    use super::*;
    use crate::db::DriveStore;
    use crate::types::GpsPoint;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::schema::migrate(&conn).unwrap();
        conn
    }

    fn tmp_json(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "sentryusb-fulldb-{}-{}-{}.json",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    /// Insert one telemetry row exercising every column the sampler
    /// writes (a charging `state` sample) and one sparse
    /// `body_controller` row that only carries ts + source.
    fn seed_telemetry(conn: &Connection) {
        conn.execute(
            "INSERT INTO telemetry_samples (
                ts, battery_pct, battery_temp_c, interior_temp_c, exterior_temp_c,
                hvac_on, source,
                tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
                odometer_mi, software_version, location_name,
                charger_power_kw, charger_actual_current_a, charger_voltage_v,
                charge_rate_mph, charge_energy_added_kwh, charge_limit_soc,
                battery_range_mi, latitude, longitude,
                charge_minutes_to_full, charging_state
            ) VALUES (
                1700000000, 72.5, 21.0, 22.5, 18.0,
                1, 'state',
                42.0, 42.5, 41.5, 41.0,
                12345.6, '2026.2.9.10', '123 Home St',
                11, 48, 240,
                30.5, 7.25, 80,
                210.4, 37.7749, -122.4194,
                95, 'charging'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO telemetry_samples (ts, source) VALUES (1700000060, 'body_controller')",
            [],
        )
        .unwrap();
    }

    /// Export → import into a fresh DB must carry the raw telemetry
    /// samples plus charge tags and cost overrides — the tables the
    /// pre-full-DB export silently dropped, which made an archive
    /// restore lose all charging history and drive chart series.
    #[test]
    fn full_db_roundtrip_restores_telemetry_and_charge_data() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route("2025-01-15/clip.mp4", "2025-01-15", &pts, &[4, 4], &[1, 1], &[25.0, 26.0], &[0.5, 0.6], 0, 2, &[], &[])
            .unwrap();
        store.with_locked_conn(|conn| seed_telemetry(conn));
        store
            .set_charge_tags(1700000000, &["Home".to_string(), "Overnight".to_string()])
            .unwrap();
        store
            .set_charge_cost(1700000000, Some((4.56, "USD".to_string())))
            .unwrap();

        let path = tmp_json("roundtrip");
        let path_str = path.to_string_lossy().to_string();
        store.export_json_to_file(&path_str).unwrap();

        let mut dest = fresh_conn();
        let (stats, _diag) = import_json(&mut dest, &path_str, |_| {}).unwrap();
        assert_eq!(stats.routes, 1);
        assert_eq!(stats.telemetry_samples, 2);
        assert_eq!(stats.charge_tags, 2);
        assert_eq!(stats.charge_costs, 1);

        // Full charging row survives column-for-column.
        let (bp, hvac, src, fl, odo, swv, loc, pkw, amps, volts, rate, kwh, lim, range, lat, lng, mins, cs): (
            Option<f64>, Option<i64>, String, Option<f64>, Option<f64>, Option<String>,
            Option<String>, Option<i64>, Option<i64>, Option<i64>, Option<f64>, Option<f64>,
            Option<i64>, Option<f64>, Option<f64>, Option<f64>, Option<i64>, Option<String>,
        ) = dest
            .query_row(
                "SELECT battery_pct, hvac_on, source, tire_fl_psi, odometer_mi, software_version, \
                        location_name, charger_power_kw, charger_actual_current_a, charger_voltage_v, \
                        charge_rate_mph, charge_energy_added_kwh, charge_limit_soc, battery_range_mi, \
                        latitude, longitude, charge_minutes_to_full, charging_state \
                 FROM telemetry_samples WHERE ts = 1700000000",
                [],
                |r| {
                    Ok((
                        r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?,
                        r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?,
                        r.get(12)?, r.get(13)?, r.get(14)?, r.get(15)?, r.get(16)?, r.get(17)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(bp, Some(72.5));
        assert_eq!(hvac, Some(1));
        assert_eq!(src, "state");
        assert_eq!(fl, Some(42.0));
        assert_eq!(odo, Some(12345.6));
        assert_eq!(swv.as_deref(), Some("2026.2.9.10"));
        assert_eq!(loc.as_deref(), Some("123 Home St"));
        assert_eq!(pkw, Some(11));
        assert_eq!(amps, Some(48));
        assert_eq!(volts, Some(240));
        assert_eq!(rate, Some(30.5));
        assert_eq!(kwh, Some(7.25));
        assert_eq!(lim, Some(80));
        assert_eq!(range, Some(210.4));
        assert_eq!(lat, Some(37.7749));
        assert_eq!(lng, Some(-122.4194));
        assert_eq!(mins, Some(95));
        assert_eq!(cs.as_deref(), Some("charging"));

        // Sparse body-controller row survives with everything else NULL.
        let (bp2, src2): (Option<f64>, String) = dest
            .query_row(
                "SELECT battery_pct, source FROM telemetry_samples WHERE ts = 1700000060",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(bp2, None);
        assert_eq!(src2, "body_controller");

        // Charge tags and cost override survive.
        let tags: Vec<String> = {
            let mut stmt = dest
                .prepare("SELECT tag FROM charge_tags WHERE session_ts = 1700000000 ORDER BY tag")
                .unwrap();
            let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
            rows.map(|r| r.unwrap()).collect()
        };
        assert_eq!(tags, vec!["Home".to_string(), "Overnight".to_string()]);
        let (amount, currency): (f64, Option<String>) = dest
            .query_row(
                "SELECT amount, currency FROM charge_costs WHERE session_ts = 1700000000",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(amount, 4.56);
        assert_eq!(currency.as_deref(), Some("USD"));

        let _ = std::fs::remove_file(&path);
    }

    /// Restoring an older backup over a live DB must never clobber rows
    /// the sampler has written since the backup was taken — same-ts
    /// telemetry rows, charge tags, and cost overrides all keep the
    /// local (newer) value.
    #[test]
    fn import_merge_never_overwrites_existing_rows() {
        let store = DriveStore::open_memory().unwrap();
        store.with_locked_conn(|conn| {
            conn.execute(
                "INSERT INTO telemetry_samples (ts, battery_pct, source) VALUES (500, 99.0, 'state')",
                [],
            )
            .unwrap();
        });
        store.set_charge_cost(500, Some((9.99, "USD".to_string()))).unwrap();
        let path = tmp_json("merge");
        let path_str = path.to_string_lossy().to_string();
        store.export_json_to_file(&path_str).unwrap();

        // Destination already has a different row at the same ts.
        let mut dest = fresh_conn();
        dest.execute(
            "INSERT INTO telemetry_samples (ts, battery_pct, source) VALUES (500, 50.0, 'state')",
            [],
        )
        .unwrap();
        dest.execute(
            "INSERT INTO charge_costs (session_ts, amount, currency) VALUES (500, 1.23, 'EUR')",
            [],
        )
        .unwrap();

        let (stats, _diag) = import_json(&mut dest, &path_str, |_| {}).unwrap();
        assert_eq!(stats.telemetry_samples, 1, "row is counted as seen even when skipped");

        let bp: Option<f64> = dest
            .query_row("SELECT battery_pct FROM telemetry_samples WHERE ts = 500", [], |r| r.get(0))
            .unwrap();
        assert_eq!(bp, Some(50.0), "existing local sample must win over the backup");
        let amount: f64 = dest
            .query_row("SELECT amount FROM charge_costs WHERE session_ts = 500", [], |r| r.get(0))
            .unwrap();
        assert_eq!(amount, 1.23, "existing local cost must win over the backup");

        let _ = std::fs::remove_file(&path);
    }

    /// A pre-full-DB backup (no telemetrySamples/chargeTags/chargeCosts
    /// sections) must keep importing exactly as before.
    #[test]
    fn old_format_backup_without_new_sections_still_imports() {
        let json = r#"{"processedFiles":["2025-01-15/clip.mp4"],"routes":[{"file":"2025-01-15/clip.mp4","date":"2025-01-15","points":[[37.0,-122.0],[37.1,-122.1]]}],"driveTags":{"k":["Sentry"]}}"#;
        let path = tmp_json("oldformat");
        std::fs::write(&path, json).unwrap();

        let mut dest = fresh_conn();
        let (stats, _diag) = import_json(&mut dest, path.to_str().unwrap(), |_| {}).unwrap();
        assert_eq!(stats.routes, 1);
        assert_eq!(stats.telemetry_samples, 0);
        assert_eq!(stats.charge_tags, 0);
        assert_eq!(stats.charge_costs, 0);

        let _ = std::fs::remove_file(&path);
    }

    /// Empty stores must not emit the new sections at all — keeps the
    /// no-telemetry export byte-stable for rsync diffing and older
    /// readers.
    #[test]
    fn export_omits_new_sections_when_tables_empty() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194]];
        store
            .add_route("2025-01-15/clip.mp4", "2025-01-15", &pts, &[4], &[1], &[25.0], &[0.5], 0, 1, &[], &[])
            .unwrap();
        let path = tmp_json("omit");
        let path_str = path.to_string_lossy().to_string();
        store.export_json_to_file(&path_str).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("telemetrySamples"));
        assert!(!raw.contains("chargeTags"));
        assert!(!raw.contains("chargeCosts"));

        let _ = std::fs::remove_file(&path);
    }
}
