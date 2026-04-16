//! Setup wizard configuration API.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

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
    let _ = sentryusb_shell::run("bash", &["-c", "/root/bin/remountfs_rw"]).await;

    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::write_file(config_path, &body) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to write config: {}", e)),
    }
}

/// POST /api/setup/run
pub async fn run_setup(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if SETUP_RUNNING.swap(true, Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Setup is already running");
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        // Remove finished marker so rc.local will re-run setup
        for p in SETUP_FINISHED_PATHS {
            let _ = std::fs::remove_file(p);
        }
        // Create started marker
        for p in SETUP_STARTED_PATHS {
            let _ = std::fs::remove_file(p);
        }
        let _ = std::fs::create_dir_all("/sentryusb");
        let _ = std::fs::write(SETUP_STARTED_PATHS[0], "");
        // Remove resize marker
        let _ = std::fs::remove_file("/root/RESIZE_ATTEMPTED");

        hub.broadcast("setup_status", &serde_json::json!({"status": "running"}));
        tracing::info!("[setup] Running /etc/rc.local (SentryUSB setup boot-loop)");

        // rc.local may reboot the system, which is expected.
        // Timeout is long because setup installs packages, partitions, etc.
        let result = sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(1800),
            "/etc/rc.local",
            &[],
        ).await;

        SETUP_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(output) => {
                hub.broadcast("setup", &serde_json::json!({"status": "complete", "output": output}));
            }
            Err(e) => {
                hub.broadcast("setup", &serde_json::json!({"status": "error", "error": e.to_string()}));
            }
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

/// POST /api/setup/test-archive
pub async fn test_archive(
    State(_s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    // Run the archive test script
    let result = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(60),
        "bash",
        &["/root/bin/setup-sentryusb", "test-archive"],
    ).await;

    match result {
        Ok(output) => (StatusCode::OK, Json(serde_json::json!({"success": true, "output": output.trim()}))),
        Err(e) => (StatusCode::OK, Json(serde_json::json!({"success": false, "error": e.to_string()}))),
    }
}
