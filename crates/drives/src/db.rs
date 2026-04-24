//! Drive data store backed by SQLite — port of Go `server/drives/store.go`.
//!
//! Public API matches the Go `Store` so `crates/api/src/drives_handler.rs`
//! and `crates/drives/src/processor.rs` don't need behavioral changes.
//!
//! Thread-safety: SQLite with WAL handles its own internal locking, but
//! we still keep a `Mutex<Connection>` to serialize writes on the Pi's
//! single-writer setup. The atomic counters give readers a fast-path for
//! `/api/drives/status` polling without taking the lock.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, ToSql};
use tracing::{info, warn};

use crate::aggregate::compute_route_aggregates;
use crate::backfill::backfill_route_aggregates;
use crate::blob::{
    decode_f32s, decode_gear_runs, decode_points, decode_u8s, encode_f32s, encode_gear_runs,
    encode_points, encode_u8s,
};
use crate::schema::{self, meta_get, meta_set};
use crate::syncguard::{self, check_sync_size_guard, read_sync_cache, write_sync_cache};
use crate::types::{GearRun, GpsPoint, Route, RouteAggregates, RouteSummary, StoreData};

/// Default SQLite DB path on the Pi.
pub const DEFAULT_DATA_PATH: &str = "/backingfiles/drive-data.db";

/// JSON staging mirror — regenerated on demand by `ExportJSONForSync` so
/// `post-archive-process.sh` can ship it to the archive server.
pub const DEFAULT_JSON_MIRROR_PATH: &str = "/mutable/drive-data.json";

/// Pre-SQLite data file on the read-only root. The JSON importer reads
/// this on first boot if the primary mirror is missing.
pub const LEGACY_JSON_PATH: &str = "/root/drive-data.json";

/// Archive-side JSON copy for CIFS/NFS mounts.
pub const ARCHIVE_DATA_PATH: &str = "/mnt/archive/drive-data.json";

/// Ordered list of paths the one-shot importer checks on first boot.
/// The first that exists wins.
pub const IMPORT_SOURCE_CANDIDATES: &[&str] = &[
    DEFAULT_JSON_MIRROR_PATH, // /mutable/drive-data.json
    LEGACY_JSON_PATH,         // /root/drive-data.json
];

/// Drive data store.
pub struct DriveStore {
    path: String,
    conn: Mutex<Connection>,
    /// Cached row counts so `/api/drives/status` doesn't hit SQLite for
    /// every poll.
    route_count: AtomicI64,
    processed_count: AtomicI64,
}

