//! Push notification pairing (mobile app).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::router::AppState;

const PAIRED_DEVICES_FILE: &str = "/mutable/.notification_devices.json";

#[derive(Serialize, Deserialize, Clone)]
struct PairedDevice {
    id: String,
    name: String,
    token: String,
    paired_at: String,
}

fn load_devices() -> Vec<PairedDevice> {
    std::fs::read_to_string(PAIRED_DEVICES_FILE)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

fn save_devices(devices: &[PairedDevice]) {
    let _ = std::fs::write(
        PAIRED_DEVICES_FILE,
        serde_json::to_string_pretty(devices).unwrap_or_default(),
    );
}

/// POST /api/notifications/generate-code
pub async fn generate_pairing_code(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // Generate 6-digit code
    let code: u32 = rand::random::<u32>() % 1_000_000;
    let code_str = format!("{:06}", code);

    // Store code temporarily
    let _ = std::fs::write("/tmp/sentryusb_pairing_code", &code_str);

    (StatusCode::OK, Json(serde_json::json!({"code": code_str})))
}

/// GET /api/notifications/paired-devices
pub async fn list_paired_devices(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let devices = load_devices();
    (StatusCode::OK, Json(serde_json::json!({"devices": devices})))
}

/// DELETE /api/notifications/paired-devices/{id}
pub async fn remove_paired_device(
    State(_s): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut devices = load_devices();
    let before = devices.len();
    devices.retain(|d| d.id != id);
    if devices.len() < before {
        save_devices(&devices);
        info!("[notifications] Removed paired device: {}", id);
    }
    crate::json_ok()
}

/// POST /api/notifications/test
pub async fn send_test_notification(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let config = sentryusb_notify::NotifyConfig::from_config();
    let results = sentryusb_notify::send_to_all(&config, "SentryUSB", "Test notification from SentryUSB").await;

    let attempted = results.len();
    let failures: Vec<String> = results
        .into_iter()
        .filter_map(|(name, r)| r.err().map(|e| format!("{}: {}", name, e)))
        .collect();

    if attempted == 0 {
        return crate::json_error(StatusCode::BAD_REQUEST, "No notification providers are enabled");
    }
    if !failures.is_empty() && failures.len() == attempted {
        return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &failures.join("; "));
    }
    (StatusCode::OK, Json(serde_json::json!({
        "status": "ok",
        "attempted": attempted,
        "failed": failures,
    })))
}
