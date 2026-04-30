//! Clip listing and telemetry.

use std::collections::BTreeMap;
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

/// Validate and extract the `YYYY-MM-DD` prefix of a Tesla clip filename.
fn date_prefix(name: &str) -> Option<&str> {
    let prefix = name.get(..10)?;
    let b = prefix.as_bytes();
    let ok = b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit);
    ok.then_some(prefix)
}

/// Group Tesla's flat-file `RecentClips/` layout by date prefix.
///
/// Tesla stores RecentClips as MP4 files at the top level of the folder
/// (e.g. `2025-02-22_17-58-00-front.mp4`), not in dated subdirectories like
/// SavedClips/SentryClips. We bucket them by the first ten characters of the
/// filename so each date becomes a single `ClipEntry`, and the frontend's
/// existing per-timestamp grouping chains the minute-segments into one
/// continuous playback sequence.
///
/// Returned vector is ordered newest date first; each date's files are sorted
/// alphabetically (which equals chronologically for Tesla's timestamp format).
fn group_recent_clip_files_by_date(filenames: Vec<String>) -> Vec<(String, Vec<String>)> {
    let mut by_date: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in filenames {
        if !name.ends_with(".mp4") { continue; }
        let Some(date) = date_prefix(&name) else { continue; };
        by_date.entry(date.to_string()).or_default().push(name);
    }
    let mut result: Vec<(String, Vec<String>)> = by_date
        .into_iter()
        .map(|(d, mut files)| {
            files.sort();
            (d, files)
        })
        .collect();
    result.sort_by(|a, b| b.0.cmp(&a.0));
    result
}

/// Read a `RecentClips/` directory and group its flat files by date.
fn enumerate_recent_clips(base: &Path) -> Vec<(String, Vec<String>)> {
    let names: Vec<String> = match std::fs::read_dir(base) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_file())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => return Vec::new(),
    };
    group_recent_clip_files_by_date(names)
}