impl DriveStore {
    /// Open (or create) the DB at `path`, apply migrations, run the
    /// one-shot JSON→DB import if needed, backfill v2 aggregate columns,
    /// and prime the row-count caches. Equivalent to Go `NewStore(p)
    /// + Load()`.
    pub fn open(path: &str) -> Result<Self> {
        let path = if path.is_empty() {
            DEFAULT_DATA_PATH.to_string()
        } else {
            path.to_string()
        };

        if let Some(parent) = Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() && parent != Path::new("/") {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("open: mkdir {}", parent.display()))?;
            }
        }

        let conn = open_connection(&path)
            .with_context(|| format!("open: sql.Open {}", path))?;

        let store = DriveStore {
            path,
            conn: Mutex::new(conn),
            route_count: AtomicI64::new(0),
            processed_count: AtomicI64::new(0),
        };

        store.load_locked(IMPORT_SOURCE_CANDIDATES)?;

        info!("Drive store opened at {}", store.path);
        Ok(store)
    }

    /// Opens an in-memory DB (for testing). Skips the one-shot JSON
    /// import since there's nothing on disk to import from.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        apply_pragmas(&conn)?;
        let store = DriveStore {
            path: ":memory:".to_string(),
            conn: Mutex::new(conn),
            route_count: AtomicI64::new(0),
            processed_count: AtomicI64::new(0),
        };
        // Still run migrate + backfill so tests exercise the real schema.
        let guard = store.conn.lock().unwrap();
        schema::migrate(&guard)?;
        drop(guard);
        store.refresh_counts()?;
        Ok(store)
    }

    /// Path the store was opened at.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Re-load (re-migrate + re-import). Safe to call multiple times.
    pub fn load(&self) -> Result<()> {
        self.load_locked(IMPORT_SOURCE_CANDIDATES)
    }

    fn load_locked(&self, import_candidates: &[&str]) -> Result<()> {
        {
            let mut guard = self.conn.lock().unwrap();
            schema::migrate(&guard).context("load: migrate")?;
            run_one_shot_import(&mut guard, import_candidates)
                .context("load: one-shot import")?;
            let mut mg = guard;
            let stats =
                backfill_route_aggregates(&mut mg, |done, total| {
                    info!("[drives] Backfilling summary aggregates: {}/{} routes", done, total);
                })
                .context("load: aggregate backfill")?;
            if stats.updated > 0 {
                info!(
                    "[drives] Summary backfill complete: {} routes updated",
                    stats.updated
                );
                meta_set(
                    &mg,
                    "summary_backfilled_at",
                    &chrono::Utc::now().to_rfc3339(),
                )?;
            }
        }
        self.refresh_counts()?;
        Ok(())
    }

    /// Passive WAL checkpoint — called periodically by the processor so
    /// the `-wal` file doesn't grow unbounded during long runs. Errors
    /// are non-fatal (the data is already durable in the WAL).
    pub fn save(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        Ok(())
    }

    /// Return the set of all processed file paths (normalized to forward
    /// slashes). Called once per ProcessDirectory run.
    pub fn processed_set(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT file FROM processed_files")?;
        let files = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(|f| normalize_path(&f))
            .collect();
        Ok(files)
    }

    /// Mark a file processed without adding route data. Idempotent.
    pub fn mark_processed(&self, relative_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_unix();
        conn.execute(
            "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            params![normalize_path(relative_path), now],
        )?;
        drop(conn);
        self.refresh_counts()?;
        Ok(())
    }

    /// True if `file` has been processed.
    pub fn is_processed(&self, file: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM processed_files WHERE file = ?1)",
                params![normalize_path(file)],
                |row| row.get(0),
            )
            .unwrap_or(0);
        Ok(exists != 0)
    }

    /// Add a processed file AND its route data. If `points` is empty the
    /// route row is skipped (the clip is still marked processed). If a
    /// route for `file` already exists it is upserted in place.
    ///
    /// Computes aggregate columns inline so the summary endpoints can
    /// serve BLOB-free rows — matches Go's `Store.AddRoute` semantics
    /// exactly (single source of truth in `aggregate.rs`).
    #[allow(clippy::too_many_arguments)]
    pub fn add_route(
        &self,
        relative_path: &str,
        date_dir: &str,
        points: &[GpsPoint],
        gears: &[u8],
        ap_states: &[u8],
        speeds: &[f32],
        accel_positions: &[f32],
        raw_park_count: u32,
        raw_frame_count: u32,
        gear_runs: &[GearRun],
    ) -> Result<()> {
        let norm = normalize_path(relative_path);
        let now = now_unix();

        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        tx.execute(
            "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            params![norm, now],
        )?;

        if !points.is_empty() {
            let route = Route {
                file: relative_path.to_string(),
                date: date_dir.to_string(),
                points: points.to_vec(),
                gear_states: gears.to_vec(),
                autopilot_states: ap_states.to_vec(),
                speeds: speeds.to_vec(),
                accel_positions: accel_positions.to_vec(),
                raw_park_count,
                raw_frame_count,
                gear_runs: gear_runs.to_vec(),
            };
            let agg = compute_route_aggregates(&route);
            insert_or_update_route(&tx, &norm, &route, &agg, now)?;
        }

        tx.commit()?;
        drop(conn);
        self.refresh_counts()?;
        Ok(())
    }

    /// Cached route count (O(1)).
    pub fn route_count(&self) -> i64 {
        self.route_count.load(Ordering::Relaxed)
    }

    /// Cached processed-files count (O(1)).
    pub fn processed_count(&self) -> i64 {
        self.processed_count.load(Ordering::Relaxed)
    }

    /// Fresh `Vec<Route>` decoded from the DB. Hot-path readers should
    /// use [`with_routes`] instead to avoid the allocation.
    pub fn get_routes(&self) -> Result<Vec<Route>> {
        let conn = self.conn.lock().unwrap();
        select_all_routes(&conn)
    }

    /// Materialize all routes and invoke `f` with the resulting slice.
    /// Slice and elements must not be retained beyond `f`'s return.
    pub fn with_routes<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&[Route]) -> R,
    {
        let routes = self.get_routes()?;
        Ok(f(&routes))
    }

    /// BLOB-free analogue of `with_routes`: materializes per-route
    /// metadata + pre-computed aggregate columns, excluding all point-data
    /// BLOBs. On a 5500-route DB this costs ~5 MB of heap instead of
    /// ~300 MB for the full `WithRoutes` materialization.
    pub fn with_route_summaries<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&[RouteSummary]) -> R,
    {
        let conn = self.conn.lock().unwrap();
        let summaries = select_all_route_summaries(&conn)?;
        Ok(f(&summaries))
    }

    /// Fetch full `Route` rows (with all BLOB columns decoded) for the
    /// named files. Intended for the single-drive detail view: after
    /// [`with_route_summaries`] has identified which ~1-20 clips make up
    /// a drive, this avoids materialising the ~5500-row full store just
    /// to answer a single-drive request. Missing files are silently
    /// skipped — the caller can compare input vs. output lengths if it
    /// needs to detect a tag/race gap.
    pub fn with_routes_by_files<F, R>(&self, files: &[&str], f: F) -> Result<R>
    where
        F: FnOnce(&[Route]) -> R,
    {
        let conn = self.conn.lock().unwrap();
        let routes = select_routes_by_files(&conn, files)?;
        Ok(f(&routes))
    }

    /// Wipe routes + processed_files + drive_tags and bulk-insert `data`.
    /// Used by `POST /api/drives/data/upload` to restore a previously-
    /// downloaded `drive-data.json`.
    pub fn replace_data(&self, data: &StoreData) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for stmt in &[
            "DELETE FROM routes",
            "DELETE FROM processed_files",
            "DELETE FROM drive_tags",
        ] {
            tx.execute(stmt, [])?;
        }
        let now = now_unix();
        let mut seen = std::collections::HashSet::new();
        {
            let mut pf = tx.prepare(
                "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            )?;
            for f in &data.processed_files {
                let n = normalize_path(f);
                if !seen.insert(n.clone()) {
                    continue;
                }
                pf.execute(params![n, now])?;
            }

            for r in &data.routes {
                let n = normalize_path(&r.file);
                let agg = compute_route_aggregates(r);
                insert_or_update_route(&tx, &n, r, &agg, now)?;
                if seen.insert(n.clone()) {
                    pf.execute(params![n, now])?;
                }
            }
        }
        {
            let mut ts = tx.prepare(
                "INSERT OR IGNORE INTO drive_tags(drive_key, tag) VALUES(?1, ?2)",
            )?;
            for (key, tags) in &data.drive_tags {
                for t in tags {
                    ts.execute(params![key, t])?;
                }
            }
        }
        tx.commit()?;
        drop(conn);
        self.refresh_counts()?;
        Ok(())
    }

    /// Full store snapshot — all routes + processed files + tags. Used
    /// by `GET /api/drives/data/download`. Allocates the whole payload.
    pub fn get_data(&self) -> Result<StoreData> {
        let conn = self.conn.lock().unwrap();

        let routes = select_all_routes(&conn)?;

        let mut processed_files = Vec::new();
        {
            let mut stmt = conn
                .prepare("SELECT file FROM processed_files ORDER BY file")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            for r in rows {
                processed_files.push(r?);
            }
        }

        let mut drive_tags = std::collections::HashMap::<String, Vec<String>>::new();
        {
            let mut stmt = conn.prepare("SELECT drive_key, tag FROM drive_tags")?;
            let rows = stmt
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
            for r in rows {
                let (key, tag) = r?;
                drive_tags.entry(key).or_default().push(tag);
            }
        }

        Ok(StoreData {
            processed_files,
            routes,
            drive_tags,
        })
    }

    /// Replace the tags for `drive_key`. Empty/zero-length `tags` drops
    /// the entry entirely.
    pub fn set_drive_tags(&self, drive_key: &str, tags: &[String]) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM drive_tags WHERE drive_key = ?1",
            params![drive_key],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO drive_tags(drive_key, tag) VALUES(?1, ?2)",
            )?;
            for t in tags {
                if t.is_empty() {
                    continue;
                }
                stmt.execute(params![drive_key, t])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Tags for a drive, or an empty vec.
    pub fn get_drive_tags(&self, drive_key: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT tag FROM drive_tags WHERE drive_key = ?1 ORDER BY tag",
        )?;
        let out = stmt
            .query_map(params![drive_key], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(out)
    }

    /// Full drive_key → tags map.
    pub fn get_all_drive_tags(&self) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT drive_key, tag FROM drive_tags ORDER BY drive_key, tag",
        )?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut out = std::collections::HashMap::<String, Vec<String>>::new();
        for r in rows {
            let (k, t) = r?;
            out.entry(k).or_default().push(t);
        }
        Ok(out)
    }

    /// Every tag name in use, sorted and deduplicated.
    pub fn get_all_tag_names(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare("SELECT DISTINCT tag FROM drive_tags ORDER BY tag")?;
        let tags = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tags)
    }

    /// Empty `processed_files` so every clip becomes eligible for
    /// re-extraction. Routes and drive_tags are preserved.
    pub fn clear_processed_for_reprocess(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM processed_files", [])?;
        drop(conn);
        self.refresh_counts()?;
        Ok(())
    }

    /// Regenerate the canonical `/mutable/drive-data.json` mirror for
    /// `post-archive-process.sh`. Idempotent; safe alongside reads.
    pub fn export_json_for_sync(&self) -> Result<()> {
        self.export_json_to_file(DEFAULT_JSON_MIRROR_PATH)
    }

    /// Import a drive-data.json file into the store. Thin wrapper around
    /// [`json_compat::import_json`](crate::json_compat::import_json) that
    /// takes care of locking the shared connection.
    pub fn import_json_file(
        &self,
        path: &str,
    ) -> Result<crate::json_compat::ImportStats> {
        self.import_json_file_with_progress(path, |_| {})
    }

    /// Like [`import_json_file`] but invokes `on_progress(routes_seen)`
    /// once the decoder knows the total route count. Used by the
    /// upload handler to forward `drive_import` WebSocket broadcasts
    /// so the web UI can show the user progress instead of a stale
    /// spinner during a large restore.
    pub fn import_json_file_with_progress<F: Fn(usize)>(
        &self,
        path: &str,
        on_progress: F,
    ) -> Result<crate::json_compat::ImportStats> {
        let stats = {
            let mut conn = self.conn.lock().unwrap();
            crate::json_compat::import_json(&mut conn, path, on_progress)?
        };
        self.refresh_counts()?;
        Ok(stats)
    }

    /// Export current DB contents as `drive-data.json` at `path`.
    /// Atomic via tmp + rename.
    pub fn export_json_to_file(&self, path: &str) -> Result<()> {
        if let Some(dir) = Path::new(path).parent() {
            if !dir.as_os_str().is_empty() && dir != Path::new("/") {
                std::fs::create_dir_all(dir)?;
            }
        }
        let tmp = format!("{}.tmp", path);
        {
            let conn = self.conn.lock().unwrap();
            let mut f = std::fs::File::create(&tmp)?;
            crate::json_compat::export_json(&conn, &mut f)
                .context("export_json")?;
            use std::io::Write;
            f.flush()?;
            f.sync_all()?;
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Regenerate the JSON mirror and copy it to `/mnt/archive/drive-data.json`
    /// with the size-guard applied. No-op if `/mnt/archive` is not a
    /// mounted filesystem.
    pub fn sync_to_archive(&self) -> Result<()> {
        if !Path::new("/mnt/archive").exists() {
            return Ok(());
        }
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            if !mounts.contains("/mnt/archive") {
                return Ok(());
            }
        }

        self.export_json_for_sync()?;
        sync_to_path(
            DEFAULT_JSON_MIRROR_PATH,
            ARCHIVE_DATA_PATH,
            syncguard::DEFAULT_CACHE_PATH,
        )
    }

    /// Copy `/mnt/archive/drive-data.json` to `/mutable/drive-data.json`
    /// so the next `Load()` picks it up via the one-shot importer.
    /// Useful after reflashing a Pi that still has an archive backup.
    pub fn restore_from_archive(&self) -> Result<()> {
        if !Path::new(ARCHIVE_DATA_PATH).exists() {
            return Ok(());
        }
        // Don't restore if we already have local data — the importer
        // would skip it anyway, and we'd rather not churn disk.
        if Path::new(DEFAULT_JSON_MIRROR_PATH).exists() {
            return Ok(());
        }
        let src = std::fs::read(ARCHIVE_DATA_PATH).unwrap_or_default();
        if let Some(dir) = Path::new(DEFAULT_JSON_MIRROR_PATH).parent() {
            if !dir.as_os_str().is_empty() && dir != Path::new("/") {
                std::fs::create_dir_all(dir)?;
            }
        }
        std::fs::write(DEFAULT_JSON_MIRROR_PATH, &src)?;
        info!(
            "[drives] Restored drive-data.json from archive ({} bytes); next Load() will import it",
            src.len()
        );
        Ok(())
    }

    /// Refresh the cached row counts. Called after every mutation.
    fn refresh_counts(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let rc: i64 = conn.query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))?;
        let pc: i64 =
            conn.query_row("SELECT COUNT(*) FROM processed_files", [], |r| r.get(0))?;
        self.route_count.store(rc, Ordering::Relaxed);
        self.processed_count.store(pc, Ordering::Relaxed);
        Ok(())
    }
}

