//! Away Mode: WiFi AP control with timed expiration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use tracing::info;

use crate::router::AppState;

const FLAG_FILE: &str = "/mutable/sentryusb_away_mode.json";

static AWAY_MODE_ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Deserialize)]
struct EnableRequest {
    duration_min: Option<u64>,
}

/// POST /api/away-mode/enable
pub async fn enable(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    let req: EnableRequest = serde_json::from_str(&body).unwrap_or(EnableRequest { duration_min: Some(60) });
    let duration_min = req.duration_min.unwrap_or(60);
    let duration = Duration::from_secs(duration_min * 60);

    AWAY_MODE_ACTIVE.store(true, Ordering::SeqCst);

    // Write flag file
    let now = chrono::Utc::now();
    let expires = now + chrono::Duration::seconds(duration.as_secs() as i64);
    let flag = serde_json::json!({
        "expires_at": expires.to_rfc3339(),
        "enabled_at": now.to_rfc3339(),
        "remaining_sec": duration.as_secs(),
    });
    let _ = std::fs::write(FLAG_FILE, serde_json::to_string_pretty(&flag).unwrap_or_default());

    info!("[away-mode] Enabled (duration: {}m)", duration_min);

    // Start AP in background
    tokio::spawn(async move {
        let _ = sentryusb_shell::run("nmcli", &["connection", "up", "SentryUSB-AP"]).await;

        // Wait for expiration
        tokio::time::sleep(duration).await;

        // Auto-disable
        AWAY_MODE_ACTIVE.store(false, Ordering::SeqCst);
        let _ = std::fs::remove_file(FLAG_FILE);
        let _ = sentryusb_shell::run("nmcli", &["connection", "down", "SentryUSB-AP"]).await;
        info!("[away-mode] Expired, disabled AP");
    });

    crate::json_ok()
}

/// POST /api/away-mode/disable
pub async fn disable(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    AWAY_MODE_ACTIVE.store(false, Ordering::SeqCst);
    let _ = std::fs::remove_file(FLAG_FILE);

    tokio::spawn(async {
        let _ = sentryusb_shell::run("nmcli", &["connection", "down", "SentryUSB-AP"]).await;
    });

    info!("[away-mode] Disabled by user");
    crate::json_ok()
}

/// GET /api/away-mode/status
pub async fn status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let active = AWAY_MODE_ACTIVE.load(Ordering::Relaxed);

    if active {
        if let Ok(data) = std::fs::read_to_string(FLAG_FILE) {
            if let Ok(flag) = serde_json::from_str::<serde_json::Value>(&data) {
                return (StatusCode::OK, Json(serde_json::json!({
                    "enabled": true,
                    "expires_at": flag.get("expires_at"),
                    "enabled_at": flag.get("enabled_at"),
                    "remaining_sec": flag.get("remaining_sec"),
                })));
            }
        }
    }

    (StatusCode::OK, Json(serde_json::json!({"enabled": false})))
}
