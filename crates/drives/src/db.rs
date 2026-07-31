//! Drive data store backed by SQLite.
//!
//! Thread-safety: WAL handles SQLite's internal locking, but a
//! `Mutex<Connection>` still serializes writes for the Pi's single-writer
//! setup. Atomic counters give `/api/drives/status` polling a lock-free
//! fast path.

use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, ToSql};

/// Upsert a cloud-sync dirty row. `changed_at` is local wall-clock ms —
/// used for last-writer-wins against the cloud, clamped server-side so
/// a bad clock can't win forever.
fn mark_mutable_dirty(conn: &Connection, kind: &str, key: &str) -> rusqlite::Result<usize> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    conn.execute(
        "INSERT INTO mutable_dirty(kind, key, changed_at) VALUES(?1, ?2, ?3) \
         ON CONFLICT(kind, key) DO UPDATE SET changed_at = excluded.changed_at",
        params![kind, key, now_ms],
    )
}
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
/// `post-archive-process.sh` can ship it to the archive server. Lives on
/// `/backingfiles` (same partition as the DB) because the export can
/// reach hundreds of MB on a long-used Pi and the 2 GB `/mutable` partition
/// can't hold two copies during atomic write. The local copy is retained
/// after upload so rsync's delta-transfer protocol only sends changed bytes.
pub const DEFAULT_JSON_MIRROR_PATH: &str = "/backingfiles/drive-data.json";

/// Pre-SQLite data file on the read-only root. The JSON importer reads
/// this on first boot if the primary mirror is missing.
pub const LEGACY_JSON_PATH: &str = "/root/drive-data.json";

/// Archive-side JSON copy for CIFS/NFS mounts.
pub const ARCHIVE_DATA_PATH: &str = "/mnt/archive/drive-data.json";

/// Meta-table key: `route_count` at the last successful archive sync.
/// `sync_to_archive` skips the multi-minute JSON export when the live
/// count still matches this baseline (the CIFS/NFS analogue of the
/// shell script's drives_count short-circuit for rsync/rclone).
const ARCHIVE_SYNC_ROUTE_COUNT_KEY: &str = "archive_sync_route_count";

/// Meta-table key: `telemetry_samples` count at the last successful
/// archive sync. A charging-only day grows telemetry without moving the
/// route count, so the skip check needs both baselines — otherwise the
/// archive backup would never pick up new charging history until the
/// next drive.
const ARCHIVE_SYNC_SAMPLE_COUNT_KEY: &str = "archive_sync_sample_count";
// Bump on every drive-list-shape change so existing on-disk caches
// rebuild on first boot after upgrade.
//
// v3 (2026-04-28): switched cache from BLOB grouper (group_clips) to
// summary grouper (group_summary_clips), added hide_tessie_overlapping_sei
// filter, and changed `DriveSummary.date` to derive from start_time
// (was the raw date_dir column). Aligns the list endpoint with what
// the single-drive endpoint already does, so clicking a drive in the
// list returns the matching points.
//
// v4 (2026-05-19): drive grouping output changed since v3 without a bump —
// SavedClips/SentryClips event folders are now skipped, and the grouper was
// reworked to match Sentry-Drive/Sentry-Cloud distance/AP math. Same routes
// now yield a different drive list, so stale v3 caches must rebuild.
//
// v5 (2026-06-09): distance moved from spherical haversine to WGS-84
// geodesic (Andoyer–Lambert) in calc.rs, so every per-drive distance shifts
// 0.1–0.3%. Stale v4 caches hold haversine-era mileage and must rebuild.
//
// v6 (2026-06-12): clip-boundary resolution for FSD disengagements and
// accel pushes in the grouper (see V15_ROUTE_BOUNDARY_COLUMNS), plus
// FSD analytics narrowed from "not tessie" to SEI-only sources. Stale
// v5 caches hold seam-inflated disengagement counts.
// v7 (2026-06-13): geodesic central angle moved from acos(law-of-cosines)
// to the numerically-stable atan2(haversine) form (calc.rs), fixing a
// ~0.7% short-segment undercount on native dashcam drives. Per-drive
// distance shifts up; stale v6 caches hold the undercounted mileage.
//
// v8 (2026-07-06): event-clip gap-fill — the summary grouper now admits
// SavedClips/SentryClips rows that fill RecentClips holes (and filters
// all other event rows, which it previously passed through untouched).
const DRIVE_LIST_CACHE_ALGO_VERSION: &str = "8";

/// Version tag for the per-clip aggregate FORMULA (compute_route_aggregates).
/// Distinct from the cache algo version above: this gates a one-shot
/// recompute of the persisted `routes` aggregate columns when the math that
/// produced them changes. Bump on any change to compute_route_aggregates'
/// numeric output. "geodesic-1" = the WGS-84 distance migration.
/// "boundary-2" = clip-boundary state export (v15 columns) + the
/// engaged-at-start accel-push grace fix + pending-disengagement flush
/// no longer counting at clip seams. "boundary-3" adds `ap_at_start`
/// so the grouper can attribute inter-clip bridge distance and seam
/// wall-time to the incoming clip's autopilot mode. "geodesic-stable-4"
/// recomputes per-clip distances with the numerically-stable geodesic
/// central angle (calc.rs) that fixes the short-segment undercount.
const AGGREGATE_FORMULA_VERSION: &str = "geodesic-stable-4";

/// Version tag for the route `file` KEY format. Gates a one-shot rewrite of
/// `routes.file` / `processed_files.file` to the canonical form (see
/// [`normalize_path`]): the Pi's native processor ingests clips under the
/// snapshot symlink layout (`RecentClips/YYYY-MM-DD/x.mp4`) while
/// Sentry-Drive exports key the same clip `YYYY-MM-DD/x.mp4` — before
/// canon-1 the same physical clip could sit in `routes` under both
/// spellings and every drive over it double-counted distance.
const ROUTE_KEY_FORMAT_VERSION: &str = "canon-1";

/// Ordered list of paths the one-shot importer checks on first boot.
/// The first that exists wins. `/mutable/drive-data.json` is kept as a
/// fallback so upgraders whose DB is still empty (mid-migration) still
/// get their legacy export imported — once the marker is set, this list
/// never runs again, and `cleanup_legacy_mutable_files` clears the
/// orphaned file in the steady-state.
pub const IMPORT_SOURCE_CANDIDATES: &[&str] = &[
    DEFAULT_JSON_MIRROR_PATH,        // /backingfiles/drive-data.json (new canonical)
    "/mutable/drive-data.json",      // legacy pre-2026-05 location (upgrade fallback)
    LEGACY_JSON_PATH,                // /root/drive-data.json (pre-SQLite)
];

/// Files this binary used to write under `/mutable` that are now obsolete and
/// are safe to delete at startup. Limited to the `drive-data.json` family
/// because the importer in `DriveStore::open` consumes the legacy
/// `/mutable/drive-data.json` (if any) *before* this cleanup runs, so the
/// data is already migrated into the DB. Other legacy paths (notification
/// history, preferences) are deliberately left alone — they're tiny and
/// some Rust code still reads them as a lazy fallback for upgraders whose
/// data only lives at the legacy location.
const LEGACY_MUTABLE_ORPHANS: &[&str] = &[
    "/mutable/drive-data.json",     // moved to /backingfiles/drive-data.json
    "/mutable/drive-data.json.tmp", // half-written atomic-rename leftover
];

/// Remove orphaned `/mutable` files left behind by older binaries that wrote
/// state to paths since moved to `/backingfiles`. Best-effort and idempotent:
/// missing files are silently skipped, removal failures are logged but never
/// abort startup. Safe to call on every boot. Sized so calling once at startup
/// is enough to keep the 2 GB `/mutable` partition stable across upgrades.
pub fn cleanup_legacy_mutable_files() {
    use std::path::Path;
    for path in LEGACY_MUTABLE_ORPHANS {
        if !Path::new(path).exists() {
            continue;
        }
        match std::fs::remove_file(path) {
            Ok(()) => tracing::info!(
                "cleanup_legacy_mutable_files: removed orphaned {}",
                path
            ),
            Err(e) => tracing::warn!(
                "cleanup_legacy_mutable_files: failed to remove {}: {}",
                path,
                e
            ),
        }
    }
}

/// Drive data store.
pub struct DriveStore {
    path: String,
    conn: Mutex<Connection>,
    /// Cached row counts so `/api/drives/status` doesn't hit SQLite for
    /// every poll.
    route_count: AtomicI64,
    processed_count: AtomicI64,
    /// Set whenever routes or tags change. `get_cached_drives_json` rebuilds
    /// and clears this flag before serving. Using a flag rather than
    /// rebuilding on every `add_route` call avoids O(n²) work when the
    /// processor adds hundreds of clips in a batch.
    drive_cache_dirty: AtomicBool,
    /// Serializes cache rebuilds so concurrent dirty reads don't each run
    /// the CPU-heavy grouper; the second caller waits, then serves the
    /// fresh cache without recomputing. Lock order: this is always taken
    /// BEFORE `conn` and never while holding it — deadlock-free.
    rebuild_lock: Mutex<()>,
    /// Cached `/api/drives/routes` overview JSON; rebuilt lazily on the first
    /// request after a route mutation.
    route_overview_cache: Mutex<Option<RouteOverviewCache>>,
    /// Bumped on every route mutation. New mutation paths must bump it too.
    route_overview_gen: AtomicU64,
}

/// Valid while `generation` and `max_points` match the request.
struct RouteOverviewCache {
    generation: u64,
    max_points: usize,
    json: String,
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
            drive_cache_dirty: AtomicBool::new(true),
            rebuild_lock: Mutex::new(()),
            route_overview_cache: Mutex::new(None),
            route_overview_gen: AtomicU64::new(0),
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
            drive_cache_dirty: AtomicBool::new(false),
            rebuild_lock: Mutex::new(()),
            route_overview_cache: Mutex::new(None),
            route_overview_gen: AtomicU64::new(0),
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

    /// Escape hatch for crates that need targeted SQL access without
    /// extending the public DriveStore API. Used by the cloud-uploader
    /// crate to read pending-upload rows and stamp `cloud_uploaded_at`
    /// without polluting this crate with cloud-specific methods.
    ///
    /// Holds the same connection mutex everything else uses, so callers
    /// share WAL serialization with `add_route` / `save` / etc. Keep the
    /// closure short — long-running work blocks all other DB I/O.
    pub fn with_locked_conn<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        let guard = self.conn.lock().unwrap();
        f(&guard)
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

            // Route-key format gate. `normalize_path` canonicalizes the
            // snapshot symlink layout (`RecentClips/YYYY-MM-DD/x.mp4`) down
            // to the date-dir form Sentry-Drive exports use
            // (`YYYY-MM-DD/x.mp4`), but rows written before canon-1 still
            // carry the prefixed keys — so the same physical clip can sit
            // in `routes` twice (native + imported) and every drive over it
            // roughly triples its distance: each copy contributes its full
            // clip distance and the boundary-gap bridge then "drives" from
            // copy A's end point back to copy A's start. One-shot rewrite:
            // drop stray event-folder rows, collapse native+import twins
            // (the native row wins — it carries the device's own extraction
            // and BLE telemetry columns), and rename the rest in place.
            let stored_key_fmt =
                meta_get(&mg, "route_key_format_version")?.unwrap_or_default();
            if stored_key_fmt != ROUTE_KEY_FORMAT_VERSION {
                // Stray SavedClips/SentryClips routes pre-date the
                // processor's event-folder skip; the grouper already hides
                // them from drives, so deleting only trims dead rows.
                // NOTE for future key-format bumps: event rows can now be
                // legitimate gap-fills (processor::scan_event_gap_fill) —
                // a canon-2 migration must not blanket-delete them.
                let events = mg.execute(
                    "DELETE FROM routes \
                     WHERE file LIKE 'SavedClips/%' OR file LIKE 'SentryClips/%'",
                    [],
                )?;
                // Where both spellings of a clip exist, delete the import
                // twin so the native row can take the canonical key.
                let twins = mg.execute(
                    "DELETE FROM routes \
                     WHERE file NOT LIKE 'RecentClips/%' \
                       AND 'RecentClips/' || file IN \
                           (SELECT file FROM routes WHERE file LIKE 'RecentClips/%')",
                    [],
                )?;
                // substr is 1-based: position 13 drops the 12-char
                // 'RecentClips/' prefix.
                let renamed = mg.execute(
                    "UPDATE routes SET file = substr(file, 13) \
                     WHERE file LIKE 'RecentClips/%'",
                    [],
                )?;
                // Mirror in processed_files so the processor's
                // already-processed check keeps matching these clips.
                // Event-folder entries are left alone — the processor
                // skips those directories at scan time anyway.
                mg.execute(
                    "INSERT OR IGNORE INTO processed_files(file, added_at) \
                     SELECT substr(file, 13), added_at FROM processed_files \
                     WHERE file LIKE 'RecentClips/%'",
                    [],
                )?;
                mg.execute(
                    "DELETE FROM processed_files WHERE file LIKE 'RecentClips/%'",
                    [],
                )?;
                if events > 0 || twins > 0 || renamed > 0 {
                    info!(
                        "[drives] Route keys canonicalized ({} -> {}): {} event row(s) dropped, {} duplicate twin(s) collapsed, {} row(s) renamed",
                        if stored_key_fmt.is_empty() { "<none>" } else { &stored_key_fmt },
                        ROUTE_KEY_FORMAT_VERSION,
                        events,
                        twins,
                        renamed
                    );
                }
                meta_set(&mg, "route_key_format_version", ROUTE_KEY_FORMAT_VERSION)?;
            }

