//! Block device listing.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;

/// GET /api/system/block-devices
pub async fn list_block_devices(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match sentryusb_shell::run("lsblk", &["-J", "-o", "NAME,SIZE,TYPE,MOUNTPOINT,FSTYPE"]).await {
        Ok(output) => {
            match serde_json::from_str::<serde_json::Value>(&output) {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(_) => (StatusCode::OK, Json(serde_json::json!({"blockdevices": []}))),
            }
        }
        Err(_) => (StatusCode::OK, Json(serde_json::json!({"blockdevices": []}))),
    }
}
