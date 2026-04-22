//! Setup wizard configuration API.
//!
//! The setup process supports mid-setup reboots (e.g. for dwc2 overlay or
//! root partition shrink). The boot-loop works like this:
//!
//! 1. User clicks "Run Setup" in the web wizard → `POST /api/setup/run`
//! 2. `run_full_setup` creates `SENTRYUSB_SETUP_STARTED`, runs phases.
//! 3. If a phase requires a reboot, setup exits early (marker still present).
//! 4. Pi reboots → systemd starts the web server → `auto_resume_setup()`
//!    sees STARTED without FINISHED → re-spawns `run_full_setup`.
//! 5. `run_full_setup` skips already-completed phases and continues.
//! 6. When all phases finish, STARTED is removed and FINISHED is created.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tracing::info;

use crate::router::AppState;

static SETUP_RUNNING: AtomicBool = AtomicBool::new(false);

const SETUP_FINISHED_PATHS: &[&str] = &[
    "/sentryusb/SENTRYUSB_SETUP_FINISHED",
    "/boot/firmware/SENTRYUSB_SETUP_FINISHED",
    "/boot/SENTRYUSB_SETUP_FINISHED",
];

const SETUP_STARTED_PATHS: &[&str] = &[
    "/sentryusb/SENTRYUSB_SETUP_STARTED",
    "/boot/firmware/SENTRYUSB_SETUP_STARTED",
    "/boot/SENTRYUSB_SETUP_STARTED",
];

fn is_setup_finished() -> bool {
    SETUP_FINISHED_PATHS.iter().any(|p| std::path::Path::new(p).exists())
}

fn is_setup_started() -> bool {
    SETUP_STARTED_PATHS.iter().any(|p| std::path::Path::new(p).exists())
}

/// Call at server startup to resume an interrupted setup after reboot.
pub fn auto_resume_setup(hub: sentryusb_ws::Hub) {
    if is_setup_started() && !is_setup_finished() {
        info!("[setup] Detected interrupted setup (STARTED marker present, no FINISHED). Auto-resuming...");
        spawn_setup(hub);
    }
}

/// GET /api/setup/status
pub async fn get_setup_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let running = SETUP_RUNNING.load(Ordering::Relaxed);
    let finished = is_setup_finished();

    // If setup was started but not finished, treat as running
    let effective_running = running || (!finished && is_setup_started());

    (StatusCode::OK, Json(serde_json::json!({
        "setup_finished": finished,
        "setup_running": effective_running,
    })))
}

/// GET /api/setup/config
pub async fn get_setup_config(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::parse_file(config_path) {
        Ok((active, commented)) => {
            let mut merged = serde_json::Map::new();
            for (k, v) in &commented {
                merged.insert(k.clone(), serde_json::json!({
                    "value": v,
                    "active": false,
                }));
            }
            for (k, v) in &active {
                merged.insert(k.clone(), serde_json::json!({
                    "value": v,
                    "active": true,
                }));
            }
            (StatusCode::OK, Json(serde_json::Value::Object(merged)))
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to read config: {}", e)),
    }
}

/// PUT /api/setup/config
pub async fn save_setup_config(
    State(_s): State<AppState>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Remount filesystem read-write (root fs may be read-only)
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::write_file(config_path, &body) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to write config: {}", e)),
    }
}

/// Shared logic: spawn the setup task in the background.
fn spawn_setup(hub: sentryusb_ws::Hub) {
    if SETUP_RUNNING.swap(true, Ordering::SeqCst) {
        info!("[setup] Setup already running, skipping duplicate spawn");
        return;
    }

    tokio::spawn(async move {
        hub.broadcast("setup_status", &serde_json::json!({"status": "running"}));
        info!("[setup] Starting native Rust setup");

        let hub_progress = hub.clone();
        let hub_phase = hub.clone();
        let emitter = sentryusb_setup::runner::make_emitter(
            move |msg: &str| {
                hub_progress.broadcast("setup_progress", &serde_json::json!({"message": msg}));
            },
            move |id: &str, label: &str| {
                hub_phase.broadcast("setup_phase", &serde_json::json!({"id": id, "label": label}));
            },
        );

        let result = sentryusb_setup::runner::run_full_setup(emitter).await;

        SETUP_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(()) => {
                hub.broadcast("setup", &serde_json::json!({"status": "complete"}));
            }
            Err(e) => {
                tracing::error!("[setup] Failed: {:#}", e);
                hub.broadcast("setup", &serde_json::json!({"status": "error", "error": e.to_string()}));
            }
        }
    });
}

const SETUP_PHASES_FILE: &str = "/sentryusb/setup-phases.jsonl";

/// GET /api/setup/phases — returns the list of phases that have already been
/// announced during the current (possibly multi-reboot) setup run. The web UI
/// fetches this on mount and on WebSocket reconnect so it can reconstruct the
/// phase list that was built up before the tab connected.
pub async fn get_setup_phases(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let phases: Vec<serde_json::Value> = std::fs::read_to_string(SETUP_PHASES_FILE)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "phases": phases })))
}