// -----------------------------------------------------------------------------
// SQL helpers (private)
// -----------------------------------------------------------------------------

fn open_connection(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

/// Insert-or-update a single route row with all v2 aggregate columns.
/// Caller is inside a transaction.
fn insert_or_update_route(
    tx: &rusqlite::Transaction,
    norm_file: &str,
    r: &Route,
    a: &RouteAggregates,
    now: i64,
) -> Result<()> {
    let pb = encode_points(Some(&r.points));
    let gb = encode_u8s(Some(&r.gear_states));
    let ab = encode_u8s(Some(&r.autopilot_states));
    let sb = encode_f32s(Some(&r.speeds));
    let acb = encode_f32s(Some(&r.accel_positions));
    let rb = encode_gear_runs(Some(&r.gear_runs));

    let first_lat: Option<f64> = r.points.first().map(|p| p[0]);
    let first_lon: Option<f64> = r.points.first().map(|p| p[1]);

    let point_count = r.points.len() as i64;

    // 34 parameters — matches Go's INSERT ... VALUES() positional binding.
    let p: Vec<Box<dyn ToSql>> = vec![
        Box::new(norm_file.to_string()),
        Box::new(r.date.clone()),
        Box::new(point_count),
        Box::new(r.raw_park_count as i64),
        Box::new(r.raw_frame_count as i64),
        Box::new(a.distance_m),
        Box::new(first_lat),
        Box::new(first_lon),
        Box::new(pb),
        Box::new(gb),
        Box::new(ab),
        Box::new(sb),
        Box::new(acb),
        Box::new(rb),
        Box::new(now),
        Box::new(a.max_speed_mps),
        Box::new(a.avg_speed_mps),
        Box::new(a.speed_sample_count),
        Box::new(a.valid_point_count),
        Box::new(a.fsd_engaged_ms),
        Box::new(a.autosteer_engaged_ms),
        Box::new(a.tacc_engaged_ms),
        Box::new(a.fsd_distance_m),
        Box::new(a.autosteer_distance_m),
        Box::new(a.tacc_distance_m),
        Box::new(a.assisted_distance_m),
        Box::new(a.fsd_disengagements),
        Box::new(a.fsd_accel_pushes),
        Box::new(a.start_lat),
        Box::new(a.start_lng),
        Box::new(a.end_lat),
        Box::new(a.end_lng),
    ];
    let refs: Vec<&dyn ToSql> = p.iter().map(|b| &**b as &dyn ToSql).collect();

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
            start_lat, start_lon, end_lat, end_lon)
         VALUES(
            ?1, ?2, ?3, ?4, ?5,
            NULL, NULL, ?6, ?7, ?8,
            ?9, ?10, ?11, ?12, ?13, ?14, ?15,
            ?16, ?17, ?18, ?19,
            ?20, ?21, ?22,
            ?23, ?24, ?25, ?26,
            ?27, ?28,
            ?29, ?30, ?31, ?32)
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
            end_lon             = excluded.end_lon",
        refs.as_slice(),
    )?;
    Ok(())
}

