//! POST /api/system/keep-accessory — manual control for Keep-Accessory
//! Power.
//!
//! Spawns `sentryusb-ble-action`, which routes the action through the
//! telemetry daemon's warm BLE session over IPC (no competing radio
//! grab). The Pi's automation (`tesla_telemetry::keep_accessory`)
//! manages this hands-free for 12V-powered Pis; this endpoint is the
//! manual override that backs the web / SC toggle.
//!
//! Write-only protocol: the car exposes no readable "is keep-accessory
//! on?" state, so there is deliberately no GET — the UI tracks the last
//! value it set (mirrors how the Tesla app's own toggle behaves).

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use tracing::{info, warn};

use crate::router::AppState;

#[derive(Deserialize)]
pub struct KeepAccessoryRequest {
    pub on: bool,
}

/// POST /api/system/keep-accessory — body `{"on": true|false}`.
pub async fn set_keep_accessory(
    State(_s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let req: KeepAccessoryRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "invalid request body (expected {\"on\": true|false})",
            );
        }
    };
    let verb = if req.on {
        "keep-accessory-on"
    } else {
        "keep-accessory-off"
    };
    info!("[keep-accessory] manual request: {}", verb);

    // 30s covers a cold direct-BLE fallback if the telemetry daemon
    // (and its warm session) happens to be down.
    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "/root/bin/sentryusb-ble-action",
        &[verb],
    )
    .await
    {
        Ok(_) => {
            info!("[keep-accessory] {} ok", verb);
            // Fire a `keep_accessory` push so the manual override produces the
            // same user-facing signal as the automatic geofence-driven flips
            // do (see tesla_telemetry::keep_accessory::notify_event). Fan out
            // through the local Notification Center, which is gated by the
            // `keep_accessory` pref toggle and routes to every configured
            // channel including Sentry Connect. Best-effort — failures don't
            // affect the API response.
            let title = if req.on {
                "Keep Accessory ON (manual)"
            } else {
                "Keep Accessory OFF (manual)"
            };
            let message = if req.on {
                "Accessory power was turned on from the web UI / mobile app."
            } else {
                "Accessory power was turned off from the web UI / mobile app."
            };
            let body = serde_json::json!({
                "notification_type": "keep_accessory",
                "title": title,
                "message": message,
            })
            .to_string();
            tokio::spawn(async move {
                let _ = tokio::task::spawn_blocking(move || {
                    std::process::Command::new("curl")
                        .args([
                            "-s",
                            "--max-time",
                            "15",
                            "-X",
                            "POST",
                            "http://localhost/api/notifications/send",
                            "-H",
                            "Content-Type: application/json",
                            "-d",
                            &body,
                        ])
                        .output()
                })
                .await;
            });
            (
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "on": req.on })),
            )
        }
        Err(e) => {
            warn!("[keep-accessory] {} failed: {}", verb, e);
            crate::json_error(
                StatusCode::BAD_GATEWAY,
                &format!("keep-accessory command failed: {e}"),
            )
        }
    }
}

/// Default home geofence radius (meters) when unset. ~120m swallows the
/// reverse-geocode drift that makes a home occasionally read as a neighbor.
const DEFAULT_HOME_RADIUS_M: f64 = 120.0;

/// GET /api/system/keep-accessory-config → the persisted keep-accessory
/// settings the daemon reads (the 12V gate + the home geofence). Powers
/// the Settings card and the setup-wizard subsection.
pub async fn keep_accessory_config_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let config_path = sentryusb_config::find_config_path();
    let (enabled, home_lat, home_lon, home_radius_m) =
        match sentryusb_config::parse_file(config_path) {
            Ok((active, commented)) => {
                let g = |k: &str| sentryusb_config::get_config_value(&active, &commented, k);
                let enabled = matches!(
                    g("KEEP_ACCESSORY_ENABLED").as_deref(),
                    Some("yes") | Some("true") | Some("1")
                );
                let lat = g("KEEP_ACCESSORY_HOME_LAT").and_then(|s| s.trim().parse::<f64>().ok());
                let lon = g("KEEP_ACCESSORY_HOME_LON").and_then(|s| s.trim().parse::<f64>().ok());
                let radius = g("KEEP_ACCESSORY_HOME_RADIUS_M")
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .filter(|r| *r > 0.0)
                    .unwrap_or(DEFAULT_HOME_RADIUS_M);
                (enabled, lat, lon, radius)
            }
            Err(_) => (false, None, None, DEFAULT_HOME_RADIUS_M),
        };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "enabled": enabled,
            "home_lat": home_lat,
            "home_lon": home_lon,
            "home_radius_m": home_radius_m,
        })),
    )
}

#[derive(Deserialize)]
pub struct KeepAccessoryConfigBody {
    pub enabled: Option<bool>,
    pub home_lat: Option<f64>,
    pub home_lon: Option<f64>,
    pub home_radius_m: Option<f64>,
}

/// PUT /api/system/keep-accessory-config → persist the keep-accessory
/// settings to sentryusb.conf. Only the fields present in the body are
/// written. The daemon re-reads config every tick, so no restart needed.
pub async fn keep_accessory_config_set(
    State(_s): State<AppState>,
    Json(body): Json<KeepAccessoryConfigBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        if let Some(enabled) = body.enabled {
            active.insert(
                "KEEP_ACCESSORY_ENABLED".to_string(),
                if enabled { "yes" } else { "no" }.to_string(),
            );
        }
        if let Some(lat) = body.home_lat {
            active.insert("KEEP_ACCESSORY_HOME_LAT".to_string(), format!("{lat:.6}"));
        }
        if let Some(lon) = body.home_lon {
            active.insert("KEEP_ACCESSORY_HOME_LON".to_string(), format!("{lon:.6}"));
        }
        if let Some(r) = body.home_radius_m {
            // Clamp to a sane range (20m–2km) so a fat-finger can't make
            // the geofence cover the county or vanish.
            let r = r.clamp(20.0, 2000.0).round() as i64;
            active.insert("KEEP_ACCESSORY_HOME_RADIUS_M".to_string(), r.to_string());
        }
        // RO root → flip rw for the write (same pattern as ble_enabled_set).
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!("[keep-accessory] config updated");
            (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
        }
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write failed: {e}"),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config task failed: {e}"),
        ),
    }
}

/// GET /api/system/keep-accessory-gps → the car's last raw GPS fix, written
/// by the telemetry daemon's location poll. Drives the Settings/setup
/// "Use current location" button. Returns nulls until a fix is available
/// (feature must be enabled + the daemon must have polled location).
pub async fn keep_accessory_gps_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let body = std::fs::read_to_string("/mutable/keep_accessory_gps.json")
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "lat": null, "lon": null, "ts": null }));
    (StatusCode::OK, Json(body))
}
