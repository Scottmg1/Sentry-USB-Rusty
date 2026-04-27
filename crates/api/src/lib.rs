pub mod auth;
pub mod router;
pub mod drives_handler;
pub mod status;
pub mod system;
pub mod files;
pub mod lock_chime;
pub mod terminal;
pub mod keep_awake;
pub mod away_mode;
pub mod notifications;
pub mod notification_center;
pub mod setup;
pub mod backup;
pub mod update;
pub mod support;
pub mod community;
pub mod healthcheck;
pub mod clips;
pub mod preferences;
pub mod memory;
pub mod logs;
pub mod devices;
pub mod cloud;

pub use auth::{AuthState, init_auth};
pub use router::build_router;

use axum::Json;
use axum::http::StatusCode;
use serde::Serialize;

/// Standard JSON response helper.
pub fn json_response<T: Serialize>(status: StatusCode, data: T) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::to_value(data).unwrap_or_default()))
}

/// Standard error response.
pub fn json_error(status: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (status, Json(serde_json::json!({"error": msg})))
}

/// Standard success response.
pub fn json_ok() -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(serde_json::json!({"success": true})))
}
