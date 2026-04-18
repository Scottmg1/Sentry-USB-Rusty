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

/// GET /api/clips/telemetry
pub async fn get_clip_telemetry(
    State(_state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let file = match params.get("file") {
        Some(f) => f,
        None => return crate::json_error(StatusCode::BAD_REQUEST, "missing file parameter"),
    };

    // The client passes `path` as the relative event-dir path (optionally
    // prefixed with the `/TeslaCam/` URL mount we serve videos from).
    let clip_path = params.get("path").map(|s| s.as_str()).unwrap_or("");
    let clip_rel = clip_path.trim_start_matches('/').trim_start_matches("TeslaCam/");
    let full_path = if clip_rel.is_empty() {
        format!("{}/{}", TESLACAM_DIR, file)
    } else {
        format!("{}/{}/{}", TESLACAM_DIR, clip_rel, file)
    };
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
