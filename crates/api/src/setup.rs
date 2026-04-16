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

        let hub_clone = hub.clone();
        let progress = sentryusb_setup::runner::make_progress(move |msg: &str| {
            hub_clone.broadcast("setup_progress", &serde_json::json!({"message": msg}));
        });

        let result = sentryusb_setup::runner::run_full_setup(progress).await;

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

/// POST /api/setup/run
pub async fn run_setup(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if SETUP_RUNNING.load(Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Setup is already running");
    }

    spawn_setup(s.hub.clone());

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

/// POST /api/setup/test-archive
pub async fn test_archive(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Read archive server from config and test connectivity
    let config_path = sentryusb_config::find_config_path();
    let server = match sentryusb_config::parse_file(config_path) {
        Ok((active, commented)) => {
            sentryusb_config::get_config_value(&active, &commented, "ARCHIVE_SERVER")
        }
        Err(_) => None,
    };

    let server = match server {
        Some(s) if !s.is_empty() && s != "localhost" => s,
        _ => {
            return (StatusCode::OK, Json(serde_json::json!({
                "success": true,
                "output": "No archive server configured"
            })));
        }
    };

    let result = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(10),
        "ping", &["-c", "1", "-W", "5", &server],
    ).await;

    match result {
        Ok(_) => (StatusCode::OK, Json(serde_json::json!({
            "success": true,
            "output": format!("Archive server {} is reachable", server)
        }))),
        Err(_) => (StatusCode::OK, Json(serde_json::json!({
            "success": false,
            "error": format!("Archive server {} is unreachable", server)
        }))),
    }
}