/// Read a `SavedClips/` or `SentryClips/` directory and return the dated
/// subfolders newest first.
fn enumerate_event_dirs(base: &Path) -> Vec<String> {
    let mut dirs: Vec<String> = match std::fs::read_dir(base) {
        Ok(entries) => entries
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => return Vec::new(),
    };
    dirs.sort_by(|a, b| b.cmp(a));
    dirs
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

    if category == "RecentClips" {
        let mut grouped = enumerate_recent_clips(&base);
        if let Some(before) = &params.before {
            grouped.retain(|(d, _)| d.as_str() < before.as_str());
        }
        let has_more = grouped.len() > limit;
        grouped.truncate(limit);

        let path_prefix = format!("/TeslaCam/{}", category);
        let entries: Vec<ClipEntry> = grouped
            .into_iter()
            .map(|(date, files)| ClipEntry {
                date,
                path: path_prefix.clone(),
                files,
                event: None,
            })
            .collect();

        return (
            StatusCode::OK,
            Json(serde_json::json!([{
                "name": category,
                "clips": entries,
                "hasMore": has_more,
            }])),
        );
    }

    // SavedClips / SentryClips: each clip is a dated subfolder.
    let mut event_dirs = enumerate_event_dirs(&base);
    if let Some(before) = &params.before {
        event_dirs.retain(|d| d.as_str() < before.as_str());
    }
    let has_more = event_dirs.len() > limit;
    event_dirs.truncate(limit);

    let mut entries = Vec::with_capacity(event_dirs.len());
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

        entries.push(ClipEntry {
            date: dir_name.clone(),
            path: format!("/TeslaCam/{}/{}", category, dir_name),
            files,
            event,
        });
    }

    (
        StatusCode::OK,
        Json(serde_json::json!([{
            "name": category,
            "clips": entries,
            "hasMore": has_more,
        }])),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn s(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn date_prefix_accepts_well_formed_filename() {
        assert_eq!(
            date_prefix("2025-02-22_17-58-00-front.mp4"),
            Some("2025-02-22"),
        );
    }

    #[test]
    fn date_prefix_rejects_short_or_malformed_names() {
        assert_eq!(date_prefix(""), None);
        assert_eq!(date_prefix("front.mp4"), None);
        assert_eq!(date_prefix("event.json"), None);
        assert_eq!(date_prefix("2025/02/22-front.mp4"), None);
        assert_eq!(date_prefix("XXXX-02-22_17-58-00-front.mp4"), None);
    }

    #[test]
    fn group_recent_clip_files_groups_by_date_newest_first() {
        let files = s(&[
            "2025-02-22_17-58-00-front.mp4",
            "2025-02-22_17-58-00-back.mp4",
            "2025-02-23_09-12-00-front.mp4",
            "event.json",
            "thumb.png",
            "2025-02-22_17-59-00-front.mp4",
        ]);
        let grouped = group_recent_clip_files_by_date(files);
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, "2025-02-23");
        assert_eq!(grouped[0].1, vec!["2025-02-23_09-12-00-front.mp4"]);
        assert_eq!(grouped[1].0, "2025-02-22");
        assert_eq!(
            grouped[1].1,
            vec![
                "2025-02-22_17-58-00-back.mp4",
                "2025-02-22_17-58-00-front.mp4",
                "2025-02-22_17-59-00-front.mp4",
            ],
        );
    }

    #[test]
    fn group_recent_clip_files_skips_non_mp4_and_unparseable_names() {
        let files = s(&[
            "event.json",
            "no-prefix.mp4",
            "2025-02-22_17-58-00-front.txt",
            "2025-02-22_17-58-00-front.mp4",
        ]);
        let grouped = group_recent_clip_files_by_date(files);
        assert_eq!(grouped.len(), 1);
        assert_eq!(grouped[0].1, vec!["2025-02-22_17-58-00-front.mp4"]);
    }

    #[test]
    fn group_recent_clip_files_returns_empty_for_empty_input() {
        let grouped = group_recent_clip_files_by_date(Vec::new());
        assert!(grouped.is_empty());
    }

    #[test]
    fn enumerate_recent_clips_walks_flat_files_only() {
        let dir = TempDir::new().unwrap();
        // Flat MP4 files (the Tesla RecentClips layout)
        for name in &[
            "2025-02-22_17-58-00-front.mp4",
            "2025-02-22_17-58-00-back.mp4",
            "2025-02-23_09-12-00-front.mp4",
        ] {
            fs::write(dir.path().join(name), b"").unwrap();
        }
        // A subfolder that should be ignored (RecentClips shouldn't have these,
        // but we don't want to descend into them if they appear).
        fs::create_dir(dir.path().join("subdir")).unwrap();
        fs::write(
            dir.path().join("subdir").join("2025-02-22_17-58-00-front.mp4"),
            b"",
        )
        .unwrap();

        let grouped = enumerate_recent_clips(dir.path());
        assert_eq!(grouped.len(), 2);
        assert_eq!(grouped[0].0, "2025-02-23");
        assert_eq!(grouped[1].0, "2025-02-22");
        assert_eq!(grouped[1].1.len(), 2);
    }

    #[test]
    fn enumerate_recent_clips_handles_missing_dir() {
        let grouped = enumerate_recent_clips(Path::new("/nonexistent/path/abc"));
        assert!(grouped.is_empty());
    }

    #[test]
    fn enumerate_event_dirs_returns_subfolders_newest_first() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("2025-02-22_17-58-00")).unwrap();
        fs::create_dir(dir.path().join("2025-02-23_09-12-00")).unwrap();
        // Stray file should be ignored
        fs::write(dir.path().join("README.txt"), b"").unwrap();

        let dirs = enumerate_event_dirs(dir.path());
        assert_eq!(dirs, vec!["2025-02-23_09-12-00", "2025-02-22_17-58-00"]);
    }
}
