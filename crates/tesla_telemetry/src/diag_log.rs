//! Writes a bounded, human-readable BLE status log once per minute.
//!
//! Line format:
//!   [2026-05-25 16:30:12 MDT] car=Awake cam_age=45s
//!     state_age=12s bc_age=18s samples_10m=24
//!
//! `run/archiveloop` parses `state_age=<n>s` from the final line. Keep the
//! field and suffix synchronized with its watchdog parser.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use rusqlite::Connection;
use tracing::{debug, warn};

use crate::usb_watch::{CarState, observe_path};

const LOG_PATH: &str = "/mutable/sentryusb-ble.log";
const CAM_DISK_PATH: &str = "/backingfiles/cam_disk.bin";
/// One line per minute.
const TICK: Duration = Duration::from_secs(60);
/// Rotation threshold; only the newest half is retained.
const ROTATE_AT_BYTES: u64 = 5 * 1024 * 1024; // 5 MB

/// Spawns the process-lifetime logger task.
pub fn spawn(db_path: PathBuf) {
    tokio::task::spawn(async move {
        if let Err(e) = run(db_path).await {
            warn!("BLE diag logger exited: {e:#}");
        }
    });
}

async fn run(db_path: PathBuf) -> Result<()> {
    // Reopen SQLite each tick rather than holding it across awaits.
    let mut ticker = tokio::time::interval(TICK);
    // Skip the immediate first tick before sampling begins.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        match build_line(&db_path) {
            Ok(line) => {
                if let Err(e) = append_line(&line) {
                    debug!("diag log append failed: {e:#}");
                }
            }
            Err(e) => debug!("diag log build failed: {e:#}"),
        }
    }
}

fn build_line(db_path: &Path) -> Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Match the sampler phase machine's disk-activity signal.
    let car = observe_path(Path::new(CAM_DISK_PATH));
    let cam_age_secs: i64 = std::fs::metadata(CAM_DISK_PATH)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| now - d.as_secs() as i64)
        .unwrap_or(-1);

    // Read sample freshness without sharing a connection across ticks.
    let conn = Connection::open_with_flags(
        db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening telemetry DB at {}", db_path.display()))?;
    let state_ts: Option<i64> = conn
        .query_row(
            "SELECT ts FROM telemetry_samples WHERE source='state' \
             ORDER BY ts DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let bc_ts: Option<i64> = conn
        .query_row(
            "SELECT ts FROM telemetry_samples WHERE source='body_controller' \
             ORDER BY ts DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .ok();
    let since_10m = now - 600;
    let samples_10m: i64 = conn
        .query_row(
            "SELECT count(*) FROM telemetry_samples WHERE ts >= ?1",
            (since_10m,),
            |r| r.get(0),
        )
        .unwrap_or(0);

    let local: DateTime<Local> = Local::now();
    let ts_str = local.format("%Y-%m-%d %H:%M:%S %Z").to_string();

    let car_label = match car {
        CarState::Awake => "Awake",
        CarState::Idle => "Idle",
        CarState::Asleep => "Asleep",
    };

    Ok(format!(
        "[{ts_str}] car={car_label} cam_age={cam_age}s state_age={state} bc_age={bc} samples_10m={samples_10m}\n",
        ts_str = ts_str,
        car_label = car_label,
        cam_age = cam_age_secs,
        state = age_token(state_ts, now),
        bc = age_token(bc_ts, now),
        samples_10m = samples_10m,
    ))
}

fn age_token(ts: Option<i64>, now: i64) -> String {
    match ts {
        Some(t) => format!("{}s", (now - t).max(0)),
        None => "<never>".into(),
    }
}

fn append_line(line: &str) -> Result<()> {
    use std::io::Write;
    rotate_if_needed()?;
    // O_APPEND preserves whole-line ordering across concurrent writers.
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(LOG_PATH)
        .with_context(|| format!("opening {} for append", LOG_PATH))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("writing to {}", LOG_PATH))?;
    Ok(())
}

/// Appends a timestamped event to the persistent Bluetooth diagnostics log.
pub fn log_event(msg: &str) {
    let local: DateTime<Local> = Local::now();
    let ts_str = local.format("%Y-%m-%d %H:%M:%S %Z").to_string();
    let line = format!("[{ts_str}] {msg}\n");
    if let Err(e) = append_line(&line) {
        debug!("diag log_event append failed: {e:#}");
    }
}

fn rotate_if_needed() -> Result<()> {
    let meta = match std::fs::metadata(LOG_PATH) {
        Ok(m) => m,
        Err(_) => return Ok(()), // No log to rotate yet.
    };
    if meta.len() < ROTATE_AT_BYTES {
        return Ok(());
    }
    // Retain only the most recent half; this is not an audit log.
    let raw = std::fs::read(LOG_PATH).context("reading log for rotation")?;
    let half = raw.len() / 2;
    // Start at the next complete line.
    let start = raw[half..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| half + p + 1)
        .unwrap_or(half);
    let kept = &raw[start..];
    std::fs::write(LOG_PATH, kept).context("writing rotated log")?;
    Ok(())
}
