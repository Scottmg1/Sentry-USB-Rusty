//! Clip listing and telemetry.

use std::path::Path;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::router::AppState;

const TESLACAM_DIR: &str = "/mnt/cam/TeslaCam";

#[derive(Deserialize)]
pub struct ClipParams {
    category: Option<String>,
    limit: Option<usize>,
    before: Option<String>,
}

#[derive(Serialize)]
struct ClipEntry {
    date: String,
    path: String,
    files: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<EventMeta>,
}

#[derive(Serialize, Deserialize)]
struct EventMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    city: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    camera: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latitude: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    longitude: Option<String>,
}

/// GET /api/clips?category=RecentClips&limit=20[&before=<date>]
pub async fn get_clips(
    State(_s): State<AppState>,
    Query(params): Query<ClipParams>,
) -> (StatusCode, Json<serde_json::Value>) {
    let category = params.category.as_deref().unwrap_or("SavedClips");
    if !matches!(category, "SavedClips" | "SentryClips" | "RecentClips") {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid category");
    }
    let limit = params.limit.unwrap_or(20).min(200);

    let base = Path::new(TESLACAM_DIR).join(category);
    let empty_group = serde_json::json!([{
        "name": category,
        "clips": [],
        "hasMore": false,
    }]);
    if !base.exists() {
        return (StatusCode::OK, Json(empty_group));
    }

    // Enumerate event dirs, newest first.
    let mut event_dirs: Vec<String> = match std::fs::read_dir(&base) {
        Ok(entries) => entries.flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => return (StatusCode::OK, Json(empty_group)),
    };
    event_dirs.sort_by(|a, b| b.cmp(a));

    // Pagination: drop everything >= before, then take `limit + 1`
    if let Some(before) = &params.before {
        event_dirs.retain(|d| d.as_str() < before.as_str());
    }
    let has_more = event_dirs.len() > limit;
    event_dirs.truncate(limit);

    let mut clips = Vec::with_capacity(event_dirs.len());
    for dir_name in event_dirs {
        let dir_path = base.join(&dir_name);
        let mut files = Vec::new();
        if let Ok(items) = std::fs::read_dir(&dir_path) {
            for item in items.flatten() {
                let name = item.file_name().to_string_lossy().to_string();
                if name.ends_with(".mp4") {
                    files.push(name);
                }
            }
        }
        files.sort();

        let event = std::fs::read_to_string(dir_path.join("event.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<EventMeta>(&s).ok());

        clips.push(ClipEntry {
            date: dir_name.clone(),
            path: format!("/TeslaCam/{}/{}", category, dir_name),
            files,
            event,
        });
    }

    let response = serde_json::json!([{
        "name": category,
        "clips": clips,
        "hasMore": has_more,
    }]);
    (StatusCode::OK, Json(response))
}

/// GET /api/clips/telemetry?path=/TeslaCam/SentryClips/<event>&file=<camera>.mp4
///
/// Response shape matches the Go `telemetryResponse` the web UI expects:
/// { frames: [{t, lat, lng, speed_mps, gear, autopilot, accel_pos}], duration_sec, has_gps, has_autopilot }
pub async fn get_clip_telemetry(
    State(_state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let file = match params.get("file") {
        Some(f) => f,
        None => return crate::json_error(StatusCode::BAD_REQUEST, "missing file parameter"),
    };
    let clip_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let clip_rel = clip_path.trim_start_matches('/').trim_start_matches("TeslaCam/");

    let full_path = if clip_rel.is_empty() {
        format!("{}/{}", TESLACAM_DIR, file)
    } else {
        format!("{}/{}/{}", TESLACAM_DIR, clip_rel, file)
    };

    // Lexical path cleaning + base-prefix check. Mirrors
    // `clips_telemetry.go:39–45`: reject any path that escapes TESLACAM_DIR
    // via `..`, absolute rewrites, or symlinks on components we normalize away.
    let cleaned = {
        let mut p = std::path::PathBuf::from("/");
        for component in std::path::Path::new(&full_path).components() {
            match component {
                std::path::Component::Normal(c) => p.push(c),
                std::path::Component::RootDir => p = std::path::PathBuf::from("/"),
                std::path::Component::ParentDir => {
                    // Treat any `..` as an attempted escape — refuse.
                    return crate::json_error(
                        StatusCode::FORBIDDEN,
                        "path must be under TeslaCam",
                    );
                }
                _ => {}
            }
        }
        p
    };
    let cleaned_str = cleaned.to_string_lossy();
    if !cleaned_str.starts_with(TESLACAM_DIR) {
        return crate::json_error(StatusCode::FORBIDDEN, "path must be under TeslaCam");
    }

    let gps = match sentryusb_drives::extract::extract_gps_from_file(cleaned_str.as_ref()) {
        Ok(g) => g,
        Err(e) => return crate::json_error(StatusCode::NOT_FOUND, &format!("could not read file: {}", e)),
    };

    const FPS: f64 = 36.0;
    let mut frames = Vec::with_capacity(gps.points.len());
    let mut has_gps = false;
    let mut has_autopilot = false;
    for (i, pt) in gps.points.iter().enumerate() {
        let ap = *gps.autopilot_states.get(i).unwrap_or(&sentryusb_drives::extract::AUTOPILOT_OFF);
        let gear = *gps.gear_states.get(i).unwrap_or(&0);
        let speed = *gps.speeds.get(i).unwrap_or(&0.0);
        let accel = *gps.accel_positions.get(i).unwrap_or(&0.0);
        if pt[0] != 0.0 || pt[1] != 0.0 {
            has_gps = true;
        }
        if ap != sentryusb_drives::extract::AUTOPILOT_OFF {
            has_autopilot = true;
        }
        frames.push(serde_json::json!({
            "t": (i as f64) / FPS,
            "lat": pt[0],
            "lng": pt[1],
            "speed_mps": speed,
            "gear": gear,
            "autopilot": ap,
            "accel_pos": accel,
        }));
    }
    let duration_sec = if frames.is_empty() { 0.0 } else { (frames.len() as f64) / FPS };

    (StatusCode::OK, Json(serde_json::json!({
        "frames": frames,
        "duration_sec": duration_sec,
        "has_gps": has_gps,
        "has_autopilot": has_autopilot,
    })))
}
