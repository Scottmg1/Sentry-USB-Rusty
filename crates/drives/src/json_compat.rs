// JSON import/export for migration from Go version's drive-data.json.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::db::DriveStore;
use crate::types::{GearRun, GpsPoint, Route};

/// The JSON format used by the Go version's drive-data.json.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyStoreData {
    #[serde(default)]
    processed_files: Vec<String>,
    #[serde(default)]
    routes: Vec<LegacyRoute>,
    #[serde(default)]
    drive_tags: std::collections::HashMap<String, Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyRoute {
    file: String,
    date: String,
    #[serde(default)]
    points: Vec<GpsPoint>,
    #[serde(default)]
    gear_states: Vec<u8>,
    #[serde(default)]
    autopilot_states: Vec<u8>,
    #[serde(default)]
    speeds: Vec<f32>,
    #[serde(default)]
    accel_positions: Vec<f32>,
    #[serde(default)]
    raw_park_count: u32,
    #[serde(default)]
    raw_frame_count: u32,
    #[serde(default)]
    gear_runs: Vec<GearRun>,
}

/// Threshold mirroring Go `server/drives/syncguard.go`: a store with at least
/// this many routes must not be overwritten by an import that shrinks it by
/// more than `SHRINK_RATIO` — that's a near-certain sign of a truncated or
/// corrupted file (disk full mid-write, snapshot corruption, etc.), and the
/// user is better off keeping the existing data than clobbering it with
/// garbage.
const SYNCGUARD_MIN_ROUTES: usize = 1000;
const SYNCGUARD_SHRINK_RATIO: f64 = 0.5;

/// Import a Go-format drive-data.json into the SQLite store.
pub fn import_json(json_path: &str, store: &DriveStore) -> Result<usize> {
    info!("importing legacy JSON from {}", json_path);

    let data = std::fs::read_to_string(json_path)
        .with_context(|| format!("failed to read {}", json_path))?;

    let legacy: LegacyStoreData = serde_json::from_str(&data)
        .with_context(|| "failed to parse legacy JSON")?;

    let route_count = legacy.routes.len();

    // Corruption guard: refuse to overwrite a large existing store with a
    // much smaller import. Matches the intent of Go's syncguard.go.
    let existing_count = store
        .with_routes(|r| r.len())
        .unwrap_or(0);
    if existing_count >= SYNCGUARD_MIN_ROUTES
        && (route_count as f64) < (existing_count as f64) * SYNCGUARD_SHRINK_RATIO
    {
        anyhow::bail!(
            "refusing import: would shrink store from {} routes to {} (< {:.0}% retained). \
             Likely a truncated or corrupted upload — delete the existing DB manually if \
             you really mean to replace it.",
            existing_count,
            route_count,
            SYNCGUARD_SHRINK_RATIO * 100.0,
        );
    }

    info!(
        "importing {} routes and {} processed files (existing: {})",
        route_count,
        legacy.processed_files.len(),
        existing_count,
    );

    // Import routes
    for lr in &legacy.routes {
        let route = Route {
            file: lr.file.clone(),
            date: lr.date.clone(),
            points: lr.points.clone(),
            gear_states: lr.gear_states.clone(),
            autopilot_states: lr.autopilot_states.clone(),
            speeds: lr.speeds.clone(),
            accel_positions: lr.accel_positions.clone(),
            raw_park_count: lr.raw_park_count,
            raw_frame_count: lr.raw_frame_count,
            gear_runs: lr.gear_runs.clone(),
        };
        store.upsert_route(&route)?;
    }

    // Import processed files
    for file in &legacy.processed_files {
        store.mark_processed(file)?;
    }

    // Import drive tags
    for (drive_id, tags) in &legacy.drive_tags {
        store.set_tags(drive_id, tags)?;
    }

    info!("import complete: {} routes", route_count);
    Ok(route_count)
}

/// Export the SQLite store back to Go-compatible JSON format.
pub fn export_json(store: &DriveStore, json_path: &str) -> Result<()> {
    info!("exporting to JSON at {}", json_path);

    let routes = store.get_all_routes()?;
    let tags = store.get_all_drive_tags()?;

    // Collect processed files
    // Note: we don't have a direct "get all processed files" yet, but
    // for export purposes we can derive it from routes
    let processed_files: Vec<String> = routes.iter().map(|r| r.file.clone()).collect();

    let legacy = LegacyStoreData {
        processed_files,
        routes: routes
            .into_iter()
            .map(|r| LegacyRoute {
                file: r.file,
                date: r.date,
                points: r.points,
                gear_states: r.gear_states,
                autopilot_states: r.autopilot_states,
                speeds: r.speeds,
                accel_positions: r.accel_positions,
                raw_park_count: r.raw_park_count,
                raw_frame_count: r.raw_frame_count,
                gear_runs: r.gear_runs,
            })
            .collect(),
        drive_tags: tags,
    };

    let json = serde_json::to_string_pretty(&legacy)?;
    std::fs::write(json_path, json)
        .with_context(|| format!("failed to write {}", json_path))?;

    info!("export complete");
    Ok(())
}
