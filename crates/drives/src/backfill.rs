//! v1 → v2 aggregate backfill — port of Go `server/drives/backfill.go`.
//!
//! Walks every route row whose aggregate columns are still NULL (the
//! pre-v2 state) and populates them from the stored BLOBs via
//! [`compute_route_aggregates`](crate::aggregate::compute_route_aggregates).
//! Runs in batched transactions so the WAL doesn't grow unbounded on
//! a 5500-route upgrade.
//!
//! Self-healing idempotency: uses `max_speed_mps IS NULL` as the sentinel,
//! so the pass correctly resumes from wherever an interrupted previous
//! run stopped. The `summary_backfilled_at` marker written by the caller
//! is observability-only — NOT the primary exit condition.

use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::aggregate::compute_route_aggregates;
use crate::blob::{decode_f32s, decode_gear_runs, decode_points, decode_u8s};
use crate::types::Route;

/// Routes per batch. Small enough to stay within ~5 MB heap; large
/// enough to amortize fsync cost. Matches the JSON importer's batch
/// size for a consistent memory profile.
const BACKFILL_BATCH_SIZE: i64 = 200;

/// Stats reported to caller and (via Load path) to WebSocket listeners.
#[derive(Debug, Default, Clone, Copy)]
pub struct BackfillStats {
    pub updated: i64,
}

// ── Process-wide migration progress tracker ────────────────────────────
//
// Lets the API expose `GET /api/drives/migration-status` so the iOS / web
// app can show "Migrating drive data..." UI during a first-boot-after-
// upgrade backfill instead of a stale spinner. Mirrors Go's
// `dh.store.MigrationStatus()` shape (server/api/drives.go:151+).
//
// Global atomics (one DriveStore per process) make this trivially safe
// to read concurrently without holding the SQLite mutex.

use std::sync::atomic::{AtomicBool, AtomicI64};
use std::sync::OnceLock;
use std::sync::Mutex;

static MIGRATION_ACTIVE: AtomicBool = AtomicBool::new(false);
static MIGRATION_DONE: AtomicI64 = AtomicI64::new(0);
static MIGRATION_TOTAL: AtomicI64 = AtomicI64::new(0);
static MIGRATION_DISK_FULL: AtomicBool = AtomicBool::new(false);
static MIGRATION_ERROR: OnceLock<Mutex<String>> = OnceLock::new();

fn err_slot() -> &'static Mutex<String> {
    MIGRATION_ERROR.get_or_init(|| Mutex::new(String::new()))
}

/// Snapshot of migration state — what the API surfaces in
/// `GET /api/drives/migration-status`.
#[derive(Debug, Clone)]
pub struct MigrationStatus {
    pub active: bool,
    pub done: i64,
    pub total: i64,
    pub error: String,
    pub disk_full: bool,
}

/// Read the current migration state. Safe to call from any thread.
pub fn migration_status() -> MigrationStatus {
    MigrationStatus {
        active: MIGRATION_ACTIVE.load(Ordering::Acquire),
        done: MIGRATION_DONE.load(Ordering::Relaxed),
        total: MIGRATION_TOTAL.load(Ordering::Relaxed),
        error: err_slot().lock().map(|g| g.clone()).unwrap_or_default(),
        disk_full: MIGRATION_DISK_FULL.load(Ordering::Relaxed),
    }
}

fn migration_begin(total: i64) {
    MIGRATION_DONE.store(0, Ordering::Relaxed);
    MIGRATION_TOTAL.store(total, Ordering::Relaxed);
    MIGRATION_DISK_FULL.store(false, Ordering::Relaxed);
    if let Ok(mut g) = err_slot().lock() { g.clear(); }
    MIGRATION_ACTIVE.store(true, Ordering::Release);
}

fn migration_finish_ok() {
    MIGRATION_ACTIVE.store(false, Ordering::Release);
}

fn migration_finish_err(msg: &str, disk_full: bool) {
    if let Ok(mut g) = err_slot().lock() { *g = msg.to_string(); }
    MIGRATION_DISK_FULL.store(disk_full, Ordering::Relaxed);
    MIGRATION_ACTIVE.store(false, Ordering::Release);
}

/// Run the backfill to completion. `on_progress` is called with
/// `(done, total)` after each batch commit; pass a no-op closure if
/// the caller doesn't need progress updates.
pub fn backfill_route_aggregates<F>(
    conn: &mut Connection,
    mut on_progress: F,
) -> Result<BackfillStats>
where
    F: FnMut(i64, i64),
{
    let mut stats = BackfillStats::default();

    // Figure out total work for progress reporting. An approximate count
    // is fine — new inserts during backfill are rare (processor is
    // usually idle at boot) and wouldn't change the order of magnitude.
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM routes WHERE max_speed_mps IS NULL",
            [],
            |row| row.get(0),
        )
        .context("backfill: count NULL rows")?;
    if total == 0 {
        return Ok(stats);
    }

    // Publish "active" + total to the process-wide tracker so the API
    // endpoint can surface progress to the UI without holding the SQLite
    // mutex on every poll.
    migration_begin(total);

    let result: Result<()> = (|| {
        loop {
            let updated = backfill_one_batch(conn).context("backfill batch")?;
            if updated == 0 {
                break;
            }
            stats.updated += updated;
            MIGRATION_DONE.store(stats.updated, Ordering::Relaxed);
            on_progress(stats.updated, total);
        }
        Ok(())
    })();

    match result {
        Ok(()) => {
            migration_finish_ok();
            Ok(stats)
        }
        Err(e) => {
            // Heuristic: rusqlite error messages for "database or disk
            // is full" / "disk I/O error" mark the disk_full flag so the
            // UI can prompt the user to free space rather than retry.
            let msg = e.to_string();
            let disk_full = msg.to_lowercase().contains("disk is full")
                || msg.to_lowercase().contains("disk i/o error")
                || msg.to_lowercase().contains("no space left");
            migration_finish_err(&msg, disk_full);
            Err(e)
        }
    }
}

