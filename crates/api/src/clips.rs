//! Clip listing and telemetry.

use std::path::Path;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::router::AppState;

const TESLACAM_DIR: &str = "/mutable/TeslaCam";

#[derive(Deserialize)]
pub struct ClipParams {
    #[serde(rename = "type")]
    clip_type: Option<String>,
    date: Option<String>,
}

#[derive(Serialize)]
struct ClipGroup {
    name: String,
    path: String,
    clips: Vec<ClipEntry>,
    timestamp: String,
}

#[derive(Serialize)]
struct ClipEntry {
    name: String,
    path: String,
    size: i64,
    camera: String,
}

/// GET /api/clips
pub async fn get_clips(
    State(_s): State<AppState>,
    Query(params): Query<ClipParams>,
) -> (StatusCode, Json<serde_json::Value>) {
    let clip_type = params.clip_type.as_deref().unwrap_or("SavedClips");

    // Validate clip type
    if !matches!(clip_type, "SavedClips" | "SentryClips" | "RecentClips") {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid clip type");
    }

    let base = Path::new(TESLACAM_DIR).join(clip_type);
    if !base.exists() {
        return (StatusCode::OK, Json(serde_json::json!({"groups": []})));
    }

    let mut groups = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&base) {
        let mut event_dirs: Vec<_> = entries.flatten()
            .filter(|e| e.path().is_dir())
            .collect();
        event_dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

        // Filter by date if specified
        if let Some(date) = &params.date {
            event_dirs.retain(|e| e.file_name().to_string_lossy().starts_with(date.as_str()));
        }

        for event_dir in event_dirs {
            let dir_name = event_dir.file_name().to_string_lossy().to_string();
            let dir_path = event_dir.path();
            let mut clips = Vec::new();

            if let Ok(files) = std::fs::read_dir(&dir_path) {
                for file in files.flatten() {
                    let name = file.file_name().to_string_lossy().to_string();
                    if name.ends_with(".mp4") {
                        let size = std::fs::metadata(file.path())
                            .map(|m| m.len() as i64)
                            .unwrap_or(0);

                        let camera = if name.contains("-front") { "front" }
                            else if name.contains("-back") { "back" }
                            else if name.contains("-left_repeater") { "left_repeater" }
                            else if name.contains("-right_repeater") { "right_repeater" }
                            else if name.contains("-left_pillar") { "left_pillar" }
                            else if name.contains("-right_pillar") { "right_pillar" }
                            else { "unknown" };

                        clips.push(ClipEntry {
                            path: format!("{}/{}/{}", clip_type, dir_name, name),
                            name,
                            size,
                            camera: camera.to_string(),
                        });
                    }
                }
            }

            clips.sort_by(|a, b| a.name.cmp(&b.name));

            groups.push(ClipGroup {
                timestamp: dir_name.clone(),
                path: format!("{}/{}", clip_type, dir_name),
                name: dir_name,
                clips,
            });
        }
    }

    (StatusCode::OK, Json(serde_json::json!({"groups": groups})))
}

/// GET /api/clips/telemetry
pub async fn get_clip_telemetry(
    State(state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let file = match params.get("file") {
        Some(f) => f,
        None => return crate::json_error(StatusCode::BAD_REQUEST, "missing file parameter"),
    };

    // Extract GPS from the specified clip
    let full_path = format!("{}/{}", TESLACAM_DIR, file);
    match sentryusb_drives::extract::extract_gps_from_file(&full_path) {
        Ok(gps) => {
            (StatusCode::OK, Json(serde_json::json!({
                "points": gps.points,
                "gear_states": gps.gear_states,
                "speeds": gps.speeds,
                "autopilot_states": gps.autopilot_states,
            })))
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}