/// Select all routes into `Vec<Route>` — fully decoded BLOB columns.
fn select_all_routes(conn: &Connection) -> Result<Vec<Route>> {
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
        let points = decode_points(pb.as_deref())
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
        let gear_runs = decode_gear_runs(rb.as_deref())
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

/// Select full routes for a specific set of files. Uses an IN (...) clause
/// bound with positional parameters so the query planner can still use the
/// `file` primary-key index. Falls back to empty when `files` is empty
/// (SQLite disallows `IN ()`).
fn select_routes_by_files(conn: &Connection, files: &[&str]) -> Result<Vec<Route>> {
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat("?").take(files.len()).collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT file, date_dir, raw_park_count, raw_frame_count,
                points_blob, gear_states_blob, ap_states_blob,
                speeds_blob, accel_blob, gear_runs_blob
         FROM routes
         WHERE file IN ({})
         ORDER BY file",
        placeholders
    );
    // The normalized path is what's stored in the routes table; callers
    // pass already-normalized strings (from RouteSummary.file, which came
    // out of the same column).
    let normalized: Vec<String> = files.iter().map(|f| normalize_path(f)).collect();
    let params: Vec<&dyn ToSql> = normalized.iter().map(|s| s as &dyn ToSql).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |row| {
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
            pb, gb, ab, sb, acb, rb,
        ))
    })?;

    let mut out = Vec::with_capacity(files.len());
    for r in rows {
        let (file, date, raw_park_count, raw_frame_count, pb, gb, ab, sb, acb, rb) = r?;
        let points = decode_points(pb.as_deref())
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
        let gear_runs = decode_gear_runs(rb.as_deref())
            .with_context(|| format!("decode gear_runs {}", file))?
            .unwrap_or_default();
        out.push(Route {
            file, date, points, gear_states, autopilot_states,
            speeds, accel_positions, raw_park_count, raw_frame_count, gear_runs,
        });
    }
    Ok(out)
}

