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

/// Read a `RecentClips/`, `SavedClips/`, or `SentryClips/` directory and return
/// its dated subfolders, newest first.
///
/// All three categories share this layout under `/mutable/TeslaCam`: the
/// snapshot symlink builder (`sentryusb_gadget::snapshot`) date-buckets the
/// car's flat RecentClips files into `YYYY-MM-DD/` folders, matching the
/// `YYYY-MM-DD_HH-MM-SS/` event folders SavedClips/SentryClips already use.
/// `path().is_dir()` follows symlinks — required, since each entry is a
/// symlink into a reflink snapshot.
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

/// Build Viewer JSON for one category of dated clip folders.
fn list_clips_in(
    teslacam_dir: &Path,
    category: &str,
    limit: usize,
    before: Option<&str>,
) -> serde_json::Value {
    let base = teslacam_dir.join(category);
    if !base.exists() {
        return serde_json::json!([{
            "name": category,
            "clips": [],
            "hasMore": false,
        }]);
    }

    let mut event_dirs = enumerate_event_dirs(&base);
    if let Some(before) = before {
        event_dirs.retain(|d| d.as_str() < before);
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

    serde_json::json!([{
        "name": category,
        "clips": entries,
        "hasMore": has_more,
    }])
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

    // Autofs snapshot mounts can make the first directory read block.
    let category = category.to_string();
    let before = params.before;
    let response = tokio::task::spawn_blocking(move || {
        list_clips_in(Path::new(TESLACAM_DIR), &category, limit, before.as_deref())
    })
    .await
    .unwrap_or_else(|_| serde_json::json!([]));
    (StatusCode::OK, Json(response))
}

/// GET /api/clips/telemetry?path=/TeslaCam/SentryClips/<event>&file=<camera>.mp4
///
/// Response shape expected by the web UI:
/// { frames: [{t, lat, lng, speed_mps, gear, autopilot, accel_pos}], duration_sec, has_gps, has_autopilot }
pub async fn get_clip_telemetry(
    State(_state): State<AppState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Video parsing is blocking and CPU-intensive.
    tokio::task::spawn_blocking(move || clip_telemetry_blocking(params))
        .await
        .unwrap_or_else(|_| {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "telemetry task failed")
        })
}