            // Aggregate-formula version gate. The per-clip aggregates
            // (distance_m, speeds, FSD distances…) are computed once at
            // insert time and persisted; the backfill below only fills
            // rows where they're NULL. So when the *formula* changes —
            // e.g. the spherical→WGS-84-geodesic distance migration — the
            // existing rows keep their old numbers forever unless we
            // force a recompute. Bump AGGREGATE_FORMULA_VERSION whenever
            // compute_route_aggregates' math changes: this NULLs the
            // aggregate columns once, the backfill then recomputes them
            // with the new formula, and the cache (algo-versioned
            // separately) rebuilds from the fresh values.
            let stored_formula =
                meta_get(&mg, "aggregate_formula_version")?.unwrap_or_default();
            let formula_gate_fired = stored_formula != AGGREGATE_FORMULA_VERSION;
            if formula_gate_fired {
                // NULL only the v2 (ALTER-added, nullable) aggregate
                // columns. `max_speed_mps` is the backfill's gate
                // (`WHERE max_speed_mps IS NULL`), so nulling it queues
                // every row for recompute; the backfill's UPDATE then
                // overwrites distance_m too. distance_m itself is the
                // original NOT NULL DEFAULT 0 column and CANNOT be set
                // NULL — doing so fails the constraint and aborts the DB
                // open (it did, the first time). It's left as-is and
                // overwritten micro-seconds later by the backfill.
                let n = mg.execute(
                    "UPDATE routes SET max_speed_mps = NULL, avg_speed_mps = NULL, \
                     assisted_distance_m = NULL, fsd_distance_m = NULL, \
                     autosteer_distance_m = NULL, tacc_distance_m = NULL, \
                     fsd_pend_ms_end = NULL, park_ms_start = NULL, \
                     fsd_at_end = NULL, fsd_accel_pushes_early = NULL, \
                     ap_at_start = NULL",
                    [],
                )?;
                info!(
                    "[drives] Aggregate formula changed ({} -> {}); reset {} rows for recompute",
                    if stored_formula.is_empty() { "<none>" } else { &stored_formula },
                    AGGREGATE_FORMULA_VERSION,
                    n
                );
                meta_set(&mg, "aggregate_formula_version", AGGREGATE_FORMULA_VERSION)?;
            }

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

            // Checkpoint the WAL after any import/backfill writes so the
            // subsequent grouper query runs against the main DB file with
            // no large WAL to walk through.
            let _ = mg.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");

