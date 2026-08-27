pub mod auth;
pub mod ble;
pub mod degraded;
pub mod ble_debug;
pub mod router;
pub mod drives_handler;
pub mod status;
pub mod system;
pub mod files;
pub mod lock_chime;
pub mod terminal;
pub mod keep_awake;
pub mod away_mode;
pub mod travel_mode;
pub mod notifications;
pub mod notification_center;
pub mod setup;
/// Moved into `sentryusb-drives` so the drive-data archive sync can take
/// the same ownership lock as archiveloop and the backup API. Re-exported
/// here so existing call sites keep working.
pub use sentryusb_drives::archive_mount_lock;
pub mod backup;
pub mod update;
pub mod support;
pub mod community;
pub mod healthcheck;
pub mod clips;
pub mod preferences;
pub mod ttl_cache;
pub mod memory;
pub mod logs;
pub mod devices;
pub mod cloud;
pub mod snapshots;
pub mod keep_accessory;
pub mod charging;
pub mod storage_repair;
pub mod profile;
pub mod wifi_firmware;

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

/// Canonicalize longitudes to [-180, 180) for map world copies and storage.
/// Geofence calculations tolerate ±360; this keeps displayed values consistent.
pub fn normalize_lon(lon: f64) -> f64 {
    (lon + 180.0).rem_euclid(360.0) - 180.0
}

/// Process-wide shared `reqwest` client for the outbound community /
/// notification proxies.
///
/// Call sites set request-specific timeouts; the client timeout is only a
/// backstop. Sharing preserves connection pooling across requests.
pub fn http_client() -> &'static reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

#[cfg(test)]
mod tests {
    use super::normalize_lon;

    #[test]
    fn normalize_lon_wraps_world_copies() {
        assert!((normalize_lon(-221.5) - 138.5).abs() < 1e-9);
        assert!((normalize_lon(538.5) - 178.5).abs() < 1e-9);
        assert!((normalize_lon(-540.0) - (-180.0)).abs() < 1e-9);
        assert!((normalize_lon(540.0) - (-180.0)).abs() < 1e-9);
    }

    #[test]
    fn normalize_lon_half_open_range() {
        assert!((normalize_lon(180.0) - (-180.0)).abs() < 1e-9);
        assert!((normalize_lon(-180.0) - (-180.0)).abs() < 1e-9);
    }

    #[test]
    fn normalize_lon_in_range_unchanged() {
        for v in [-179.999, -98.35, 0.0, 138.6, 179.999] {
            assert!((normalize_lon(v) - v).abs() < 1e-9);
        }
    }
}