/// POST /api/setup/run
pub async fn run_setup(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if SETUP_RUNNING.load(Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Setup is already running");
    }

    spawn_setup(s.hub.clone());

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

/// POST /api/setup/test-archive
///
/// Body: JSON map with keys matching sentryusb.conf entries:
/// `ARCHIVE_SYSTEM` (cifs|rsync|rclone|nfs), plus protocol-specific fields.
/// Mirrors `server/api/setup.go:testArchive` — an actual mount/connect probe,
/// not just a ping.
pub async fn test_archive(
    State(_s): State<AppState>,
    Json(params): Json<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let system = params
        .get("ARCHIVE_SYSTEM")
        .map(|s| s.as_str())
        .unwrap_or("");
    if system.is_empty() || system == "none" {
        return crate::json_error(StatusCode::BAD_REQUEST, "No archive system specified");
    }

    let timeout = std::time::Duration::from_secs(15);
    let tmp_dir = "/tmp/sentryusb-archive-test";

    let test_result: Result<(), String> = match system {
        "cifs" => {
            let server = params.get("ARCHIVE_SERVER").cloned().unwrap_or_default();
            let share = params.get("SHARE_NAME").cloned().unwrap_or_default();
            let user = params.get("SHARE_USER").cloned().unwrap_or_default();
            let pass = params.get("SHARE_PASSWORD").cloned().unwrap_or_default();
            let domain = params.get("SHARE_DOMAIN").cloned().unwrap_or_default();
            let cifs_ver = params.get("CIFS_VERSION").cloned().unwrap_or_default();
            if server.is_empty() || share.is_empty() || user.is_empty() || pass.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required CIFS fields");
            }
            let _ = std::fs::create_dir_all(tmp_dir);
            let mut opts = format!("username={},password={},iocharset=utf8", user, pass);
            if !domain.is_empty() {
                opts.push_str(&format!(",domain={}", domain));
            }
            if !cifs_ver.is_empty() {
                opts.push_str(&format!(",vers={}", cifs_ver));
            }
            let src = format!("//{}/{}", server, share);
            let res = sentryusb_shell::run_with_timeout(
                timeout, "mount", &["-t", "cifs", &src, tmp_dir, "-o", &opts],
            ).await;
            if res.is_ok() {
                let _ = sentryusb_shell::run_with_timeout(
                    std::time::Duration::from_secs(5), "umount", &[tmp_dir],
                ).await;
            }
            let _ = std::fs::remove_dir(tmp_dir);
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        "rsync" => {
            let server = params.get("RSYNC_SERVER").cloned().unwrap_or_default();
            let user = params.get("RSYNC_USER").cloned().unwrap_or_default();
            let path = params.get("RSYNC_PATH").cloned().unwrap_or_default();
            if server.is_empty() || user.is_empty() || path.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required rsync fields");
            }
            let target = format!("{}@{}", user, server);
            let res = sentryusb_shell::run_with_timeout(
                timeout, "ssh", &[
                    "-o", "ConnectTimeout=10",
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "BatchMode=yes",
                    &target, "echo", "ok",
                ],
            ).await;
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        "rclone" => {
            let drive = params.get("RCLONE_DRIVE").cloned().unwrap_or_default();
            let rpath = params.get("RCLONE_PATH").cloned().unwrap_or_default();
            if drive.is_empty() || rpath.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required rclone fields");
            }
            let target = format!("{}:{}", drive, rpath);
            let res = sentryusb_shell::run_with_timeout(
                timeout, "rclone", &["lsd", &target],
            ).await;
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        "nfs" => {
            let server = params.get("ARCHIVE_SERVER").cloned().unwrap_or_default();
            let export = params.get("SHARE_NAME").cloned().unwrap_or_default();
            if server.is_empty() || export.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required NFS fields");
            }
            let _ = std::fs::create_dir_all(tmp_dir);
            let src = format!("{}:{}", server, export);
            let res = sentryusb_shell::run_with_timeout(
                timeout, "mount", &["-t", "nfs", &src, tmp_dir, "-o", "nolock,soft,timeo=50"],
            ).await;
            if res.is_ok() {
                let _ = sentryusb_shell::run_with_timeout(
                    std::time::Duration::from_secs(5), "umount", &[tmp_dir],
                ).await;
            }
            let _ = std::fs::remove_dir(tmp_dir);
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        other => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                &format!("Unknown archive system: {}", other),
            );
        }
    };

    match test_result {
        Ok(()) => {
            info!("[setup] Archive test succeeded for {}", system);
            (StatusCode::OK, Json(serde_json::json!({"success": true})))
        }
        Err(mut err_msg) => {
            // Strip the "stderr: " prefix the shell helpers prepend, matching
            // Go's cosmetic cleanup before displaying to the user.
            if let Some(idx) = err_msg.find("stderr: ") {
                err_msg = err_msg[idx + "stderr: ".len()..].to_string();
            }
            tracing::warn!("[setup] Archive test failed for {}: {}", system, err_msg);
            (StatusCode::OK, Json(serde_json::json!({
                "success": false,
                "error": err_msg.trim(),
            })))
        }
    }
}