            // Rebuild the drive list cache only when the DB contents differ
            // from what the cache was built from. On a typical restart where
            // nothing changed, this skips the expensive grouper run entirely
            // (two COUNT(*) queries instead of a full 5k-row table scan).
            //
            // A fired formula gate forces the rebuild: the recompute
            // rewrites aggregate columns without touching updated_at or
            // row counts, so the validity marker still matches a cache
            // built from the OLD formula's numbers.
            if !formula_gate_fired && is_drive_cache_valid(&mg)? {
                info!("[drives] Drive list cache is current; skipping rebuild on startup");
            } else {
                rebuild_drive_list_cache(&mg).context("load: build drive cache")?;
            }
        }
        self.drive_cache_dirty.store(false, Ordering::Release);
        // load() re-imports/re-migrates route rows; drop the lazy route
        // overview cache so the next request recomputes.
        self.route_overview_gen.fetch_add(1, Ordering::Release);
        *self.route_overview_cache.lock().unwrap() = None;
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
        let mut stmt = conn.prepare_cached("SELECT file FROM processed_files")?;
        let files = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            // Rows are already normalized on write; reuse the owned String
            // instead of re-allocating one per row through normalize_path.
            .map(|f| if f.contains('\\') { f.replace('\\', "/") } else { f })
            .collect();
        Ok(files)
    }

    /// Mark a file processed without adding route data. Idempotent.
    pub fn mark_processed(&self, relative_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let now = now_unix();
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            params![normalize_path(relative_path), now],
        )?;
        drop(conn);
        // Counter maintained incrementally — this runs once per clip during
        // ingest, where the old full COUNT(*) refresh added two table scans
        // per call (see refresh_counts).
        self.processed_count
            .fetch_add(inserted as i64, Ordering::Relaxed);
        Ok(())
    }

    /// True if `file` has been processed.
    pub fn is_processed(&self, file: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: i64 = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM processed_files WHERE file = ?1)",
            params![normalize_path(file)],
            |row| row.get(0),
        )?;
        Ok(exists != 0)
    }

    /// Add a processed file AND its route data. If `points` is empty the
    /// route row is skipped (the clip is still marked processed). If a
    /// route for `file` already exists it is upserted in place.
    ///
    /// Computes aggregate columns inline so the summary endpoints can
    /// serve BLOB-free rows
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

        let pf_inserted = tx.execute(
            "INSERT OR IGNORE INTO processed_files(file, added_at) VALUES(?1, ?2)",
            params![norm, now],
        )?;

        let mut route_inserted: i64 = 0;
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
                source: None,
                external_signature: None,
                tessie_autopilot_percent: None,
                // BLE telemetry rollup is written separately by
                // `write_route_telemetry` after this insert lands; leave
                // None here.
                ..Default::default()
            };
            let agg = compute_route_aggregates(&route);
            // Indexed point lookup so the counter update below knows
            // insert vs upsert — replaces a full COUNT(*) per clip.
            let existed: i64 = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM routes WHERE file = ?1)",
                params![norm],
                |r| r.get(0),
            )?;
            route_inserted = 1 - existed;
            insert_or_update_route(&tx, &norm, &route, &agg, now)?;

            // v6 telemetry rollup: join the just-inserted clip's
            // 60s window against any telemetry_samples that landed
            // in it. Best-effort — telemetry failure must not block
            // the route insert (drive grouping is the critical path
            // here, telemetry is a value-add).
            match crate::aggregate_telemetry::compute_telemetry_for_route(&tx, &norm) {
                Ok(tele) => {
                    if let Err(e) = crate::aggregate_telemetry::write_route_telemetry(
                        &tx, &norm, &tele,
                    ) {
                        warn!("telemetry write failed for {}: {}", norm, e);
                    }
                }
                Err(e) => warn!("telemetry compute failed for {}: {}", norm, e),
            }
        }

        tx.commit()?;
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.route_overview_gen.fetch_add(1, Ordering::Release);
        self.processed_count
            .fetch_add(pf_inserted as i64, Ordering::Relaxed);
        self.route_count.fetch_add(route_inserted, Ordering::Relaxed);
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

    /// Fresh `Vec<Route>` decoded from the DB — full BLOB decode, heavy.
    /// Prefer [`Self::with_route_summaries`] for list-shaped reads and
    /// [`Self::with_routes_by_files`] for single-drive reads.
    pub fn get_routes(&self) -> Result<Vec<Route>> {
        let conn = self.conn.lock().unwrap();
        select_all_routes(&conn)
    }

    /// BLOB-free bulk read: materializes per-route
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

    /// Files of every **driving** event-folder route row — i.e. the
    /// Saved/Sentry clips the gap-fill spliced into a drive to cover a
    /// RecentClips recording hole. Normal drive routes are keyed under
    /// `RecentClips/`, so the LIKE clause returns exactly the gap-fill
    /// set, and the driving filter drops any parked row that entered the
    /// table before the `do_process` driving gate existed (older builds,
    /// drive-data.json imports). The snapshot builder reads a manifest
    /// derived from this (unioned with the processor's ungated
    /// interior-hole scan, which restores parked minutes a user save moved
    /// out of RecentClips) to cross-link those clips back into RecentClips
    /// for continuous playback. A parked ROW here would also put the clip
    /// in the drive list — exactly the parked presence 60c5602 removed —
    /// so the driving filter stays.
    ///
    /// The predicate is `grouper::row_has_driving` — the SAME one the
    /// grouper applies to stored rows when admitting gap-fills — so the
    /// manifest and the drive list never disagree on which event clips are
    /// part of a drive.
    pub fn gap_fill_files(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT file, raw_park_count, raw_frame_count, gear_runs_blob, max_speed_mps \
             FROM routes \
             WHERE file LIKE 'SavedClips/%' OR file LIKE 'SentryClips/%'",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as u32,
                    r.get::<_, i64>(2)? as u32,
                    r.get::<_, Option<Vec<u8>>>(3)?,
                    r.get::<_, Option<f64>>(4)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mut out = Vec::with_capacity(rows.len());
        for (file, raw_park, raw_frame, gear_blob, max_speed) in rows {
            let gear_runs = decode_gear_runs(gear_blob.as_deref())
                .with_context(|| format!("decode gear_runs {}", file))?
                .unwrap_or_default();
            if crate::grouper::row_has_driving(
                &gear_runs,
                raw_park,
                raw_frame,
                max_speed.unwrap_or(0.0),
            ) {
                out.push(file);
            }
        }
        Ok(out)
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
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.route_overview_gen.fetch_add(1, Ordering::Release);
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
            let mut stmt = conn.prepare_cached("SELECT drive_key, tag FROM drive_tags")?;
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
    /// the entry entirely. Queues the change for cloud sync.
    pub fn set_drive_tags(&self, drive_key: &str, tags: &[String]) -> Result<()> {
        self.set_drive_tags_inner(drive_key, tags, true)
    }

    /// Same as `set_drive_tags` but does NOT queue the change for cloud
    /// sync — used when APPLYING a change pulled from the cloud, so the
    /// write doesn't echo straight back up.
    pub fn set_drive_tags_from_sync(&self, drive_key: &str, tags: &[String]) -> Result<()> {
        self.set_drive_tags_inner(drive_key, tags, false)
    }

    fn set_drive_tags_inner(&self, drive_key: &str, tags: &[String], mark_dirty: bool) -> Result<()> {
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
        if mark_dirty {
            mark_mutable_dirty(&tx, "drive", drive_key)?;
        }
        tx.commit()?;
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.route_overview_gen.fetch_add(1, Ordering::Release);
        Ok(())
    }

    /// Tags for a drive, or an empty vec.
    pub fn get_drive_tags(&self, drive_key: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
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
        let mut stmt = conn.prepare_cached(
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
        let mut stmt = conn.prepare_cached("SELECT DISTINCT tag FROM drive_tags ORDER BY tag")?;
        let tags = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tags)
    }

    // ── Charge-session tags ────────────────────────────────────────────
    // Keyed on the session's start timestamp (unix seconds), the stable
    // id the /api/charging endpoints group by. No `drive_cache_dirty`
    // flag: charge tags don't feed the route cache.

    /// Replace the tags for charge session `session_ts`. Empty `tags`
    /// drops the entry entirely. Queues the change for cloud sync.
    pub fn set_charge_tags(&self, session_ts: i64, tags: &[String]) -> Result<()> {
        self.set_charge_tags_inner(session_ts, tags, true)
    }

    /// `set_charge_tags` without the cloud-sync dirty mark — for applying
    /// changes pulled FROM the cloud (no echo loop).
    pub fn set_charge_tags_from_sync(&self, session_ts: i64, tags: &[String]) -> Result<()> {
        self.set_charge_tags_inner(session_ts, tags, false)
    }

    fn set_charge_tags_inner(&self, session_ts: i64, tags: &[String], mark_dirty: bool) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM charge_tags WHERE session_ts = ?1",
            params![session_ts],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT OR IGNORE INTO charge_tags(session_ts, tag) VALUES(?1, ?2)",
            )?;
            for t in tags {
                if t.is_empty() {
                    continue;
                }
                stmt.execute(params![session_ts, t])?;
            }
        }
        if mark_dirty {
            mark_mutable_dirty(&tx, "charge", &session_ts.to_string())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Tags for one charge session, or an empty vec.
    pub fn get_charge_tags(&self, session_ts: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT tag FROM charge_tags WHERE session_ts = ?1 ORDER BY tag",
        )?;
        let out = stmt
            .query_map(params![session_ts], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(out)
    }

    /// Full session_ts → tags map for charge sessions.
    pub fn get_all_charge_tags(
        &self,
    ) -> Result<std::collections::HashMap<i64, Vec<String>>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT session_ts, tag FROM charge_tags ORDER BY session_ts, tag",
        )?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
        let mut out = std::collections::HashMap::<i64, Vec<String>>::new();
        for r in rows {
            let (k, t) = r?;
            out.entry(k).or_default().push(t);
        }
        Ok(out)
    }

    /// Every charge tag name in use, sorted and deduplicated.
    pub fn get_all_charge_tag_names(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT DISTINCT tag FROM charge_tags ORDER BY tag")?;
        let tags = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(tags)
    }

    // ── Charge-cost overrides ───────────────────────────────────────────
    // A manual per-charge cost (e.g. a Supercharger receipt total) that
    // overrides the tag/rate-derived cost. Keyed on the session start
    // timestamp like charge_tags; `None` clears the override. The web
    // offers this only for fast-charging sessions.

    /// Set, or clear when `cost` is `None`, the manual cost override for
    /// charge session `session_ts`. `cost` is `(amount, currency)`.
    /// Queues the change for cloud sync (tags + cost share one envelope).
    pub fn set_charge_cost(&self, session_ts: i64, cost: Option<(f64, String)>) -> Result<()> {
        self.set_charge_cost_inner(session_ts, cost, true)
    }

    /// `set_charge_cost` without the cloud-sync dirty mark — for applying
    /// changes pulled FROM the cloud (no echo loop).
    pub fn set_charge_cost_from_sync(&self, session_ts: i64, cost: Option<(f64, String)>) -> Result<()> {
        self.set_charge_cost_inner(session_ts, cost, false)
    }

    fn set_charge_cost_inner(
        &self,
        session_ts: i64,
        cost: Option<(f64, String)>,
        mark_dirty: bool,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        match cost {
            Some((amount, currency)) => conn.execute(
                "INSERT INTO charge_costs(session_ts, amount, currency) VALUES(?1, ?2, ?3) \
                 ON CONFLICT(session_ts) DO UPDATE SET amount = ?2, currency = ?3",
                params![session_ts, amount, currency],
            )?,
            None => conn.execute(
                "DELETE FROM charge_costs WHERE session_ts = ?1",
                params![session_ts],
            )?,
        };
        if mark_dirty {
            mark_mutable_dirty(&conn, "charge", &session_ts.to_string())?;
        }
        Ok(())
    }

    /// Manual cost override for one charge session, if set.
    pub fn get_charge_cost(&self, session_ts: i64) -> Result<Option<(f64, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn
            .prepare_cached("SELECT amount, currency FROM charge_costs WHERE session_ts = ?1")?;
        let mut rows = stmt.query_map(params![session_ts], |row| {
            Ok((
                row.get::<_, f64>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    /// Full session_ts → (amount, currency) map of manual cost overrides,
    /// for pricing the whole charging list in one query.
    pub fn get_all_charge_costs(
        &self,
    ) -> Result<std::collections::HashMap<i64, (f64, String)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt =
            conn.prepare_cached("SELECT session_ts, amount, currency FROM charge_costs")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (k, amount, currency) = r?;
            out.insert(k, (amount, currency));
        }
        Ok(out)
    }

    // ── Cloud mutable sync ─────────────────────────────────────────────

    /// Queue the per-Pi rate config for cloud push. Called by the api
    /// crate whenever a `charging_*` preference changes.
    pub fn mark_rate_config_dirty(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        mark_mutable_dirty(&conn, "rate", "")?;
        Ok(())
    }

    /// Every locally-changed mutable awaiting push: (kind, key, changed_at ms).
    pub fn dirty_mutables(&self) -> Result<Vec<(String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT kind, key, changed_at FROM mutable_dirty ORDER BY changed_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Drop a dirty row after a successful push — only if `changed_at`
    /// still matches (a newer local edit during the push stays queued).
    pub fn clear_mutable_dirty(&self, kind: &str, key: &str, changed_at: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM mutable_dirty WHERE kind = ?1 AND key = ?2 AND changed_at = ?3",
            params![kind, key, changed_at],
        )?;
        Ok(())
    }

    /// Record a successful charge upload (or, with `uploaded_at = -1`,
    /// a permanent skip). Caches the wrapped key for mutable sync.
    pub fn charge_upload_mark(
        &self,
        session_ts: i64,
        cloud_charge_id: &str,
        wrapped_charge_key_b64: &str,
        uploaded_at: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO charge_uploads(session_ts, cloud_charge_id, wrapped_charge_key, uploaded_at) \
             VALUES(?1, ?2, ?3, ?4) \
             ON CONFLICT(session_ts) DO UPDATE SET \
               cloud_charge_id = ?2, wrapped_charge_key = ?3, uploaded_at = ?4",
            params![session_ts, cloud_charge_id, wrapped_charge_key_b64, uploaded_at],
        )?;
        Ok(())
    }

    /// session_ts → (cloud_charge_id, wrapped_charge_key b64, uploaded_at)
    /// for every uploaded (or skipped) charge session.
    pub fn charge_uploads_map(
        &self,
    ) -> Result<std::collections::HashMap<i64, (String, String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT session_ts, cloud_charge_id, wrapped_charge_key, uploaded_at FROM charge_uploads",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (ts, id, key, at) = r?;
            out.insert(ts, (id, key, at));
        }
        Ok(out)
    }

    /// Reverse lookup: cloud chargeId → local session_ts.
    pub fn charge_session_ts_for_cloud_id(&self, cloud_charge_id: &str) -> Result<Option<i64>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT session_ts FROM charge_uploads WHERE cloud_charge_id = ?1",
                params![cloud_charge_id],
                |r| r.get::<_, i64>(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Cache the wrappedRouteKey (base64) produced at upload time so tag
    /// sync can re-derive the routeKey without a cloud round-trip.
    pub fn set_cloud_wrapped_route_key(&self, file: &str, wrapped_b64: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE routes SET cloud_wrapped_route_key = ?1 WHERE file = ?2",
            params![wrapped_b64, file],
        )?;
        Ok(())
    }

    /// (file, cloud_wrapped_route_key) for a cloud routeId, if known.
    pub fn route_sync_info_by_cloud_id(
        &self,
        cloud_route_id: &str,
    ) -> Result<Option<(String, Option<String>)>> {
        let conn = self.conn.lock().unwrap();
        let v = conn
            .query_row(
                "SELECT file, cloud_wrapped_route_key FROM routes WHERE cloud_route_id = ?1",
                params![cloud_route_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        Ok(v)
    }

    /// Per-file cloud sync info for tag push:
    /// file → (cloud_route_id, cloud_wrapped_route_key, uploaded). Files
    /// the DB doesn't know are absent from the map.
    pub fn route_sync_info_for_files(
        &self,
        files: &[&str],
    ) -> Result<std::collections::HashMap<String, (Option<String>, Option<String>, bool)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(
            "SELECT cloud_route_id, cloud_wrapped_route_key, cloud_uploaded_at \
             FROM routes WHERE file = ?1",
        )?;
        let mut out = std::collections::HashMap::new();
        for f in files {
            let row = stmt
                .query_row(params![f], |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<i64>>(2)?,
                    ))
                })
                .optional()?;
            if let Some((id, key, uploaded_at)) = row {
                out.insert(
                    f.to_string(),
                    (id, key, uploaded_at.is_some_and(|t| t > 0)),
                );
            }
        }
        Ok(out)
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

    /// Wipe routes, processed_files, and drive_tags — clean slate.
    pub fn clear_all_drives(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        for stmt in &[
            "DELETE FROM routes",
            "DELETE FROM processed_files",
            "DELETE FROM drive_tags",
        ] {
            conn.execute(stmt, [])?;
        }
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.route_overview_gen.fetch_add(1, Ordering::Release);
        self.refresh_counts()?;
        Ok(())
    }

    /// Bulk-delete the specified clip files from `routes` and
    /// `processed_files`, and prune any matching `drive_tags` rows.
    /// `drive_keys` lists the start_time strings used as `drive_tags`
    /// keys so the tag rows tied to the removed drives are cleaned up
    /// at the same time.
    ///
    /// All deletes run in a single transaction so a partial failure
    /// can't leave routes deleted but processed_files stale (which
    /// would silently prevent reprocessing). Returns the number of
    /// routes actually removed — useful for surfacing "deleted 42
    /// drives (61 clips)" in the UI.
    pub fn delete_routes_by_files(
        &self,
        files: &[String],
        drive_keys: &[String],
    ) -> Result<usize> {
        if files.is_empty() && drive_keys.is_empty() {
            return Ok(0);
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let mut deleted: usize = 0;
        let mut deleted_processed: usize = 0;
        if !files.is_empty() {
            let mut del_routes =
                tx.prepare("DELETE FROM routes WHERE file = ?1")?;
            let mut del_processed =
                tx.prepare("DELETE FROM processed_files WHERE file = ?1")?;
            for f in files {
                let n = normalize_path(f);
                deleted += del_routes.execute(params![n])?;
                deleted_processed += del_processed.execute(params![n])?;
            }
        }
        if !drive_keys.is_empty() {
            let mut del_tags =
                tx.prepare("DELETE FROM drive_tags WHERE drive_key = ?1")?;
            for k in drive_keys {
                del_tags.execute(params![k])?;
            }
        }
        tx.commit()?;
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        drop(conn);
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.route_overview_gen.fetch_add(1, Ordering::Release);
        self.route_count.fetch_sub(deleted as i64, Ordering::Relaxed);
        self.processed_count
            .fetch_sub(deleted_processed as i64, Ordering::Relaxed);
        Ok(deleted)
    }

    /// Regenerate the canonical `/backingfiles/drive-data.json` mirror for
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
        let existing_before = self.route_count.load(Ordering::Relaxed);
        let (stats, diag) = {
            let mut conn = self.conn.lock().unwrap();
            let s = crate::json_compat::import_json(&mut conn, path, on_progress)?;
            let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
            // Persist the diagnostics record while we still hold the writer
            // lock. Best-effort — a failure here is logged but not fatal,
            // since the import itself already committed.
            if let Err(e) = persist_import_history(&conn, &s.0, &s.1) {
                warn!("import_json_file_with_progress: failed to persist import history: {}", e);
            }
            s
        };
        self.drive_cache_dirty.store(true, Ordering::Release);
        self.route_overview_gen.fetch_add(1, Ordering::Release);
        self.refresh_counts()?;
        let after = self.route_count.load(Ordering::Relaxed);
        info!(
            "import_json_file: existing_before={} stats_routes={} after={} (delta={})",
            existing_before,
            stats.routes,
            after,
            after - existing_before
        );
        if diag.has_problems() {
            warn!(
                "import_json_file: diagnostics flagged problems — see import_json warnings above; \
                 query GET /api/drives/data/import-history for the persisted record"
            );
        }
        Ok(stats)
    }

    /// Read the ring-buffered import history (last [`IMPORT_HISTORY_MAX`]
    /// entries). Each entry contains the `ImportStats` and `ImportDiagnostics`
    /// captured at import time, plus a Unix epoch timestamp. Used by
    /// `GET /api/drives/data/import-history` so operators can see why drives
    /// went missing without scraping logs.
    pub fn import_history(&self) -> Result<Vec<ImportHistoryEntry>> {
        let conn = self.conn.lock().unwrap();
        read_import_history_inner(&conn)
    }

    /// Export current DB contents as `drive-data.json` at `path`.
    /// Atomic via tmp + rename.
    ///
    /// Opens a fresh read-only connection rather than locking the shared
    /// writer connection, so the 3+ minute mirror regeneration on a
    /// well-used Pi doesn't block `/api/drives`, `/api/drives/routes`,
    /// or any other DB-touching endpoint. WAL mode (set in
    /// `apply_pragmas`) lets this reader stream a consistent snapshot
    /// concurrently with writes from the main connection. Falls back
    /// to the in-memory `:memory:` path by reusing the shared
    /// connection, since you can't open a second handle to an
    /// in-memory DB.
    pub fn export_json_to_file(&self, path: &str) -> Result<()> {
        if let Some(dir) = Path::new(path).parent() {
            if !dir.as_os_str().is_empty() && dir != Path::new("/") {
                std::fs::create_dir_all(dir)?;
            }
        }
        let tmp = format!("{}.tmp", path);
        if self.path == ":memory:" {
            let conn = self.conn.lock().unwrap();
            write_export_json(&conn, &tmp)?;
        } else {
            let conn = open_readonly_connection(&self.path)
                .with_context(|| format!("export_json_to_file: open read-only {}", self.path))?;
            write_export_json(&conn, &tmp)?;
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
    ///
    /// Called at the tail of every processing run (see `Processor::do_process`)
    /// — the CIFS/NFS analogue of post-archive-process.sh's rsync/rclone
    /// blocks. Skips the multi-minute export when no new routes landed
    /// since the last successful sync (`ARCHIVE_SYNC_ROUTE_COUNT_KEY`
    /// baseline). The baseline only advances on a successful copy, so
    /// routes mapped while away from the archive (snapshotloop with
    /// DRIVE_MAP_WHILE_AWAY) are shipped on the next mounted cycle.
    pub fn sync_to_archive(&self) -> Result<()> {
        if !Path::new("/mnt/archive").exists() {
            return Ok(());
        }
        if let Ok(mounts) = std::fs::read_to_string("/proc/mounts") {
            if !mounts.contains("/mnt/archive") {
                return Ok(());
            }
        }

        self.sync_to_archive_at(
            DEFAULT_JSON_MIRROR_PATH,
            ARCHIVE_DATA_PATH,
            syncguard::DEFAULT_CACHE_PATH,
        )
    }

    /// Path-parameterized core of [`sync_to_archive`] (testable off-Pi).
    fn sync_to_archive_at(&self, mirror: &str, archive: &str, cache: &str) -> Result<()> {
        // Reflash recovery first: an empty store with an archive copy means
        // the Pi (or its backing drive) was wiped — pull the backup down for
        // the next-boot importer instead of pushing an empty export over it.
        if self.restore_from_archive_at(archive, mirror)? {
            return Ok(());
        }

        let routes_now = self.route_count();
        if routes_now == 0 {
            // Nothing worth pushing. An empty export must never reach the
            // archive: on a freshly-wiped /mutable the size-guard cache is
            // gone and the guard fails open, so this is the only thing
            // standing between a fresh install and a clobbered backup.
            return Ok(());
        }

        // No-op short-circuit, mirroring the shell script's rsync path:
        // skip the export + copy when neither the route count nor the
        // telemetry sample count has moved since the last successful
        // sync (samples cover charging-only days). Tag-only edits don't
        // bump either count and stay stale for one cycle — same accepted
        // trade-off as the shell's drives_count check.
        let (samples_now, last_synced, last_synced_samples) = {
            let conn = self.conn.lock().unwrap();
            let samples: i64 = conn
                .query_row("SELECT COUNT(*) FROM telemetry_samples", [], |r| r.get(0))
                .unwrap_or(0);
            let routes: Option<i64> =
                schema::meta_get(&conn, ARCHIVE_SYNC_ROUTE_COUNT_KEY)?.and_then(|s| s.parse().ok());
            let samples_baseline: Option<i64> = schema::meta_get(&conn, ARCHIVE_SYNC_SAMPLE_COUNT_KEY)?
                .and_then(|s| s.parse().ok());
            (samples, routes, samples_baseline)
        };
        if last_synced == Some(routes_now)
            && last_synced_samples == Some(samples_now)
            && Path::new(archive).exists()
        {
            info!(
                "[drives] No new routes or telemetry since last archive sync (route_count={} sample_count={}); skipping drive-data.json export",
                routes_now, samples_now
            );
            return Ok(());
        }

        self.export_json_to_file(mirror)?;
        sync_to_path(mirror, archive, cache)?;

        let conn = self.conn.lock().unwrap();
        schema::meta_set(&conn, ARCHIVE_SYNC_ROUTE_COUNT_KEY, &routes_now.to_string())?;
        schema::meta_set(&conn, ARCHIVE_SYNC_SAMPLE_COUNT_KEY, &samples_now.to_string())?;
        Ok(())
    }

    /// Copy `/mnt/archive/drive-data.json` to `/backingfiles/drive-data.json`
    /// so the next `Load()` picks it up via the one-shot importer.
    /// Useful after reflashing a Pi that still has an archive backup.
    /// Returns `true` if a restore happened.
    pub fn restore_from_archive(&self) -> Result<bool> {
        self.restore_from_archive_at(ARCHIVE_DATA_PATH, DEFAULT_JSON_MIRROR_PATH)
    }

    /// Path-parameterized core of [`restore_from_archive`] (testable off-Pi).
    fn restore_from_archive_at(&self, archive: &str, mirror: &str) -> Result<bool> {
        if !Path::new(archive).exists() {
            return Ok(false);
        }
        // Don't restore if we already have local data — the importer
        // would skip it anyway, and we'd rather not churn disk.
        if Path::new(mirror).exists() {
            return Ok(false);
        }
        // Only restore into an empty store. A populated store with a
        // missing mirror just hasn't exported yet; copying a stale archive
        // snapshot over would feed old data to the next-boot importer.
        if self.route_count() > 0 {
            return Ok(false);
        }
        if let Some(dir) = Path::new(mirror).parent() {
            if !dir.as_os_str().is_empty() && dir != Path::new("/") {
                std::fs::create_dir_all(dir)?;
            }
        }
        // Stream the copy (tmp + rename for atomicity) — the archive JSON
        // can be hundreds of MB; never buffer it in RAM.
        let tmp = format!("{}.tmp", mirror);
        let copied = std::fs::copy(archive, &tmp)?;
        if let Err(e) = std::fs::rename(&tmp, mirror) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        info!(
            "[drives] Restored drive-data.json from archive ({} bytes); next Load() will import it",
            copied
        );
        Ok(true)
    }

    /// Rebuild the drive caches while holding the connection lock only for
    /// the short read-snapshot and write-back phases. The heavy middle —
    /// grouping 7k+ summaries, stats, FSD analytics, JSON serialization
    /// (~400ms on a Pi 5, seconds on a Zero 2 W) — runs with NO lock held,
    /// so telemetry inserts and unrelated API queries don't stall behind it
    /// (previously every DB user queued for the entire rebuild).
    ///
    /// `rebuild_lock` serializes concurrent rebuilds: the second caller
    /// blocks until the first finishes, then sees a clean flag and returns
    /// without recomputing. Lock order is rebuild_lock → conn, never the
    /// reverse, so this cannot deadlock with any with_locked_conn user.
    /// `force` skips the it-was-rebuilt-while-we-waited early exit — used
    /// by getters that found a missing/placeholder cache entry despite a
    /// clean dirty flag (e.g. the legacy "{}" fsd_analytics_cache), where
    /// only an actual rebuild repairs the entry.
    fn rebuild_caches_off_lock(&self, force: bool) -> Result<()> {
        let _rebuild = self.rebuild_lock.lock().unwrap();

        // A concurrent caller may have rebuilt while we waited for the
        // rebuild lock — if the flag is clean and a cache exists, done.
        if !force && !self.drive_cache_dirty.load(Ordering::Acquire) {
            let conn = self.conn.lock().unwrap();
            if let Some(json) = schema::meta_get(&conn, "drive_list_cache")? {
                if !json.is_empty() {
                    return Ok(());
                }
            }
        }

        // Snapshot point: clear the dirty flag BEFORE opening the read
        // snapshot. A mutation landing in the gap is still visible to the
        // snapshot (it begins after) AND re-dirties the flag — worst case
        // one redundant rebuild, never a missed one. Clearing later would
        // let a mid-rebuild mutation be swallowed.
        self.drive_cache_dirty.store(false, Ordering::Release);

        // Phase 1: snapshot inputs. On a file-backed store this uses a
        // dedicated READ-ONLY connection — WAL lets it scan the summary
        // rows concurrently with the shared connection's writers, so the
        // heaviest read (7k+ rows, ~70ms on a Pi 5, far more on a Zero
        // 2 W) doesn't block telemetry inserts or API queries at all.
        // The explicit transaction pins all three queries (summaries,
        // tags, markers) to one consistent snapshot.
        let inputs = if self.path == ":memory:" {
            // A second connection to ":memory:" would be a different,
            // empty database — fall back to the shared one (tests only).
            let conn = self.conn.lock().unwrap();
            select_cache_inputs(&conn)?
        } else {
            let rconn = open_readonly_connection(&self.path)?;
            let tx = rconn.unchecked_transaction()?;
            let inputs = select_cache_inputs(&tx)?;
            drop(tx);
            inputs
        };

        // Phase 2 (no lock): the expensive part.
        let artifacts = compute_drive_caches(inputs)?;

        // Phase 3 (locked, fast): persist.
        let conn = self.conn.lock().unwrap();
        write_drive_caches(&conn, &artifacts)
    }

    /// Return the pre-computed drives list as a JSON string. On a cache hit
    /// (the common case after startup) this is a single-row meta-table
    /// lookup — no grouper work, no BLOB decoding, no sorter allocation.
    ///
    /// On a cache miss (first request after startup or after routes/tags
    /// change), builds the cache from route summaries + tags and stores it
    /// in the `meta` table for subsequent requests — holding the connection
    /// lock only for the snapshot and write phases, not the compute.
    pub fn get_cached_drives_json(&self) -> Result<String> {
        if !self.drive_cache_dirty.load(Ordering::Acquire) {
            let conn = self.conn.lock().unwrap();
            if let Some(json) = schema::meta_get(&conn, "drive_list_cache")? {
                if !json.is_empty() {
                    return Ok(json);
                }
            }
        }

        // force when the flag was clean (the entry itself was missing or
        // empty) — the stampede early-exit would otherwise skip the repair.
        let force = !self.drive_cache_dirty.load(Ordering::Acquire);
        self.rebuild_caches_off_lock(force)?;
        let conn = self.conn.lock().unwrap();
        Ok(schema::meta_get(&conn, "drive_list_cache")?.unwrap_or_else(|| "[]".to_string()))
    }

    /// Serve `/api/drives/routes` map-overview JSON from an in-memory cache;
    /// recompute (full grouper) only after a route mutation.
    pub fn get_cached_route_overviews_json(&self, max_points: usize) -> Result<String> {
        // Read generation before routes: a mutation in between just forces one
        // extra recompute next time, never a stale serve.
        let generation = self.route_overview_gen.load(Ordering::Acquire);
        {
            let cache = self.route_overview_cache.lock().unwrap();
            if let Some(c) = cache.as_ref() {
                if c.generation == generation && c.max_points == max_points {
                    return Ok(c.json.clone());
                }
            }
        }
        let routes = {
            let conn = self.conn.lock().unwrap();
            select_all_routes(&conn)?
        };
        let overviews = crate::grouper::route_overviews(routes, max_points);
        let json = serde_json::to_string(&overviews).unwrap_or_else(|_| "[]".to_string());
        let mut cache = self.route_overview_cache.lock().unwrap();
        *cache = Some(RouteOverviewCache { generation, max_points, json: json.clone() });
        Ok(json)
    }

    /// Return the pre-computed drive stats as a JSON string. `processed_count`
    /// is stored as 0 in the cache; callers must inject the live value.
    pub fn get_cached_drive_stats_json(&self) -> Result<String> {
        if !self.drive_cache_dirty.load(Ordering::Acquire) {
            let conn = self.conn.lock().unwrap();
            if let Some(json) = schema::meta_get(&conn, "drive_stats_cache")? {
                if !json.is_empty() {
                    return Ok(json);
                }
            }
        }
        let force = !self.drive_cache_dirty.load(Ordering::Acquire);
        self.rebuild_caches_off_lock(force)?;
        let conn = self.conn.lock().unwrap();
        Ok(schema::meta_get(&conn, "drive_stats_cache")?.unwrap_or_else(|| "{}".to_string()))
    }

    /// Return the pre-computed FSD analytics as a JSON string.
    pub fn get_cached_fsd_analytics_json(&self) -> Result<String> {
        if !self.drive_cache_dirty.load(Ordering::Acquire) {
            let conn = self.conn.lock().unwrap();
            if let Some(json) = schema::meta_get(&conn, "fsd_analytics_cache")? {
                // Treat "{}" as a cache miss: older builds could persist an
                // empty-object placeholder which then masks real data forever.
                if !json.is_empty() && json.trim() != "{}" {
                    return Ok(json);
                }
            }
        }
        let force = !self.drive_cache_dirty.load(Ordering::Acquire);
        self.rebuild_caches_off_lock(force)?;
        let conn = self.conn.lock().unwrap();
        Ok(schema::meta_get(&conn, "fsd_analytics_cache")?.unwrap_or_else(|| "{}".to_string()))
    }

    /// Refresh the cached row counts with full COUNT(*) queries. Called
    /// after bulk mutations (load / replace_data / clear_* / JSON import);
    /// the per-clip paths (add_route / mark_processed /
    /// delete_routes_by_files) maintain the counters incrementally so
    /// ingest doesn't pay two table scans per clip.
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

/// Open a second SQLite handle on the same DB file, in read-only mode,
/// for long-running reads (the JSON export mirror) that would otherwise
/// hold the shared writer connection's mutex for minutes. WAL mode on
/// the writer lets this handle see a consistent snapshot.
fn open_readonly_connection(path: &str) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(path, flags)?;
    // mmap_size matches apply_pragmas — this connection serves the
    // BLOB-heavy full-table scans (JSON export, cache-rebuild snapshot),
    // which benefit most from skipping the pager-buffer copy.
    conn.execute_batch(
        "PRAGMA query_only = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA mmap_size = 67108864;",
    )?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // mmap_size = 64 MB: SQLite mmaps the DB file up to this size, which
    // eliminates the pager-buffer copy on BLOB-heavy reads (e.g.
    // select_all_route_summaries scanning thousands of routes). 64 MB
    // fits comfortably in 32-bit ARMv7's ~3 GB user-space VA, so it's
    // safe across every SBC the project supports.
    //
    // cache_size = -8000: 8 MB page cache (negative value = KB). Default
    // is 2 MB which is too small to keep the rebuild_drive_list_cache
    // working set hot on a populated DB. Bumped to 8 MB; still trivial
    // relative to Pi RAM budgets.
    //
    // temp_store = MEMORY: keep ORDER BY / GROUP BY temp tables in RAM
    // instead of /tmp. On read-only-root Pi setups /tmp is tmpfs anyway,
    // so this is equivalent in effect, but it's explicit and safe.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA mmap_size = 67108864;
         PRAGMA cache_size = -8000;
         PRAGMA temp_store = MEMORY;",
    )?;
    Ok(())
}