/// Select BLOB-free summary rows — metadata + v2 aggregate columns only.
fn select_all_route_summaries(conn: &Connection) -> Result<Vec<RouteSummary>> {
    let mut stmt = conn.prepare(
        "SELECT file, date_dir, raw_park_count, raw_frame_count, gear_runs_blob,
                distance_m, max_speed_mps, avg_speed_mps, speed_sample_count,
                valid_point_count, fsd_engaged_ms, autosteer_engaged_ms,
                tacc_engaged_ms, fsd_distance_m, autosteer_distance_m,
                tacc_distance_m, assisted_distance_m,
                fsd_disengagements, fsd_accel_pushes,
                start_lat, start_lon, end_lat, end_lon
         FROM routes
         ORDER BY file",
    )?;
    let rows = stmt.query_map([], |row| {
        let rb: Option<Vec<u8>> = row.get(4)?;
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)? as u32,
            row.get::<_, i64>(3)? as u32,
            rb,
            row.get::<_, Option<f64>>(5)?,
            row.get::<_, Option<f64>>(6)?,
            row.get::<_, Option<f64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, Option<i64>>(9)?,
            row.get::<_, Option<i64>>(10)?,
            row.get::<_, Option<i64>>(11)?,
            row.get::<_, Option<i64>>(12)?,
            row.get::<_, Option<f64>>(13)?,
            row.get::<_, Option<f64>>(14)?,
            row.get::<_, Option<f64>>(15)?,
            row.get::<_, Option<f64>>(16)?,
            row.get::<_, Option<i64>>(17)?,
            row.get::<_, Option<i64>>(18)?,
            row.get::<_, Option<f64>>(19)?,
            row.get::<_, Option<f64>>(20)?,
            row.get::<_, Option<f64>>(21)?,
            row.get::<_, Option<f64>>(22)?,
        ))
    })?;

    let mut out = Vec::new();
    for r in rows {
        let (
            file,
            date,
            raw_park_count,
            raw_frame_count,
            rb,
            distance_m,
            max_speed_mps,
            avg_speed_mps,
            speed_sample_count,
            valid_point_count,
            fsd_engaged_ms,
            autosteer_engaged_ms,
            tacc_engaged_ms,
            fsd_distance_m,
            autosteer_distance_m,
            tacc_distance_m,
            assisted_distance_m,
            fsd_disengagements,
            fsd_accel_pushes,
            start_lat,
            start_lon,
            end_lat,
            end_lon,
        ) = r?;

        let gear_runs = decode_gear_runs(rb.as_deref())
            .with_context(|| format!("decode gear_runs {}", file))?
            .unwrap_or_default();

        out.push(RouteSummary {
            file,
            date,
            raw_park_count,
            raw_frame_count,
            gear_runs,
            aggregates: RouteAggregates {
                distance_m: distance_m.unwrap_or(0.0),
                max_speed_mps: max_speed_mps.unwrap_or(0.0),
                avg_speed_mps: avg_speed_mps.unwrap_or(0.0),
                speed_sample_count: speed_sample_count.unwrap_or(0),
                valid_point_count: valid_point_count.unwrap_or(0),
                fsd_engaged_ms: fsd_engaged_ms.unwrap_or(0),
                autosteer_engaged_ms: autosteer_engaged_ms.unwrap_or(0),
                tacc_engaged_ms: tacc_engaged_ms.unwrap_or(0),
                fsd_distance_m: fsd_distance_m.unwrap_or(0.0),
                autosteer_distance_m: autosteer_distance_m.unwrap_or(0.0),
                tacc_distance_m: tacc_distance_m.unwrap_or(0.0),
                assisted_distance_m: assisted_distance_m.unwrap_or(0.0),
                fsd_disengagements: fsd_disengagements.unwrap_or(0) as i32,
                fsd_accel_pushes: fsd_accel_pushes.unwrap_or(0) as i32,
                start_lat,
                start_lng: start_lon,
                end_lat,
                end_lng: end_lon,
            },
        });
    }
    Ok(out)
}

