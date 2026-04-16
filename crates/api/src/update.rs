//! OTA update: check for updates, run update, version info.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;
use crate::status::get_sbc_model;

static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

/// GET /api/system/check-internet
pub async fn check_internet(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let connected = sentryusb_shell::run("ping", &["-c", "1", "-W", "3", "8.8.8.8"]).await.is_ok();
    (StatusCode::OK, Json(serde_json::json!({"connected": connected})))
}

/// POST /api/system/update
pub async fn run_update(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if UPDATE_RUNNING.swap(true, Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Update already in progress");
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        hub.broadcast("update", &serde_json::json!({"status": "running"}));

        let result = sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(600),
            "bash",
            &["/root/bin/setup-sentryusb", "selfupdate"],
        ).await;

        UPDATE_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(output) => hub.broadcast("update", &serde_json::json!({"status": "complete", "output": output})),
            Err(e) => hub.broadcast("update", &serde_json::json!({"status": "error", "error": e.to_string()})),
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

/// GET /api/system/version
pub async fn get_version(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let version = env!("CARGO_PKG_VERSION");
    let sbc_model = get_sbc_model();

    // Read installed version tag if available
    let installed = std::fs::read_to_string("/root/.sentryusb_version")
        .unwrap_or_else(|_| version.to_string());

    (StatusCode::OK, Json(serde_json::json!({
        "version": installed.trim(),
        "binary_version": version,
        "sbc_model": sbc_model,
    })))
}

/// POST /api/system/check-update
pub async fn check_for_update(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // Check GitHub releases for newer version
    match sentryusb_shell::run("bash", &["-c", "curl -s https://api.github.com/repos/Scottmg1/Sentry-USB/releases/latest | grep -o '\"tag_name\": *\"[^\"]*\"' | head -1"]).await {
        Ok(output) => {
            let latest = output.trim()
                .trim_start_matches("\"tag_name\":")
                .trim()
                .trim_matches('"');
            let current = std::fs::read_to_string("/root/.sentryusb_version")
                .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());
            let available = !latest.is_empty() && latest != current.trim();
            (StatusCode::OK, Json(serde_json::json!({
                "available": available,
                "latest": latest,
                "current": current.trim(),
            })))
        }
        Err(_) => (StatusCode::OK, Json(serde_json::json!({"available": false, "error": "could not check"}))),
    }
}

/// GET /api/system/update-status
pub async fn get_update_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let running = UPDATE_RUNNING.load(Ordering::Relaxed);
    (StatusCode::OK, Json(serde_json::json!({
        "status": if running { "running" } else { "idle" },
    })))
}