fn write_export_json(conn: &Connection, tmp_path: &str) -> Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(tmp_path)?;
    crate::json_compat::export_json(conn, &mut f).context("export_json")?;
    f.flush()?;
    f.sync_all()?;
    Ok(())
}

/// Maximum number of import-history records kept in the `meta` table.
const IMPORT_HISTORY_MAX: usize = 20;

/// Wire-format record for one entry in the persisted import history.
/// Stored as a JSON array under `meta` key `import_history` (newest last).
#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImportHistoryEntry {
    /// Unix epoch seconds when the import completed.
    pub timestamp: i64,
    pub stats: crate::json_compat::ImportStats,
    pub diagnostics: crate::json_compat::ImportDiagnostics,
}

/// Append a single import's stats + diagnostics to the ring-buffered
/// `import_history` JSON array in the `meta` table. Keeps the most recent
/// `IMPORT_HISTORY_MAX` entries; older ones are dropped from the front.
fn persist_import_history(
    conn: &Connection,
    stats: &crate::json_compat::ImportStats,
    diag: &crate::json_compat::ImportDiagnostics,
) -> Result<()> {
    let entry = ImportHistoryEntry {
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        stats: *stats,
        diagnostics: diag.clone(),
    };
    let mut history: Vec<ImportHistoryEntry> = match schema::meta_get(conn, "import_history")? {
        Some(s) if !s.is_empty() => serde_json::from_str(&s).unwrap_or_default(),
        _ => Vec::new(),
    };
    history.push(entry);
    if history.len() > IMPORT_HISTORY_MAX {
        let drop = history.len() - IMPORT_HISTORY_MAX;
        history.drain(0..drop);
    }
    let json = serde_json::to_string(&history)
        .map_err(|e| anyhow::anyhow!("serialize import_history: {}", e))?;
    schema::meta_set(conn, "import_history", &json)?;
    Ok(())
}

/// Read the persisted import history from the `meta` table. Returns an
/// empty Vec if no imports have been recorded.
fn read_import_history_inner(conn: &Connection) -> Result<Vec<ImportHistoryEntry>> {
    match schema::meta_get(conn, "import_history")? {
        Some(s) if !s.is_empty() => serde_json::from_str(&s)
            .map_err(|e| anyhow::anyhow!("parse import_history: {}", e)),
        _ => Ok(Vec::new()),
    }
}

/// Build the grouped drive list and store it as JSON in the `meta` table,
/// along with the route count and tag row count used to validate the cache
/// on the next startup.
/// Inputs one cache rebuild needs, snapshotted under a single lock hold so
/// the validity markers (route_count / tags_count / max_updated_at)
/// describe exactly the data the cache is built from. If the DB changes
/// after the snapshot, the markers stop matching live counts and
/// `is_drive_cache_valid` treats the cache as stale — self-correcting.
struct DriveCacheInputs {
    summaries: Vec<RouteSummary>,
    tags: std::collections::HashMap<String, Vec<String>>,
    tags_count: i64,
    max_updated_at: i64,
}

/// Everything one rebuild produces. Computed WITHOUT the connection lock
/// (the grouper + serialization are pure CPU — ~400ms for 7k routes on a
/// Pi 5, seconds on a Zero 2 W), then persisted under a short lock.
struct DriveCacheArtifacts {
    drives_json: String,
    stats_json: String,
    fsd_json: String,
    route_count: i64,
    tags_count: i64,
    max_updated_at: i64,
    drives_total: usize,
    visible_total: usize,
}

/// Rebuild phase 1 — locked, read-only, fast: snapshot the rebuild inputs.
fn select_cache_inputs(conn: &Connection) -> Result<DriveCacheInputs> {
    let summaries = select_all_route_summaries(conn)?;

    let mut tags = std::collections::HashMap::<String, Vec<String>>::new();
    let mut tags_count: i64 = 0;
    {
        let mut stmt = conn.prepare_cached("SELECT drive_key, tag FROM drive_tags")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for r in rows {
            let (k, t) = r?;
            tags.entry(k).or_default().push(t);
            tags_count += 1;
        }
    }

    // MAX(updated_at) lets the validity check detect in-place route updates
    // (same row count, different aggregates — e.g. archiveloop reprocess,
    // or a grouper change shipped via OTA without bumping
    // DRIVE_LIST_CACHE_ALGO_VERSION). Without this marker, the cache stays
    // "valid" while the live grouper would produce a different drive list.
    // Read in the same lock hold as the data so the marker can never be
    // newer than the snapshot it describes.
    let max_updated_at: i64 = conn
        .query_row("SELECT COALESCE(MAX(updated_at), 0) FROM routes", [], |r| r.get(0))
        .unwrap_or(0);

    Ok(DriveCacheInputs { summaries, tags, tags_count, max_updated_at })
}