// -----------------------------------------------------------------------------
// Import dance + path helpers
// -----------------------------------------------------------------------------

fn run_one_shot_import(conn: &mut Connection, candidates: &[&str]) -> Result<()> {
    if let Some(v) = meta_get(conn, "imported_from_json_at")? {
        if !v.is_empty() {
            return Ok(()); // already imported
        }
    }

    let mut source: Option<&str> = None;
    let mut also_present: Vec<&str> = Vec::new();
    for p in candidates {
        if let Ok(info) = std::fs::metadata(p) {
            if !info.is_dir() {
                if source.is_none() {
                    source = Some(p);
                } else {
                    also_present.push(p);
                }
            }
        }
    }
    if !also_present.is_empty() {
        warn!(
            "[drives] Multiple drive-data.json candidates exist; importing {} and ignoring {:?}. Delete the unused file(s) to silence this warning.",
            source.unwrap(),
            also_present
        );
    }

    let Some(source_path) = source else {
        // True fresh install — mark so we don't keep checking.
        info!("[drives] No legacy drive-data.json found; treating as fresh install");
        meta_set(conn, "imported_from_json_at", &chrono::Utc::now().to_rfc3339())?;
        return Ok(());
    };

    info!("[drives] Importing legacy JSON from {}", source_path);
    let stats =
        crate::json_compat::import_json(conn, source_path, |routes_imported| {
            info!("[drives] Import progress: {} routes", routes_imported);
        })
        .with_context(|| format!("import_json {}", source_path))?;
    info!(
        "[drives] Import complete: {} routes, {} processed files, {} tags",
        stats.routes, stats.processed_files, stats.drive_tags
    );

    // Set the marker BEFORE renaming. If we die between these two steps,
    // the worst outcome on next boot is an orphan JSON left alone (the
    // marker is set → no double-import).
    meta_set(conn, "imported_from_json_at", &chrono::Utc::now().to_rfc3339())?;

    let bak_path = {
        let ts = chrono::Utc::now().timestamp();
        format!("{}.bak-{}-{:04x}", source_path, ts, rand_suffix4())
    };
    if let Err(e) = rename_or_copy(source_path, &bak_path) {
        warn!(
            "[drives] Import succeeded but failed to archive {} -> {}: {}",
            source_path, bak_path, e
        );
    } else {
        info!(
            "[drives] Renamed source JSON to {} (backup; safe to delete after verifying drives page)",
            bak_path
        );
    }
    Ok(())
}