/// Select up to `BACKFILL_BATCH_SIZE` NULL-aggregate rows, decode their
/// BLOBs, compute aggregates, and UPDATE them in a single transaction.
/// Returns the number of rows updated.
fn backfill_one_batch(conn: &mut Connection) -> Result<i64> {
    // Read phase: pull BLOBs + metadata. Done before opening the write
    // transaction so the UPDATE tx stays short.
    struct Row {
        file: String,
        date: String,
        raw_park_count: u32,
        raw_frame_count: u32,
        pb: Option<Vec<u8>>,
        gb: Option<Vec<u8>>,
        ab: Option<Vec<u8>>,
        sb: Option<Vec<u8>>,
        acb: Option<Vec<u8>>,
        rb: Option<Vec<u8>>,
    }

    let mut batch: Vec<Row> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT file, date_dir, raw_park_count, raw_frame_count,
                    points_blob, gear_states_blob, ap_states_blob,
                    speeds_blob, accel_blob, gear_runs_blob
             FROM routes
             WHERE max_speed_mps IS NULL
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![BACKFILL_BATCH_SIZE], |row| {
            Ok(Row {
                file: row.get(0)?,
                date: row.get(1)?,
                raw_park_count: row.get::<_, i64>(2)? as u32,
                raw_frame_count: row.get::<_, i64>(3)? as u32,
                pb: row.get(4)?,
                gb: row.get(5)?,
                ab: row.get(6)?,
                sb: row.get(7)?,
                acb: row.get(8)?,
                rb: row.get(9)?,
            })
        })?;
        for r in rows {
            batch.push(r?);
        }
    }

    if batch.is_empty() {
        return Ok(0);
    }

    // Compute phase (outside the transaction): decode BLOBs and compute
    // aggregates for every row in memory. Heap usage is bounded by
    // `BACKFILL_BATCH_SIZE × avg route size`.
    let mut decoded = Vec::with_capacity(batch.len());
    for r in &batch {
        let points = decode_points(r.pb.as_deref())
            .with_context(|| format!("decode points {}", r.file))?
            .unwrap_or_default();
        let gear_states = decode_u8s(r.gb.as_deref()).unwrap_or_default();
        let autopilot_states = decode_u8s(r.ab.as_deref()).unwrap_or_default();
        let speeds = decode_f32s(r.sb.as_deref())
            .with_context(|| format!("decode speeds {}", r.file))?
            .unwrap_or_default();
        let accel_positions = decode_f32s(r.acb.as_deref())
            .with_context(|| format!("decode accel {}", r.file))?
            .unwrap_or_default();
        let gear_runs = decode_gear_runs(r.rb.as_deref())
            .with_context(|| format!("decode gear_runs {}", r.file))?
            .unwrap_or_default();

        let route = Route {
            file: r.file.clone(),
            date: r.date.clone(),
            points,
            gear_states,
            autopilot_states,
            speeds,
            accel_positions,
            raw_park_count: r.raw_park_count,
            raw_frame_count: r.raw_frame_count,
            gear_runs,
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
        };
        let agg = compute_route_aggregates(&route);
        decoded.push((r.file.clone(), agg));
    }

    // Write phase: apply updates in one transaction.
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "UPDATE routes SET
                distance_m           = ?1,
                max_speed_mps        = ?2,
                avg_speed_mps        = ?3,
                speed_sample_count   = ?4,
                valid_point_count    = ?5,
                fsd_engaged_ms       = ?6,
                autosteer_engaged_ms = ?7,
                tacc_engaged_ms      = ?8,
                fsd_distance_m       = ?9,
                autosteer_distance_m = ?10,
                tacc_distance_m      = ?11,
                assisted_distance_m  = ?12,
                fsd_disengagements   = ?13,
                fsd_accel_pushes     = ?14,
                start_lat            = ?15,
                start_lon            = ?16,
                end_lat              = ?17,
                end_lon              = ?18
             WHERE file = ?19",
        )?;
        for (file, a) in &decoded {
            stmt.execute(params![
                a.distance_m,
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
                file,
            ])
            .with_context(|| format!("update {}", file))?;
        }
    }
    tx.commit()?;

    Ok(decoded.len() as i64)
}