/// Rebuild phase 2 — NO lock held: grouping, stats, analytics, serialization.
fn compute_drive_caches(inputs: DriveCacheInputs) -> Result<DriveCacheArtifacts> {
    let DriveCacheInputs { summaries, tags, tags_count, max_updated_at } = inputs;
    let route_count = summaries.len() as i64;

    // Use the BLOB-free summary grouper so this cache and the
    // `single_drive` endpoint (which also resolves drive IDs through
    // the summary grouper) agree on drive count, boundaries, and IDs.
    // The previous BLOB-grouper cache could split a clip mid-park-gap
    // while the summary grouper kept the whole clip in one drive,
    // producing different drive lists for /api/drives vs
    // /api/drives/{id} and causing clicked drives to load wrong points.
    //
    // Heap win: ~5 MB instead of ~300 MB on a 5500-route DB (no BLOB
    // decode here). Numerical drift on noisy GPS is fractions of a
    // percent, invisible after the UI's 0.1-mi / whole-percent rounding.
    //
    // Group with original (un-hidden) IDs first — these are what
    // `find_drive_files` looks up against, so the cached list must
    // hold the same IDs even after the Tessie-overlap filter strips
    // duplicates.
    let drives = crate::grouper::group_summaries_fast(&summaries, &tags);
    let visible = crate::grouper::hide_tessie_overlapping_sei(drives.clone());
    info!(
        "drive cache: route_count={} drives={} visible={} tags={}",
        route_count,
        drives.len(),
        visible.len(),
        tags_count
    );
    let drives_json = serde_json::to_string(&visible)
        .map_err(|e| anyhow::anyhow!("drive cache serialize: {}", e))?;

    // Cache drive stats from grouped drives so `/api/drives` and
    // `/api/drives/stats` are consistent on drive count and mileage.
    // Totals include all drives; FSD-specific analytics are SEI-only.
    let r = |v: f64| -> f64 { (v * 100.0).round() / 100.0 };
    let drives_count = drives.len() as i64;
    let total_distance_km: f64 = drives.iter().map(|d| d.distance_km).sum();
    let total_distance_mi: f64 = drives.iter().map(|d| d.distance_mi).sum();
    let total_duration_ms: i64 = drives.iter().map(|d| d.duration_ms).sum();

    // SEI = native dashcam only. Anything with a non-SEI source string
    // (tessie, teslascope, future importers) is imported data with fuzzy
    // or absent per-point autopilot telemetry — counted in totals, never
    // in FSD analytics. Matches Sentry-Drive's isImportedSource rule.
    let sei_drives: Vec<_> = drives
        .iter()
        .filter(|d| !matches!(d.source.as_deref(), Some(s) if s != "sei"))
        .collect();
    let sei_total_km: f64 = sei_drives.iter().map(|d| d.distance_km).sum();
    let fsd_distance_km: f64 = sei_drives.iter().map(|d| d.fsd_distance_km).sum();
    let fsd_distance_mi: f64 = sei_drives.iter().map(|d| d.fsd_distance_mi).sum();
    let autosteer_distance_km: f64 = sei_drives.iter().map(|d| d.autosteer_distance_km).sum();
    let autosteer_distance_mi: f64 = sei_drives.iter().map(|d| d.autosteer_distance_mi).sum();
    let tacc_distance_km: f64 = sei_drives.iter().map(|d| d.tacc_distance_km).sum();
    let tacc_distance_mi: f64 = sei_drives.iter().map(|d| d.tacc_distance_mi).sum();
    let fsd_engaged_ms: i64 = sei_drives.iter().map(|d| d.fsd_engaged_ms).sum();
    let autosteer_engaged_ms: i64 = sei_drives.iter().map(|d| d.autosteer_engaged_ms).sum();
    let tacc_engaged_ms: i64 = sei_drives.iter().map(|d| d.tacc_engaged_ms).sum();
    let fsd_disengagements: i32 = sei_drives.iter().map(|d| d.fsd_disengagements).sum();
    let fsd_accel_pushes: i32 = sei_drives.iter().map(|d| d.fsd_accel_pushes).sum();
    let fsd_percent = if sei_total_km > 0.0 {
        (fsd_distance_km / sei_total_km * 100.0 * 10.0).round() / 10.0
    } else {
        0.0
    };
    let assisted_percent = if sei_total_km > 0.0 {
        ((fsd_distance_km + autosteer_distance_km + tacc_distance_km) / sei_total_km * 100.0 * 10.0)
            .round()
            / 10.0
    } else {
        0.0
    };
    let stats_json = serde_json::to_string(&serde_json::json!({
        "drives_count":          drives_count,
        "routes_count":          route_count,
        "processed_count":       0,
        "total_distance_km":     r(total_distance_km),
        "total_distance_mi":     r(total_distance_mi),
        "total_duration_ms":     total_duration_ms,
        "fsd_engaged_ms":        fsd_engaged_ms,
        "fsd_distance_km":       r(fsd_distance_km),
        "fsd_distance_mi":       r(fsd_distance_mi),
        "fsd_percent":           fsd_percent,
        "fsd_disengagements":    fsd_disengagements,
        "fsd_accel_pushes":      fsd_accel_pushes,
        "autosteer_engaged_ms":  autosteer_engaged_ms,
        "autosteer_distance_km": r(autosteer_distance_km),
        "autosteer_distance_mi": r(autosteer_distance_mi),
        "tacc_engaged_ms":       tacc_engaged_ms,
        "tacc_distance_km":      r(tacc_distance_km),
        "tacc_distance_mi":      r(tacc_distance_mi),
        "assisted_percent":      assisted_percent,
    })).map_err(|e| anyhow::anyhow!("stats cache serialize: {}", e))?;

    // FSD analytics — reuses the already-grouped drives list.
    let fsd = crate::grouper::fsd_analytics_from_drives(&drives);
    let fsd_json = serde_json::to_string(&fsd)
        .map_err(|e| anyhow::anyhow!("fsd analytics cache serialize: {}", e))?;

    Ok(DriveCacheArtifacts {
        drives_json,
        stats_json,
        fsd_json,
        route_count,
        tags_count,
        max_updated_at,
        drives_total: drives.len(),
        visible_total: visible.len(),
    })
}

/// Rebuild phase 3 — locked, fast: persist the artifacts + the validity
/// markers describing the snapshot they were built from.
fn write_drive_caches(conn: &Connection, a: &DriveCacheArtifacts) -> Result<()> {
    schema::meta_set(conn, "drive_list_cache", &a.drives_json)?;
    schema::meta_set(conn, "drive_list_cache_route_count", &a.route_count.to_string())?;
    schema::meta_set(conn, "drive_list_cache_tags_count", &a.tags_count.to_string())?;
    schema::meta_set(conn, "drive_list_cache_algo", DRIVE_LIST_CACHE_ALGO_VERSION)?;
    schema::meta_set(conn, "drive_list_cache_max_updated_at", &a.max_updated_at.to_string())?;
    schema::meta_set(conn, "drive_stats_cache", &a.stats_json)?;
    schema::meta_set(conn, "fsd_analytics_cache", &a.fsd_json)?;
    info!(
        "[drives] Drive list cache rebuilt ({} drives, {} visible after Tessie/SEI hide, from {} routes)",
        a.drives_total,
        a.visible_total,
        a.route_count
    );
    Ok(())
}

/// Single-lock-context rebuild for callers that already hold the
/// connection (startup's `load_locked`, where nothing else is running
/// yet). Request paths use [`DriveStore::rebuild_caches_off_lock`]
/// instead, which keeps the heavy compute phase off the connection lock.
fn rebuild_drive_list_cache(conn: &Connection) -> Result<()> {
    let inputs = select_cache_inputs(conn)?;
    let artifacts = compute_drive_caches(inputs)?;
    write_drive_caches(conn, &artifacts)
}

/// True when the persisted drive list cache matches the current DB contents.
/// Compares route count and drive_tags row count stored at cache-build time
/// against live COUNT(*) values. Two cheap queries per startup skip the
/// expensive grouper run on restarts where nothing changed.
fn is_drive_cache_valid(conn: &Connection) -> Result<bool> {
    let cache = schema::meta_get(conn, "drive_list_cache")?;
    if cache.map_or(true, |s| s.is_empty()) {
        return Ok(false);
    }

    let stored_rc = schema::meta_get(conn, "drive_list_cache_route_count")?
        .and_then(|s| s.parse::<i64>().ok());
    let current_rc: i64 =
        conn.query_row("SELECT COUNT(*) FROM routes", [], |r| r.get(0))?;
    if stored_rc != Some(current_rc) {
        return Ok(false);
    }

    let stored_tc = schema::meta_get(conn, "drive_list_cache_tags_count")?
        .and_then(|s| s.parse::<i64>().ok());
    let current_tc: i64 =
        conn.query_row("SELECT COUNT(*) FROM drive_tags", [], |r| r.get(0))?;
    if stored_tc != Some(current_tc) {
        return Ok(false);
    }

    let algo = schema::meta_get(conn, "drive_list_cache_algo")?;
    if algo.as_deref() != Some(DRIVE_LIST_CACHE_ALGO_VERSION) {
        return Ok(false);
    }

    // Detect in-place route updates (same row count, changed aggregates).
    // Without this, an archiveloop reprocess pass — or an OTA where the
    // grouper changes but DRIVE_LIST_CACHE_ALGO_VERSION isn't bumped —
    // leaves the cache serving stale drive boundaries while the live
    // grouper would produce a different list. Field-reproduced 2026-05-19
    // on v2.7.5: cache held 213 drives from a prior state; fresh grouper
    // computed 144; `/api/drives/{id}` 404'd for IDs 144-212 because they
    // existed in the cache but not in the live grouping.
    //
    // Treat a missing stored value as invalid so caches written by
    // pre-fix builds get rebuilt on first read after the upgrade.
    let stored_max_ua = schema::meta_get(conn, "drive_list_cache_max_updated_at")?
        .and_then(|s| s.parse::<i64>().ok());
    let current_max_ua: i64 = conn
        .query_row("SELECT COALESCE(MAX(updated_at), 0) FROM routes", [], |r| r.get(0))
        .unwrap_or(-1);
    if stored_max_ua != Some(current_max_ua) {
        return Ok(false);
    }

    Ok(true)
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

    // `params![]` builds a stack-allocated `[&dyn ToSql; N]`, replacing
    // the prior `Vec<Box<dyn ToSql>>` + `Vec<&dyn ToSql>` pattern which
    // heap-allocated 35 small boxes plus two Vecs per insert. Called once
    // per ingested clip (50+/min during Tesla recording).
    // The v6+ telemetry columns ride along from the Route so restore
    // paths (replace_data, JSON import) preserve them — they used to be
    // silently dropped, so an export→import round-trip lost every BLE
    // telemetry badge. On the live ingest path the Route carries None
    // here and write_route_telemetry recomputes them right after, in
    // the same transaction.
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
            ap_at_start)
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
            ?50, ?51, ?52, ?53, ?54)
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
            battery_pct_start   = excluded.battery_pct_start,
            battery_pct_end     = excluded.battery_pct_end,
            interior_temp_min   = excluded.interior_temp_min,
            interior_temp_max   = excluded.interior_temp_max,
            exterior_temp_avg   = excluded.exterior_temp_avg,
            hvac_runtime_s      = excluded.hvac_runtime_s,
            tire_fl_psi         = excluded.tire_fl_psi,
            tire_fr_psi         = excluded.tire_fr_psi,
            tire_rl_psi         = excluded.tire_rl_psi,
            tire_rr_psi         = excluded.tire_rr_psi,
            odometer_mi_start   = excluded.odometer_mi_start,
            odometer_mi_end     = excluded.odometer_mi_end,
            location_name_start = excluded.location_name_start,
            location_name_end   = excluded.location_name_end,
            fsd_pend_ms_end     = excluded.fsd_pend_ms_end,
            park_ms_start       = excluded.park_ms_start,
            fsd_at_end          = excluded.fsd_at_end,
            fsd_accel_pushes_early = excluded.fsd_accel_pushes_early,
            ap_at_start         = excluded.ap_at_start",
        params![
            norm_file,
            &r.date,
            point_count,
            r.raw_park_count as i64,
            r.raw_frame_count as i64,
            a.distance_m,
            first_lat,
            first_lon,
            pb,
            gb,
            ab,
            sb,
            acb,
            rb,
            now,
            a.max_speed_mps,
            a.avg_speed_mps,
            a.speed_sample_count,
            a.valid_point_count,
            a.fsd_engaged_ms,
            a.autosteer_engaged_ms,
            a.tacc_engaged_ms,
            a.fsd_distance_m,
            a.autosteer_distance_m,
            a.tacc_distance_m,
            a.assisted_distance_m,
            a.fsd_disengagements,
            a.fsd_accel_pushes,
            a.start_lat,
            a.start_lng,
            a.end_lat,
            a.end_lng,
            &r.source,
            &r.external_signature,
            r.tessie_autopilot_percent,
            r.battery_pct_start,
            r.battery_pct_end,
            r.interior_temp_min,
            r.interior_temp_max,
            r.exterior_temp_avg,
            r.hvac_runtime_s,
            r.tire_fl_psi,
            r.tire_fr_psi,
            r.tire_rl_psi,
            r.tire_rr_psi,
            r.odometer_mi_start,
            r.odometer_mi_end,
            &r.location_name_start,
            &r.location_name_end,
            a.fsd_pend_ms_end,
            a.park_ms_start,
            a.fsd_at_end as i64,
            a.fsd_accel_pushes_early,
            a.ap_at_start,
        ],
    )?;
    Ok(())
}

/// Select all routes into `Vec<Route>` — fully decoded BLOB columns.
fn select_all_routes(conn: &Connection) -> Result<Vec<Route>> {
    let mut stmt = conn.prepare_cached(
        "SELECT file, date_dir, raw_park_count, raw_frame_count,
                points_blob, gear_states_blob, ap_states_blob,
                speeds_blob, accel_blob, gear_runs_blob,
                source, external_signature, tessie_autopilot_percent,
                battery_pct_start, battery_pct_end,
                interior_temp_min, interior_temp_max, exterior_temp_avg,
                hvac_runtime_s,
                tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
                odometer_mi_start, odometer_mi_end,
                location_name_start, location_name_end
         FROM routes
         ORDER BY file",
    )?;
    let rows = stmt.query_map([], route_row_mapper)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(build_route_from_row(r?)?);
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
                speeds_blob, accel_blob, gear_runs_blob,
                source, external_signature, tessie_autopilot_percent,
                battery_pct_start, battery_pct_end,
                interior_temp_min, interior_temp_max, exterior_temp_avg,
                hvac_runtime_s,
                tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
                odometer_mi_start, odometer_mi_end,
                location_name_start, location_name_end
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
    let rows = stmt.query_map(params.as_slice(), route_row_mapper)?;

    let mut out = Vec::with_capacity(files.len());
    for r in rows {
        out.push(build_route_from_row(r?)?);
    }
    Ok(out)
}

/// Tuple shape that [`route_row_mapper`] returns. Carries raw column
/// values straight from the row; [`build_route_from_row`] decodes the
/// blob columns and assembles the `Route`. Kept as a free-standing
/// tuple to avoid a one-off struct used only by these two selects.
type RouteRow = (
    String, String, u32, u32,
    Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>,
    Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>,
    Option<String>, Option<String>, Option<f64>,
    // BLE telemetry rollup — populated by
    // `aggregate_telemetry::write_route_telemetry`. NULL on rows whose
    // clip-window had no telemetry samples, or pre-v6 rows.
    Option<f64>, Option<f64>,
    Option<f64>, Option<f64>, Option<f64>,
    Option<i64>,
    Option<f64>, Option<f64>, Option<f64>, Option<f64>,
    Option<f64>, Option<f64>,
    Option<String>, Option<String>,
);