fn rand_suffix4() -> u16 {
    // Simple xorshift on a nanosecond clock — not security-sensitive.
    let mut t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    t ^= t >> 13;
    t ^= t << 7;
    t ^= t >> 17;
    (t & 0xffff) as u16
}

fn rename_or_copy(src: &str, dst: &str) -> Result<()> {
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    // Cross-filesystem fallback.
    let data = std::fs::read(src)?;
    std::fs::write(dst, &data)?;
    // Best-effort fsync the destination so a crash doesn't lose data.
    if let Ok(f) = std::fs::File::open(dst) {
        let _ = f.sync_all();
    }
    std::fs::remove_file(src)?;
    Ok(())
}

/// Atomic sync of `src` → `dst` with size-guard gated by `cache_path`.
fn sync_to_path(src: &str, dst: &str, cache_path: &str) -> Result<()> {
    let src_meta = std::fs::metadata(src)?;
    let new_size = src_meta.len() as i64;

    let last_size = read_sync_cache(cache_path);
    if let Err(e) = check_sync_size_guard(new_size, last_size) {
        warn!("[drives] {}", e);
        return Err(e.into());
    }

    if let Some(dir) = Path::new(dst).parent() {
        if !dir.as_os_str().is_empty() && dir != Path::new("/") {
            std::fs::create_dir_all(dir)?;
        }
    }
    let tmp = format!("{}.tmp", dst);
    let copied = std::fs::copy(src, &tmp)?;

    // Short-copy detection: if src shrank between stat and copy, do NOT
    // poison the size-guard cache with the truncated length.
    if copied as i64 != new_size {
        let _ = std::fs::remove_file(&tmp);
        anyhow::bail!(
            "sync_to_path: short copy ({} of {} bytes); refusing to poison size-guard cache",
            copied,
            new_size
        );
    }

    if let Err(e) = rename_or_copy(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if let Err(e) = write_sync_cache(cache_path, new_size) {
        warn!(
            "[drives] Failed to update sync-size cache at {}: {}",
            cache_path, e
        );
    }
    info!("[drives] Synced drive data to archive ({} bytes)", new_size);
    Ok(())
}

/// Convert backslashes to forward slashes so Windows-shaped paths
/// collide with their POSIX equivalents in `processed_files` and
/// `routes`. Matches Go's `normalizePath`.
pub fn normalize_path(p: &str) -> String {
    p.replace('\\', "/")
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// -----------------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_route_is_bit_identical() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15/clip-front.mp4",
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
        let routes = store.get_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].points, pts);
        assert_eq!(routes[0].gear_states, vec![4, 4]);
        assert_eq!(routes[0].autopilot_states, vec![1, 1]);
        assert_eq!(routes[0].speeds, vec![25.0, 26.0]);
        assert_eq!(routes[0].accel_positions, vec![0.5, 0.6]);
        assert_eq!(routes[0].raw_frame_count, 2);
        assert_eq!(routes[0].gear_runs.len(), 1);
    }

    #[test]
    fn route_summary_reads_precomputed_aggregates() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4194]];
        store
            .add_route(
                "a.mp4", "2025-01-01", &pts, &[], &[], &[], &[], 0, 2, &[],
            )
            .unwrap();
        let out = store.with_route_summaries(|s| s.to_vec()).unwrap();
        assert_eq!(out.len(), 1);
        // Distance should be > 0 since AddRoute populated the aggregate
        // column from the BLOB via ComputeRouteAggregates.
        assert!(out[0].aggregates.distance_m > 0.0);
    }

    #[test]
    fn path_normalization_dedups_windows_and_posix() {
        let store = DriveStore::open_memory().unwrap();
        store.mark_processed("2025\\01\\clip.mp4").unwrap();
        store.mark_processed("2025/01/clip.mp4").unwrap();
        assert_eq!(store.processed_count(), 1);
        assert!(store.is_processed("2025\\01\\clip.mp4").unwrap());
        assert!(store.is_processed("2025/01/clip.mp4").unwrap());
    }

    #[test]
    fn tags_set_and_get() {
        let store = DriveStore::open_memory().unwrap();
        store
            .set_drive_tags(
                "drive1",
                &["Work".to_string(), "Commute".to_string()],
            )
            .unwrap();
        let tags = store.get_drive_tags("drive1").unwrap();
        assert_eq!(tags, vec!["Commute".to_string(), "Work".to_string()]);
    }

    #[test]
    fn replace_data_wipes_and_reinserts() {
        let store = DriveStore::open_memory().unwrap();
        store.mark_processed("old.mp4").unwrap();
        assert_eq!(store.processed_count(), 1);

        let data = StoreData {
            processed_files: vec!["new.mp4".to_string()],
            routes: vec![],
            drive_tags: std::collections::HashMap::new(),
        };
        store.replace_data(&data).unwrap();
        assert_eq!(store.processed_count(), 1);
        assert!(!store.is_processed("old.mp4").unwrap());
        assert!(store.is_processed("new.mp4").unwrap());
    }
}