fn clip_telemetry_blocking(
    params: std::collections::HashMap<String, String>,
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

    // Reject lexical traversal or absolute rewrites outside TESLACAM_DIR.
    let cleaned = {
        let mut p = std::path::PathBuf::from("/");
        for component in std::path::Path::new(&full_path).components() {
            match component {
                std::path::Component::Normal(c) => p.push(c),
                std::path::Component::RootDir => p = std::path::PathBuf::from("/"),
                std::path::Component::ParentDir => {
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

    // Deduplication would destroy the frame-to-time overlay mapping.
    let (points, gear_states, ap_states, speeds, accel_positions, flag_bytes, accel_x, accel_y) =
        match sentryusb_drives::extract::extract_gps_from_file_raw(cleaned_str.as_ref()) {
            Ok(raw) => raw,
            Err(e) => return crate::json_error(StatusCode::NOT_FOUND, &format!("could not read file: {}", e)),
        };

    let video_duration = mp4_duration_sec(cleaned_str.as_ref()).unwrap_or(0.0);
    let num_points = points.len();
    let mut frames = Vec::with_capacity(num_points);
    let mut has_gps = false;
    let mut has_autopilot = false;
    for (i, pt) in points.iter().enumerate() {
        let ap = *ap_states.get(i).unwrap_or(&sentryusb_drives::extract::AUTOPILOT_OFF);
        let gear = *gear_states.get(i).unwrap_or(&0);
        let speed = *speeds.get(i).unwrap_or(&0.0);
        let accel = *accel_positions.get(i).unwrap_or(&0.0);
        if pt[0] != 0.0 || pt[1] != 0.0 {
            has_gps = true;
        }
        if ap != sentryusb_drives::extract::AUTOPILOT_OFF {
            has_autopilot = true;
        }
        let t = if num_points > 1 && video_duration > 0.0 {
            video_duration * (i as f64) / ((num_points - 1) as f64)
        } else {
            0.0
        };
        // Bitfield: 1 left blinker, 2 right blinker, 4 brake, 8 accelerator.
        let flags = *flag_bytes.get(i).unwrap_or(&0);
        // IMU lateral/longitudinal acceleration; absent SEI fields yield zero.
        let gx = *accel_x.get(i).unwrap_or(&0.0);
        let gy = *accel_y.get(i).unwrap_or(&0.0);
        frames.push(serde_json::json!({
            "t": t,
            "lat": pt[0],
            "lng": pt[1],
            "speed_mps": speed,
            "gear": gear,
            "autopilot": ap,
            "accel_pos": accel,
            "flags": flags,
            "accel_mps2_x": gx,
            "accel_mps2_y": gy,
        }));
    }
    let duration_sec = if video_duration > 0.0 { video_duration } else { 0.0 };

    (StatusCode::OK, Json(serde_json::json!({
        "frames": frames,
        "duration_sec": duration_sec,
        "has_gps": has_gps,
        "has_autopilot": has_autopilot,
    })))
}

fn mp4_duration_sec(path: &str) -> Option<f64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let file_size = f.metadata().ok()?.len();
    let mut pos: u64 = 0;
    let mut buf8 = [0u8; 8];
    while pos < file_size {
        f.seek(SeekFrom::Start(pos)).ok()?;
        f.read_exact(&mut buf8).ok()?;
        let mut box_size = u32::from_be_bytes([buf8[0], buf8[1], buf8[2], buf8[3]]) as u64;
        let box_type = [buf8[4], buf8[5], buf8[6], buf8[7]];
        if box_size == 1 {
            let mut ext = [0u8; 8];
            f.read_exact(&mut ext).ok()?;
            box_size = u64::from_be_bytes(ext);
        } else if box_size == 0 {
            box_size = file_size - pos;
        }
        if &box_type == b"moov" {
            let moov_end = pos + box_size;
            let mut mpos = pos + 8;
            while mpos < moov_end {
                f.seek(SeekFrom::Start(mpos)).ok()?;
                let mut mhdr = [0u8; 8];
                f.read_exact(&mut mhdr).ok()?;
                let msz = u32::from_be_bytes([mhdr[0], mhdr[1], mhdr[2], mhdr[3]]) as u64;
                if &mhdr[4..8] == b"mvhd" {
                    let mut vf = [0u8; 4];
                    f.read_exact(&mut vf).ok()?;
                    let ver = vf[0];
                    if ver == 0 {
                        let mut skip = [0u8; 8];
                        f.read_exact(&mut skip).ok()?;
                        let mut b4 = [0u8; 4];
                        f.read_exact(&mut b4).ok()?;
                        let timescale = u32::from_be_bytes(b4) as f64;
                        f.read_exact(&mut b4).ok()?;
                        let duration = u32::from_be_bytes(b4) as f64;
                        if timescale > 0.0 {
                            return Some(duration / timescale);
                        }
                    }
                }
                if msz < 8 { break; }
                mpos += msz;
            }
            return None;
        }
        if box_size < 8 { break; }
        pos += box_size;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn enumerate_event_dirs_returns_subfolders_newest_first() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("2025-02-22_17-58-00")).unwrap();
        fs::create_dir(dir.path().join("2025-02-23_09-12-00")).unwrap();
        fs::write(dir.path().join("README.txt"), b"").unwrap();

        let dirs = enumerate_event_dirs(dir.path());
        assert_eq!(dirs, vec!["2025-02-23_09-12-00", "2025-02-22_17-58-00"]);
    }

    #[test]
    fn lists_recent_clips_from_dated_subdirs() {
        let root = TempDir::new().unwrap();
        let day = root.path().join("RecentClips").join("2025-02-22");
        fs::create_dir_all(&day).unwrap();
        fs::write(day.join("2025-02-22_17-58-00-front.mp4"), b"").unwrap();
        fs::write(day.join("2025-02-22_17-58-00-back.mp4"), b"").unwrap();

        let value = list_clips_in(root.path(), "RecentClips", 20, None);
        assert_eq!(value[0]["name"].as_str().unwrap(), "RecentClips");
        assert_eq!(value[0]["hasMore"].as_bool().unwrap(), false);

        let clips = value[0]["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0]["date"].as_str().unwrap(), "2025-02-22");
        assert_eq!(
            clips[0]["path"].as_str().unwrap(),
            "/TeslaCam/RecentClips/2025-02-22",
        );
        let files: Vec<&str> = clips[0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            files,
            vec![
                "2025-02-22_17-58-00-back.mp4",
                "2025-02-22_17-58-00-front.mp4",
            ],
        );
        assert!(clips[0].get("event").is_none());
    }

    #[test]
    fn lists_event_clips_with_event_json() {
        let root = TempDir::new().unwrap();
        let event = root.path().join("SentryClips").join("2025-02-22_17-58-00");
        fs::create_dir_all(&event).unwrap();
        fs::write(event.join("2025-02-22_17-58-00-front.mp4"), b"").unwrap();
        fs::write(
            event.join("event.json"),
            r#"{"city":"San Francisco, CA","reason":"sentry_aware_object_detection"}"#,
        )
        .unwrap();

        let value = list_clips_in(root.path(), "SentryClips", 20, None);
        let clips = value[0]["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0]["date"].as_str().unwrap(), "2025-02-22_17-58-00");
        assert_eq!(
            clips[0]["path"].as_str().unwrap(),
            "/TeslaCam/SentryClips/2025-02-22_17-58-00",
        );
        assert_eq!(clips[0]["event"]["city"].as_str().unwrap(), "San Francisco, CA");
        assert_eq!(
            clips[0]["event"]["reason"].as_str().unwrap(),
            "sentry_aware_object_detection",
        );
    }

    #[test]
    fn list_clips_respects_limit_and_before() {
        let root = TempDir::new().unwrap();
        let saved = root.path().join("SavedClips");
        for name in &[
            "2025-02-20_10-00-00",
            "2025-02-21_10-00-00",
            "2025-02-22_10-00-00",
        ] {
            let d = saved.join(name);
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join(format!("{}-front.mp4", name)), b"").unwrap();
        }

        let value = list_clips_in(root.path(), "SavedClips", 2, None);
        assert_eq!(value[0]["hasMore"].as_bool().unwrap(), true);
        let clips = value[0]["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0]["date"].as_str().unwrap(), "2025-02-22_10-00-00");
        assert_eq!(clips[1]["date"].as_str().unwrap(), "2025-02-21_10-00-00");

        let value = list_clips_in(root.path(), "SavedClips", 20, Some("2025-02-22_10-00-00"));
        assert_eq!(value[0]["hasMore"].as_bool().unwrap(), false);
        let clips = value[0]["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0]["date"].as_str().unwrap(), "2025-02-21_10-00-00");
        assert_eq!(clips[1]["date"].as_str().unwrap(), "2025-02-20_10-00-00");
    }

    #[test]
    fn list_clips_empty_for_missing_category_dir() {
        let root = TempDir::new().unwrap();
        let value = list_clips_in(root.path(), "SavedClips", 20, None);
        assert_eq!(value[0]["name"].as_str().unwrap(), "SavedClips");
        assert_eq!(value[0]["clips"].as_array().unwrap().len(), 0);
        assert_eq!(value[0]["hasMore"].as_bool().unwrap(), false);
    }

    #[cfg(unix)]
    #[test]
    fn list_clips_follows_symlinked_dirs() {
        let root = TempDir::new().unwrap();
        let saved = root.path().join("SavedClips");
        fs::create_dir_all(&saved).unwrap();

        let real = root.path().join("snapshot").join("2025-02-22_17-58-00");
        fs::create_dir_all(&real).unwrap();
        fs::write(real.join("2025-02-22_17-58-00-front.mp4"), b"").unwrap();

        std::os::unix::fs::symlink(&real, saved.join("2025-02-22_17-58-00")).unwrap();

        let value = list_clips_in(root.path(), "SavedClips", 20, None);
        let clips = value[0]["clips"].as_array().unwrap();
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0]["date"].as_str().unwrap(), "2025-02-22_17-58-00");
        let files: Vec<&str> = clips[0]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(files, vec!["2025-02-22_17-58-00-front.mp4"]);
    }
}