/// Shared row mapper for the two route SELECTs above. The column order
/// in both SQL strings must match this exactly.
fn route_row_mapper(row: &rusqlite::Row<'_>) -> rusqlite::Result<RouteRow> {
    Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, i64>(2)? as u32,
        row.get::<_, i64>(3)? as u32,
        row.get::<_, Option<Vec<u8>>>(4)?,
        row.get::<_, Option<Vec<u8>>>(5)?,
        row.get::<_, Option<Vec<u8>>>(6)?,
        row.get::<_, Option<Vec<u8>>>(7)?,
        row.get::<_, Option<Vec<u8>>>(8)?,
        row.get::<_, Option<Vec<u8>>>(9)?,
        row.get::<_, Option<String>>(10)?,
        row.get::<_, Option<String>>(11)?,
        row.get::<_, Option<f64>>(12)?,
        row.get::<_, Option<f64>>(13)?,
        row.get::<_, Option<f64>>(14)?,
        row.get::<_, Option<f64>>(15)?,
        row.get::<_, Option<f64>>(16)?,
        row.get::<_, Option<f64>>(17)?,
        row.get::<_, Option<i64>>(18)?,
        row.get::<_, Option<f64>>(19)?,
        row.get::<_, Option<f64>>(20)?,
        row.get::<_, Option<f64>>(21)?,
        row.get::<_, Option<f64>>(22)?,
        row.get::<_, Option<f64>>(23)?,
        row.get::<_, Option<f64>>(24)?,
        row.get::<_, Option<String>>(25)?,
        row.get::<_, Option<String>>(26)?,
    ))
}

/// Decode blob columns + assemble a `Route` from a [`RouteRow`] tuple.
fn build_route_from_row(r: RouteRow) -> Result<Route> {
    let (
        file, date, raw_park_count, raw_frame_count,
        pb, gb, ab, sb, acb, rb,
        source, external_signature, tessie_autopilot_percent,
        battery_pct_start, battery_pct_end,
        interior_temp_min, interior_temp_max, exterior_temp_avg,
        hvac_runtime_s,
        tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
        odometer_mi_start, odometer_mi_end,
        location_name_start, location_name_end,
    ) = r;
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
    Ok(Route {
        file, date, points, gear_states, autopilot_states,
        speeds, accel_positions, raw_park_count, raw_frame_count, gear_runs,
        source, external_signature, tessie_autopilot_percent,
        battery_pct_start, battery_pct_end,
        interior_temp_min, interior_temp_max, exterior_temp_avg,
        hvac_runtime_s,
        tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
        odometer_mi_start, odometer_mi_end,
        location_name_start, location_name_end,
        temp_samples: Vec::new(),
    })
}

/// Select BLOB-free summary rows — metadata + v2 aggregate columns +
/// v6 telemetry rollups. The telemetry columns may be NULL on pre-v6
/// rows or routes whose 60s window had no samples; the consumer
/// handles that via the `Option` shape inside `RouteTelemetryAggregates`.
fn select_all_route_summaries(conn: &Connection) -> Result<Vec<RouteSummary>> {
    let mut stmt = conn.prepare_cached(
        "SELECT file, date_dir, raw_park_count, raw_frame_count, gear_runs_blob,
                distance_m, max_speed_mps, avg_speed_mps, speed_sample_count,
                valid_point_count, fsd_engaged_ms, autosteer_engaged_ms,
                tacc_engaged_ms, fsd_distance_m, autosteer_distance_m,
                tacc_distance_m, assisted_distance_m,
                fsd_disengagements, fsd_accel_pushes,
                start_lat, start_lon, end_lat, end_lon,
                source, external_signature,
                battery_pct_start, battery_pct_end,
                interior_temp_min, interior_temp_max, exterior_temp_avg,
                hvac_runtime_s,
                tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi,
                odometer_mi_start, odometer_mi_end,
                location_name_start, location_name_end,
                fsd_pend_ms_end, park_ms_start, fsd_at_end, fsd_accel_pushes_early,
                ap_at_start
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
            row.get::<_, Option<String>>(23)?,
            row.get::<_, Option<String>>(24)?,
            // v6 telemetry columns (battery_temp_avg intentionally
            // not fetched — Tesla doesn't expose battery cell temp)
            row.get::<_, Option<f64>>(25)?,
            row.get::<_, Option<f64>>(26)?,
            row.get::<_, Option<f64>>(27)?,
            row.get::<_, Option<f64>>(28)?,
            row.get::<_, Option<f64>>(29)?,
            row.get::<_, Option<i64>>(30)?,
            // v7 TPMS columns
            (
                row.get::<_, Option<f64>>(31)?,
                row.get::<_, Option<f64>>(32)?,
                row.get::<_, Option<f64>>(33)?,
                row.get::<_, Option<f64>>(34)?,
            ),
            // v9 odometer. software_version intentionally not
            // fetched — Tesla doesn't expose car_version over BLE.
            (
                row.get::<_, Option<f64>>(35)?,
                row.get::<_, Option<f64>>(36)?,
            ),
            // v10 location names (start / end)
            (
                row.get::<_, Option<String>>(37)?,
                row.get::<_, Option<String>>(38)?,
            ),
            // v15 clip-boundary state
            (
                row.get::<_, Option<f64>>(39)?,
                row.get::<_, Option<f64>>(40)?,
                row.get::<_, Option<i64>>(41)?,
                row.get::<_, Option<i64>>(42)?,
                row.get::<_, Option<i64>>(43)?,
            ),
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
            source,
            external_signature,
            battery_pct_start,
            battery_pct_end,
            interior_temp_min,
            interior_temp_max,
            exterior_temp_avg,
            hvac_runtime_s,
            (tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi),
            (odometer_mi_start, odometer_mi_end),
            (location_name_start, location_name_end),
            (fsd_pend_ms_end, park_ms_start, fsd_at_end, fsd_accel_pushes_early, ap_at_start),
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
                fsd_pend_ms_end,
                park_ms_start,
                fsd_at_end: fsd_at_end.unwrap_or(0) != 0,
                fsd_accel_pushes_early: fsd_accel_pushes_early.unwrap_or(0) as i32,
                ap_at_start: ap_at_start.map(|v| v as i32),
                start_lat,
                start_lng: start_lon,
                end_lat,
                end_lng: end_lon,
            },
            source,
            external_signature,
            telemetry: crate::types::RouteTelemetryAggregates {
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
    let (stats, diag) =
        crate::json_compat::import_json(conn, source_path, |routes_imported| {
            info!("[drives] Import progress: {} routes", routes_imported);
        })
        .with_context(|| format!("import_json {}", source_path))?;
    info!(
        "[drives] Import complete: {} routes, {} processed files, {} tags",
        stats.routes, stats.processed_files, stats.drive_tags
    );
    if let Err(e) = persist_import_history(conn, &stats, &diag) {
        warn!("[drives] failed to persist import history: {}", e);
    }

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
    // Cross-filesystem fallback. fs::copy streams in fixed-size chunks —
    // the export JSON can reach hundreds of MB and some Pis have 1 GB of
    // RAM, so never buffer the whole file in memory.
    std::fs::copy(src, dst)?;
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
/// `routes`.
/// Canonical DB key for a clip path: forward slashes, and the snapshot
/// symlink layout's `RecentClips/` prefix stripped so the Pi's native
/// ingest (`RecentClips/YYYY-MM-DD/x.mp4`) and Sentry-Drive imports
/// (`YYYY-MM-DD/x.mp4`) key the same physical clip identically.
/// `SavedClips/`/`SentryClips/` prefixes are kept — the grouper filters
/// event-folder rows by exactly those prefixes.
pub fn normalize_path(p: &str) -> String {
    let n = p.replace('\\', "/");
    match n.strip_prefix("RecentClips/") {
        Some(rest) if !rest.is_empty() => rest.to_string(),
        _ => n,
    }
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
    fn route_overview_cache_matches_live_and_invalidates_on_mutation() {
        let store = DriveStore::open_memory().unwrap();
        let add = |file: &str, lat: f64| {
            store
                .add_route(
                    file,
                    "2025-01-15",
                    &vec![[lat, -122.4194], [lat + 0.001, -122.4195]],
                    &[4, 4],
                    &[1, 1],
                    &[25.0, 26.0],
                    &[0.5, 0.6],
                    0,
                    2,
                    &[GearRun { gear: 4, frames: 2 }],
                )
                .unwrap();
        };

        add("2025-01-15/2025-01-15_10-00-00-front.mp4", 37.7749);
        // Cache == live output.
        let live1 = serde_json::to_string(&crate::grouper::route_overviews(
            store.get_routes().unwrap(),
            20,
        ))
        .unwrap();
        let cached1 = store.get_cached_route_overviews_json(20).unwrap();
        assert_eq!(cached1, live1, "cache must equal live output");
        // Hit.
        assert_eq!(store.get_cached_route_overviews_json(20).unwrap(), live1);

        // Mutation invalidates.
        add("2025-01-15/2025-01-15_11-00-00-front.mp4", 40.0000);
        let live2 = serde_json::to_string(&crate::grouper::route_overviews(
            store.get_routes().unwrap(),
            20,
        ))
        .unwrap();
        let cached2 = store.get_cached_route_overviews_json(20).unwrap();
        assert_ne!(cached2, cached1, "cache must refresh after a new route");
        assert_eq!(cached2, live2, "refreshed cache must equal live output");
    }

    #[test]
    fn off_lock_rebuild_works_on_file_backed_store() {
        // The in-memory tests exercise the shared-connection fallback;
        // this pins the real path: snapshot via a dedicated read-only
        // connection (WAL), compute unlocked, write back. Mirrors the
        // temp-file pattern syncguard's tests use (no tempfile dep).
        let path = std::env::temp_dir().join(format!(
            "sentryusb-offlock-test-{}-{}.db",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path_str = path.to_str().unwrap().to_string();

        let store = DriveStore::open(&path_str).unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7760, -122.4180]];
        store
            .add_route("a/2025-02-02_09-00-00-front.mp4", "a", &pts, &[4, 4], &[0, 0], &[15.0, 16.0], &[0.0, 0.0], 0, 2, &[])
            .unwrap();
        let json = store.get_cached_drives_json().unwrap();
        assert!(json.contains("2025-02-02"), "file-backed rebuild should serve the drive: {json}");
        assert!(!store.drive_cache_dirty.load(Ordering::Acquire));

        drop(store);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path_str));
        let _ = std::fs::remove_file(format!("{}-shm", path_str));
    }

    #[test]
    fn off_lock_rebuild_serves_fresh_cache_and_redirties_on_mutation() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7760, -122.4180]];
        store
            .add_route("a/2025-01-01_10-00-00-front.mp4", "a", &pts, &[4, 4], &[0, 0], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[])
            .unwrap();
        // add_route marked the cache dirty; the getter must rebuild via the
        // off-lock path and serve a list containing the new drive.
        let json = store.get_cached_drives_json().unwrap();
        assert!(json.contains("2025-01-01"), "cache should contain the drive: {json}");
        assert!(!store.drive_cache_dirty.load(Ordering::Acquire));

        // A further mutation re-dirties; the next read rebuilds again and
        // reflects it (second route is 2.5h later — a separate drive).
        store
            .add_route("a/2025-01-01_12-30-00-front.mp4", "a", &pts, &[4, 4], &[0, 0], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[])
            .unwrap();
        assert!(store.drive_cache_dirty.load(Ordering::Acquire));
        let json2 = store.get_cached_drives_json().unwrap();
        assert!(
            json2.matches("\"id\"").count() > json.matches("\"id\"").count(),
            "rebuilt cache should reflect the new route: {json2}"
        );

        // Stats + FSD caches were written by the same rebuild.
        let stats = store.get_cached_drive_stats_json().unwrap();
        assert!(stats.contains("drives_count"));
    }

    #[test]
    fn aggregate_formula_migration_recomputes_without_violating_not_null() {
        // Regression: the formula-version reset must NOT try to set the
        // NOT NULL `distance_m` column to NULL (that aborts the DB open).
        // It NULLs the nullable v2 columns to queue rows for the backfill,
        // which recomputes everything from the BLOB.
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7760, -122.4180]];
        store
            .add_route("a.mp4", "2025-01-01", &pts, &[4, 4], &[1, 1], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[])
            .unwrap();

        {
            let conn = store.conn.lock().unwrap();
            // Force the gate so the next load() runs the reset + recompute.
            meta_set(&conn, "aggregate_formula_version", "stale-test").unwrap();
            // Simulate a stale haversine distance + a non-NULL gate column,
            // so the migration path (not a fresh insert) is exercised.
            conn.execute(
                "UPDATE routes SET distance_m = 999.0, max_speed_mps = 12.3",
                [],
            )
            .unwrap();
        }

        // Must not error (the NOT NULL trap), and must recompute distance.
        store.load().unwrap();

        let out = store.with_route_summaries(|s| s.to_vec()).unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].aggregates.distance_m > 0.0);
        assert!(
            (out[0].aggregates.distance_m - 999.0).abs() > 1.0,
            "stale distance should have been recomputed, got {}",
            out[0].aggregates.distance_m
        );

        let conn = store.conn.lock().unwrap();
        let ver = meta_get(&conn, "aggregate_formula_version").unwrap();
        assert_eq!(ver.as_deref(), Some(AGGREGATE_FORMULA_VERSION));
    }

    #[test]
    fn gap_fill_files_excludes_parked_event_rows() {
        use crate::extract::{GEAR_DRIVE, GEAR_PARK};
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.0, -122.0], [37.001, -122.0]];

        // A driving event row (real gap-fill) — must appear.
        store
            .add_route(
                "SentryClips/2026-07-05_16-12-51/2026-07-05_16-05-47-front.mp4",
                "2026-07-05",
                &pts,
                &[GEAR_DRIVE, GEAR_DRIVE],
                &[0, 0],
                &[20.0, 21.0],
                &[0.0, 0.0],
                0,
                2,
                &[GearRun { gear: GEAR_DRIVE, frames: 2 }],
            )
            .unwrap();

        // A fully-PARKED event row (drive 241's arrival residue from an
        // older build, before the do_process gate) — must be filtered out
        // so the playback manifest never cross-links it into RecentClips.
        let parked_pts: Vec<GpsPoint> = vec![[37.001, -122.0], [37.001, -122.0]];
        store
            .add_route(
                "SentryClips/2026-07-05_16-12-51/2026-07-05_16-07-48-front.mp4",
                "2026-07-05",
                &parked_pts,
                &[GEAR_PARK, GEAR_PARK],
                &[0, 0],
                &[0.0, 0.0],
                &[0.0, 0.0],
                60,
                60,
                &[GearRun { gear: GEAR_PARK, frames: 60 }],
            )
            .unwrap();

        // A normal RecentClips drive row is never in the event-folder set.
        store
            .add_route(
                "2026-07-05/2026-07-05_16-04-47-front.mp4",
                "2026-07-05",
                &pts,
                &[GEAR_DRIVE, GEAR_DRIVE],
                &[0, 0],
                &[20.0, 21.0],
                &[0.0, 0.0],
                0,
                2,
                &[GearRun { gear: GEAR_DRIVE, frames: 2 }],
            )
            .unwrap();

        let files = store.gap_fill_files().unwrap();
        assert_eq!(
            files,
            vec!["SentryClips/2026-07-05_16-12-51/2026-07-05_16-05-47-front.mp4".to_string()],
            "manifest must list the driving event clip and exclude the parked arrival"
        );
    }

    /// 61-point clip (dt = 1000 ms) for boundary tests: per-point AP and
    /// gear arrays, valid GPS, no gear_runs (no mid-clip park splitting).
    fn add_boundary_clip(store: &DriveStore, file: &str, ap: Vec<u8>, gears: Vec<u8>) {
        let n = 61;
        let mut pts: Vec<GpsPoint> = Vec::with_capacity(n);
        for i in 0..n {
            pts.push([37.7749 + (i as f64) * 0.00001, -122.4194]);
        }
        let speeds = vec![10.0f32; n];
        let accel = vec![0.0f32; n];
        store
            .add_route(file, "2025-01-01", &pts, &gears, &ap, &speeds, &accel, 0, 61, &[])
            .unwrap();
    }

    #[test]
    fn disengagement_boundary_resolved_across_clips() {
        use crate::extract::{AUTOPILOT_FSD, AUTOPILOT_OFF, GEAR_PARK};
        let store = DriveStore::open_memory().unwrap();

        // Drive 1: clip ends with a pending disengagement (FSD off at
        // 59s), next clip parks within the remaining grace window →
        // "FSD parked the car", NOT a disengagement.
        let pend_ap: Vec<u8> = (0..61)
            .map(|i| if i < 59 { AUTOPILOT_FSD } else { AUTOPILOT_OFF })
            .collect();
        let mut park_gears = vec![4u8; 61];
        park_gears[0] = GEAR_PARK;
        park_gears[1] = GEAR_PARK;
        add_boundary_clip(&store, "2025-01-01/2025-01-01_10-00-00-front.mp4", pend_ap.clone(), vec![4; 61]);
        add_boundary_clip(&store, "2025-01-01/2025-01-01_10-01-00-front.mp4", vec![AUTOPILOT_OFF; 61], park_gears);

        // Drive 2 (2.5h later, separate drive): same pending end, but the
        // next clip never parks → a real driver disengagement.
        add_boundary_clip(&store, "2025-01-01/2025-01-01_13-00-00-front.mp4", pend_ap, vec![4; 61]);
        add_boundary_clip(&store, "2025-01-01/2025-01-01_13-01-00-front.mp4", vec![AUTOPILOT_OFF; 61], vec![4; 61]);

        let drives: serde_json::Value =
            serde_json::from_str(&store.get_cached_drives_json().unwrap()).unwrap();
        let arr = drives.as_array().unwrap();
        assert_eq!(arr.len(), 2, "expected two drives: {drives}");
        let dis_for = |start: &str| -> i64 {
            arr.iter()
                .find(|d| d["startTime"].as_str().unwrap_or("").starts_with(start))
                .unwrap_or_else(|| panic!("drive {start} missing: {drives}"))
                ["fsdDisengagements"]
                .as_i64()
                .unwrap()
        };
        assert_eq!(
            dis_for("2025-01-01T10:00"), 0,
            "park within grace across the clip seam must cancel the disengagement"
        );
        assert_eq!(
            dis_for("2025-01-01T13:00"), 1,
            "no park across the seam must count the disengagement"
        );
    }

    #[test]
    fn trailing_pending_disengagement_counts_at_drive_end() {
        use crate::extract::{AUTOPILOT_FSD, AUTOPILOT_OFF};
        let store = DriveStore::open_memory().unwrap();
        // Single-clip drive ending with a pending disengagement and no
        // Park before recording stops — matches Sentry-Drive's flush rule.
        let pend_ap: Vec<u8> = (0..61)
            .map(|i| if i < 59 { AUTOPILOT_FSD } else { AUTOPILOT_OFF })
            .collect();
        add_boundary_clip(&store, "2025-01-01/2025-01-01_10-00-00-front.mp4", pend_ap, vec![4; 61]);
        let drives: serde_json::Value =
            serde_json::from_str(&store.get_cached_drives_json().unwrap()).unwrap();
        assert_eq!(drives[0]["fsdDisengagements"].as_i64(), Some(1), "{drives}");
    }

    #[test]
    fn non_sei_sources_excluded_from_fsd_stats() {
        use crate::extract::AUTOPILOT_FSD;
        let store = DriveStore::open_memory().unwrap();
        // One FSD-heavy clip... imported from Teslascope. Totals must
        // include its distance; FSD analytics must not (dashcam-only),
        // matching Sentry-Drive's "anything non-SEI is imported" rule.
        add_boundary_clip(&store, "2025-01-01/2025-01-01_10-00-00-front.mp4", vec![AUTOPILOT_FSD; 61], vec![4; 61]);
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("UPDATE routes SET source = 'teslascope'", []).unwrap();
        }
        store.drive_cache_dirty.store(true, Ordering::Release);
        let stats: serde_json::Value =
            serde_json::from_str(&store.get_cached_drive_stats_json().unwrap()).unwrap();
        assert!(
            stats["total_distance_km"].as_f64().unwrap() > 0.0,
            "imported drives still count toward totals: {stats}"
        );
        assert_eq!(
            stats["fsd_distance_km"].as_f64(), Some(0.0),
            "imported drives must not feed FSD analytics: {stats}"
        );
    }

    /// Like `add_boundary_clip` but with a custom base latitude so two
    /// clips can sit a known gap apart for bridge-attribution tests.
    fn add_boundary_clip_at(
        store: &DriveStore,
        file: &str,
        ap: Vec<u8>,
        gears: Vec<u8>,
        base_lat: f64,
    ) {
        let n = 61;
        let mut pts: Vec<GpsPoint> = Vec::with_capacity(n);
        for i in 0..n {
            pts.push([base_lat + (i as f64) * 0.00001, -122.4194]);
        }
        let speeds = vec![10.0f32; n];
        let accel = vec![0.0f32; n];
        store
            .add_route(file, "2025-01-01", &pts, &gears, &ap, &speeds, &accel, 0, 61, &[])
            .unwrap();
    }

    #[test]
    fn fsd_seam_time_and_bridge_distance_attributed() {
        use crate::extract::AUTOPILOT_FSD;
        let store = DriveStore::open_memory().unwrap();
        // Two all-FSD clips, 62s apart: a 2s wall-clock seam plus a ~55m
        // GPS gap between clip A's end and clip B's start. Sentry-Drive's
        // merged walk attributes both to FSD (next point is engaged);
        // per-clip sums alone drop them.
        add_boundary_clip_at(
            &store, "2025-01-01/2025-01-01_10-00-00-front.mp4",
            vec![AUTOPILOT_FSD; 61], vec![4; 61], 37.7749,
        );
        add_boundary_clip_at(
            &store, "2025-01-01/2025-01-01_10-01-02-front.mp4",
            vec![AUTOPILOT_FSD; 61], vec![4; 61], 37.7760,
        );
        let drives: serde_json::Value =
            serde_json::from_str(&store.get_cached_drives_json().unwrap()).unwrap();
        let d = &drives[0];
        let engaged = d["fsdEngagedMs"].as_i64().unwrap();
        assert!(
            (121_500..=122_500).contains(&engaged),
            "2s seam must join the 2x60s of engaged time, got {engaged}: {d}"
        );
        let fsd_km = d["fsdDistanceKm"].as_f64().unwrap();
        assert!(
            fsd_km > 0.17,
            "bridge segment (~55m) must count as FSD distance on top of ~133m in-clip, got {fsd_km}"
        );
    }

    #[test]
    fn analytics_avgs_use_sei_drive_denominator_and_sd_grades() {
        use crate::extract::{AUTOPILOT_FSD, AUTOPILOT_OFF};
        let store = DriveStore::open_memory().unwrap();
        // Drive 1: FSD for the first 55s, then disengaged (1 disengagement).
        let ap1: Vec<u8> = (0..61)
            .map(|i| if i < 55 { AUTOPILOT_FSD } else { AUTOPILOT_OFF })
            .collect();
        add_boundary_clip(&store, "2025-01-01/2025-01-01_10-00-00-front.mp4", ap1, vec![4; 61]);
        // Drive 2 (3h later): manual the whole way — still an SEI drive.
        add_boundary_clip(
            &store, "2025-01-01/2025-01-01_13-00-00-front.mp4",
            vec![AUTOPILOT_OFF; 61], vec![4; 61],
        );

        let analytics = store
            .with_route_summaries(|s| {
                crate::grouper::fsd_analytics_from_summaries_for_period(s, "all")
            })
            .unwrap();
        // Sentry-Drive divides the averages by ALL SEI drives in the
        // period, not just drives where FSD engaged: 1 disengagement
        // over 2 drives = 0.5.
        assert_eq!(
            analytics.avg_disengagements_per_drive, 0.5,
            "avg must use the SEI drive count as denominator"
        );
        // ~45% FSD share lands in Sentry-Drive's "Okay" band
        // (>=90 Great, >=70 Good, >=40 Okay, else Bad).
        assert!(
            (40.0..70.0).contains(&analytics.fsd_percent),
            "fixture should land in the Okay band, got {}",
            analytics.fsd_percent
        );
        assert_eq!(analytics.fsd_grade, "Okay");
    }

    #[test]
    fn aggregate_formula_gate_invalidates_drive_caches() {
        // The formula gate recomputes every routes row from the BLOBs,
        // but neither the reset nor the backfill bumps updated_at — so
        // the drive caches' validity marker (algo version + row counts +
        // max updated_at) still matches and a stale cache would survive
        // the recompute. The gate must force a rebuild itself.
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7760, -122.4180]];
        store
            .add_route("2025-01-01/2025-01-01_10-00-00-front.mp4", "2025-01-01", &pts, &[4, 4], &[1, 1], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[])
            .unwrap();
        // Plant an absurd stored distance and bake it INTO the caches —
        // this is the old-formula world the gate exists to replace.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("UPDATE routes SET distance_m = 999000.0", []).unwrap();
        }
        store.drive_cache_dirty.store(true, Ordering::Release);
        let stale: serde_json::Value =
            serde_json::from_str(&store.get_cached_drive_stats_json().unwrap()).unwrap();
        assert!(
            stale["total_distance_km"].as_f64().unwrap() > 900.0,
            "precondition: cache must hold the planted value: {stale}"
        );
        {
            let conn = store.conn.lock().unwrap();
            meta_set(&conn, "aggregate_formula_version", "stale-test").unwrap();
        }
        store.load().unwrap();
        let stats: serde_json::Value =
            serde_json::from_str(&store.get_cached_drive_stats_json().unwrap()).unwrap();
        let km = stats["total_distance_km"].as_f64().unwrap();
        assert!(
            km < 10.0,
            "stats cache must reflect the recomputed aggregates, not the stale 999km: {stats}"
        );
    }

    #[test]
    fn normalize_path_canonicalizes_recentclips_prefix() {
        // The Pi's native processor sees clips under the snapshot symlink
        // layout (`RecentClips/YYYY-MM-DD/x.mp4`) while Sentry-Drive
        // exports key the same clip as `YYYY-MM-DD/x.mp4`. Both must
        // normalize to the same DB key or every clip ingests twice.
        assert_eq!(
            normalize_path("RecentClips/2026-06-07/x-front.mp4"),
            "2026-06-07/x-front.mp4"
        );
        assert_eq!(
            normalize_path("RecentClips\\2026-06-07\\x-front.mp4"),
            "2026-06-07/x-front.mp4"
        );
        // Already-canonical input is unchanged (idempotent).
        assert_eq!(
            normalize_path("2026-06-07/x-front.mp4"),
            "2026-06-07/x-front.mp4"
        );
        // Event-folder prefixes are NOT stripped — the grouper filters
        // those rows by this exact prefix.
        assert_eq!(
            normalize_path("SavedClips/2026-06-07_10-00-00/x-front.mp4"),
            "SavedClips/2026-06-07_10-00-00/x-front.mp4"
        );
        // Only a whole leading component counts.
        assert_eq!(normalize_path("MyRecentClips/x.mp4"), "MyRecentClips/x.mp4");
        assert_eq!(normalize_path("RecentClips/x-front.mp4"), "x-front.mp4");
        // Tessie import paths are untouched.
        assert_eq!(
            normalize_path("tessie/2026-04-18/x-front-tessie-1.mp4"),
            "tessie/2026-04-18/x-front-tessie-1.mp4"
        );
    }

    #[test]
    fn native_and_imported_route_share_one_canonical_row() {
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7760, -122.4180]];
        // Pi-native ingest under the snapshot symlink layout.
        store
            .add_route(
                "RecentClips/2026-06-07/c-front.mp4", "2026-06-07", &pts,
                &[4, 4], &[1, 1], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[],
            )
            .unwrap();
        // Sentry-Drive-style import of the same clip (Windows separators).
        store
            .add_route(
                "2026-06-07\\c-front.mp4", "2026-06-07", &pts,
                &[4, 4], &[1, 1], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[],
            )
            .unwrap();

        let (n, file) = {
            let conn = store.conn.lock().unwrap();
            let n: i64 = conn
                .query_row("SELECT count(*) FROM routes", [], |r| r.get(0))
                .unwrap();
            let file: String = conn
                .query_row("SELECT file FROM routes LIMIT 1", [], |r| r.get(0))
                .unwrap();
            (n, file)
        };
        assert_eq!(n, 1, "same clip under two path spellings must collapse to one route row");
        assert_eq!(file, "2026-06-07/c-front.mp4");

        // Both spellings count as processed, so the processor never
        // re-extracts a clip the import already covered (and vice versa).
        assert!(store.is_processed("RecentClips/2026-06-07/c-front.mp4").unwrap());
        assert!(store.is_processed("2026-06-07/c-front.mp4").unwrap());
        // The processed set the processor's bulk filter uses holds the
        // canonical key.
        let set = store.processed_set().unwrap();
        assert!(set.contains("2026-06-07/c-front.mp4"));
        assert!(!set.iter().any(|f| f.starts_with("RecentClips/")));
    }

    #[test]
    fn route_key_canon_migration_dedupes_and_renames() {
        let store = DriveStore::open_memory().unwrap();
        let pts2: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7760, -122.4180]];
        let pts3: Vec<GpsPoint> =
            vec![[37.7749, -122.4194], [37.7755, -122.4187], [37.7760, -122.4180]];

        // Import twin (canonical key, 2 points).
        store
            .add_route(
                "2026-06-07/dup-front.mp4", "2026-06-07", &pts2,
                &[4, 4], &[1, 1], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[],
            )
            .unwrap();
        // Native copy of the same clip (3 points), a lone native row, and a
        // stray SavedClips event row. add_route now canonicalizes, so build
        // the legacy keys via raw UPDATE, the same way the aggregate gate
        // test simulates stale rows.
        for (tmp, legacy, pts) in [
            ("tmp-dup.mp4", "RecentClips/2026-06-07/dup-front.mp4", &pts3),
            ("tmp-lone.mp4", "RecentClips/2026-06-08/lone-front.mp4", &pts2),
            ("tmp-event.mp4", "SavedClips/2026-06-07_10-00-00/ev-front.mp4", &pts2),
        ] {
            store
                .add_route(
                    tmp, "2026-06-07", pts,
                    &[4, 4], &[1, 1], &[20.0, 21.0], &[0.0, 0.0], 0, 2, &[],
                )
                .unwrap();
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "UPDATE routes SET file = ?1 WHERE file = ?2",
                params![legacy, tmp],
            )
            .unwrap();
            conn.execute(
                "UPDATE processed_files SET file = ?1 WHERE file = ?2",
                params![legacy, tmp],
            )
            .unwrap();
        }

        {
            let conn = store.conn.lock().unwrap();
            // Force the gate so the next load() runs the migration.
            meta_set(&conn, "route_key_format_version", "stale-test").unwrap();
        }
        store.load().unwrap();

        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT file, point_count FROM routes ORDER BY file")
            .unwrap();
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows.iter().map(|(f, _)| f.as_str()).collect::<Vec<_>>(),
            vec!["2026-06-07/dup-front.mp4", "2026-06-08/lone-front.mp4"],
            "dup collapses to canonical, lone renames, event row deleted: {:?}",
            rows
        );
        // The native (RecentClips) copy wins the dedupe — it carries the
        // device's own extraction + BLE telemetry columns.
        assert_eq!(rows[0].1, 3, "native 3-point row should replace the import twin");

        // processed_files canonicalized the RecentClips rows; the event row
        // entry is left alone (the processor skips those dirs at scan).
        let pf: Vec<String> = {
            let mut s = conn.prepare("SELECT file FROM processed_files ORDER BY file").unwrap();
            s.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        assert!(pf.contains(&"2026-06-07/dup-front.mp4".to_string()));
        assert!(pf.contains(&"2026-06-08/lone-front.mp4".to_string()));
        assert!(!pf.iter().any(|f| f.starts_with("RecentClips/")), "{:?}", pf);

        let ver = meta_get(&conn, "route_key_format_version").unwrap();
        assert_eq!(ver.as_deref(), Some(ROUTE_KEY_FORMAT_VERSION));
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
    fn charge_tags_set_get_and_replace() {
        let store = DriveStore::open_memory().unwrap();
        // Keyed on the session start-ts; sorted on read.
        store
            .set_charge_tags(1700, &["Public".to_string(), "Home".to_string()])
            .unwrap();
        assert_eq!(
            store.get_charge_tags(1700).unwrap(),
            vec!["Home".to_string(), "Public".to_string()],
        );

        // A second session is independent.
        store
            .set_charge_tags(1800, &["Work".to_string()])
            .unwrap();

        // Replace semantics: set overwrites, empty drops the entry.
        store
            .set_charge_tags(1700, &["Home".to_string()])
            .unwrap();
        assert_eq!(store.get_charge_tags(1700).unwrap(), vec!["Home".to_string()]);
        store.set_charge_tags(1700, &[]).unwrap();
        assert!(store.get_charge_tags(1700).unwrap().is_empty());

        // The map + name list reflect only what remains (session 1800).
        let map = store.get_all_charge_tags().unwrap();
        assert_eq!(map.get(&1800), Some(&vec!["Work".to_string()]));
        assert!(!map.contains_key(&1700));
        assert_eq!(store.get_all_charge_tag_names().unwrap(), vec!["Work".to_string()]);
    }

    #[test]
    fn charge_cost_override_set_get_clear() {
        let store = DriveStore::open_memory().unwrap();
        // No override initially.
        assert_eq!(store.get_charge_cost(1700).unwrap(), None);

        // Set an override; read it back with its currency symbol.
        store
            .set_charge_cost(1700, Some((24.50, "$".to_string())))
            .unwrap();
        assert_eq!(
            store.get_charge_cost(1700).unwrap(),
            Some((24.50, "$".to_string())),
        );

        // Upsert replaces the amount in place (PRIMARY KEY, no dup row).
        store
            .set_charge_cost(1700, Some((30.0, "$".to_string())))
            .unwrap();
        assert_eq!(
            store.get_charge_cost(1700).unwrap(),
            Some((30.0, "$".to_string())),
        );

        // A second session is independent; the map carries both.
        store
            .set_charge_cost(1800, Some((12.0, "€".to_string())))
            .unwrap();
        let map = store.get_all_charge_costs().unwrap();
        assert_eq!(map.get(&1700), Some(&(30.0, "$".to_string())));
        assert_eq!(map.get(&1800), Some(&(12.0, "€".to_string())));

        // None clears the override.
        store.set_charge_cost(1700, None).unwrap();
        assert_eq!(store.get_charge_cost(1700).unwrap(), None);
        assert!(!store.get_all_charge_costs().unwrap().contains_key(&1700));
    }

    /// End-to-end contract for the `drive_tags` join key.
    ///
    /// Regression test for the v2.9.x "Add tag" bug: the PUT handler used
    /// to pass the raw URL id (typically the numeric DriveSummary.id, e.g.
    /// `"0"`) straight into `set_drive_tags`, but the grouper joins tags
    /// onto drives using the `%Y-%m-%dT%H:%M:%S` start_time string. The
    /// row was written but never matched on read, so the tag silently
    /// vanished from the UI even though the request returned 200.
    ///
    /// This test:
    ///   1. Pins down the contract: rows keyed by start_time DO surface.
    ///   2. Pins down the failure mode: rows keyed by the numeric idx do
    ///      NOT surface — so future refactors of the handler can't
    ///      regress this without breaking the test.
    ///   3. Exercises `find_drive_start_time` as the bridge the handler
    ///      uses to translate numeric ids into the canonical key.
    #[test]
    fn drive_tags_join_on_start_time_string_not_numeric_id() {
        use crate::grouper;
        let store = DriveStore::open_memory().unwrap();
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(
                "2025-01-15_12-30-45-front.mp4",
                "2025-01-15",
                &pts,
                &[4, 4],
                &[0, 0],
                &[10.0, 10.0],
                &[0.0, 0.0],
                0,
                2,
                &[GearRun { gear: 4, frames: 2 }],
            )
            .unwrap();

        // Sanity: grouper sees one drive whose start_time is the
        // parsed-from-filename `%Y-%m-%dT%H:%M:%S` form.
        let (drive_id, drive_start_time) = store
            .with_route_summaries(|s| {
                let drives = grouper::group_summaries_fast(s, &std::collections::HashMap::new());
                (drives[0].id, drives[0].start_time.clone())
            })
            .unwrap();
        assert_eq!(drive_id, 0);
        assert_eq!(drive_start_time, "2025-01-15T12:30:45");

        // The resolver must translate the numeric URL id into the
        // start_time the grouper joins on.
        let resolved = store
            .with_route_summaries(|s| grouper::find_drive_start_time(s, "0"))
            .unwrap();
        assert_eq!(resolved.as_deref(), Some("2025-01-15T12:30:45"));
        // And accept the start_time form too (single_drive does).
        let resolved_st = store
            .with_route_summaries(|s| grouper::find_drive_start_time(s, "2025-01-15T12:30:45"))
            .unwrap();
        assert_eq!(resolved_st.as_deref(), Some("2025-01-15T12:30:45"));
        // Bogus id resolves to None — handler returns 404.
        let bogus = store
            .with_route_summaries(|s| grouper::find_drive_start_time(s, "999"))
            .unwrap();
        assert!(bogus.is_none());

        // Negative control: storing under the raw numeric id (the old
        // broken path) MUST NOT surface the tag on the drive.
        store
            .set_drive_tags("0", &["BugCanary".to_string()])
            .unwrap();
        let tags_after_bad = store.get_all_drive_tags().unwrap();
        let drive_after_bad = store
            .with_route_summaries(|s| {
                grouper::group_summaries_fast(s, &tags_after_bad)[0]
                    .tags
                    .clone()
            })
            .unwrap();
        assert!(
            drive_after_bad.is_empty(),
            "numeric-keyed tag rows must not surface — got {:?}",
            drive_after_bad,
        );

        // Positive: storing under the resolved start_time key DOES surface.
        store
            .set_drive_tags(&resolved.unwrap(), &["Work".to_string()])
            .unwrap();
        let tags_after_good = store.get_all_drive_tags().unwrap();
        let drive_after_good = store
            .with_route_summaries(|s| {
                grouper::group_summaries_fast(s, &tags_after_good)[0]
                    .tags
                    .clone()
            })
            .unwrap();
        assert_eq!(drive_after_good, vec!["Work".to_string()]);
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

    // -- sync_to_archive_at / restore_from_archive_at ------------------------

    /// Unique temp directory cleaned up on drop (same no-tempfile-dep
    /// pattern as syncguard's tests).
    struct TmpDir(std::path::PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir().join(format!(
                "sentryusb-syncarch-{}-{}-{}",
                tag,
                std::process::id(),
                nanos
            ));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self, name: &str) -> String {
            self.0.join(name).to_str().unwrap().to_string()
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn add_test_route(store: &DriveStore, name: &str) {
        let pts: Vec<GpsPoint> = vec![[37.7749, -122.4194], [37.7750, -122.4195]];
        store
            .add_route(name, "2025-01-15", &pts, &[4, 4], &[1, 1], &[25.0, 26.0], &[0.5, 0.6], 0, 2, &[])
            .unwrap();
    }

    #[test]
    fn archive_sync_pushes_then_skips_until_new_routes() {
        let dir = TmpDir::new("push-skip");
        let (mirror, archive, cache) =
            (dir.path("mirror.json"), dir.path("archive.json"), dir.path("cache"));
        let store = DriveStore::open_memory().unwrap();
        add_test_route(&store, "a/2025-01-15_10-00-00-front.mp4");

        // First sync: exports the mirror and copies it to the archive.
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        let pushed = std::fs::read_to_string(&archive).unwrap();
        assert_eq!(pushed, std::fs::read_to_string(&mirror).unwrap());
        assert!(pushed.contains("2025-01-15"));

        // Same route count again: short-circuits, archive untouched.
        std::fs::write(&archive, "sentinel").unwrap();
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), "sentinel");

        // New route moves the baseline: archive rewritten.
        add_test_route(&store, "a/2025-01-15_11-00-00-front.mp4");
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        let repushed = std::fs::read_to_string(&archive).unwrap();
        assert_ne!(repushed, "sentinel");
        assert!(repushed.contains("11-00-00"));
    }

    /// A charging-only day adds telemetry samples without moving the
    /// route count — the sync must still re-export so the archive backup
    /// covers the new charging history.
    #[test]
    fn archive_sync_pushes_when_only_telemetry_added() {
        let dir = TmpDir::new("telemetry-push");
        let (mirror, archive, cache) =
            (dir.path("mirror.json"), dir.path("archive.json"), dir.path("cache"));
        let store = DriveStore::open_memory().unwrap();
        add_test_route(&store, "a/2025-01-15_10-00-00-front.mp4");
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();

        // No new routes, but the sampler wrote a row: archive rewritten.
        std::fs::write(&archive, "sentinel").unwrap();
        store.with_locked_conn(|conn| {
            conn.execute(
                "INSERT INTO telemetry_samples (ts, battery_pct, source) VALUES (1700000000, 72.5, 'state')",
                [],
            )
            .unwrap();
        });
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        let repushed = std::fs::read_to_string(&archive).unwrap();
        assert_ne!(repushed, "sentinel");
        assert!(repushed.contains("telemetrySamples"));

        // Nothing new since: short-circuits again.
        std::fs::write(&archive, "sentinel2").unwrap();
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), "sentinel2");
    }

    #[test]
    fn archive_sync_noop_on_empty_store() {
        let dir = TmpDir::new("empty");
        let (mirror, archive, cache) =
            (dir.path("mirror.json"), dir.path("archive.json"), dir.path("cache"));
        let store = DriveStore::open_memory().unwrap();

        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        assert!(!Path::new(&archive).exists(), "empty store must not create an archive copy");
        assert!(!Path::new(&mirror).exists(), "empty store must not export a mirror");
    }

    #[test]
    fn archive_sync_restores_into_empty_store_without_clobbering() {
        let dir = TmpDir::new("restore");
        let (mirror, archive, cache) =
            (dir.path("mirror.json"), dir.path("archive.json"), dir.path("cache"));
        let store = DriveStore::open_memory().unwrap();
        std::fs::write(&archive, "precious-backup").unwrap();

        // Empty store + archive copy + no mirror → reflash recovery:
        // pull the backup down, never push over it.
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        assert_eq!(std::fs::read_to_string(&mirror).unwrap(), "precious-backup");
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), "precious-backup");

        // Next cycle (mirror now present, store still empty): still no push.
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        assert_eq!(std::fs::read_to_string(&archive).unwrap(), "precious-backup");
    }

    #[test]
    fn archive_sync_prefers_push_when_store_has_data() {
        let dir = TmpDir::new("stale-archive");
        let (mirror, archive, cache) =
            (dir.path("mirror.json"), dir.path("archive.json"), dir.path("cache"));
        let store = DriveStore::open_memory().unwrap();
        add_test_route(&store, "a/2025-01-15_10-00-00-front.mp4");
        std::fs::write(&archive, "stale-go-era-copy").unwrap();

        // Populated store + missing mirror is the post-port CIFS state:
        // restore must NOT trigger; the stale archive copy gets replaced.
        store.sync_to_archive_at(&mirror, &archive, &cache).unwrap();
        let pushed = std::fs::read_to_string(&archive).unwrap();
        assert_ne!(pushed, "stale-go-era-copy");
        assert!(pushed.contains("2025-01-15"));
    }
}
