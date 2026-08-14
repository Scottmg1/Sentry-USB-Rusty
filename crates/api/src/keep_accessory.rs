//! Manual Keep-Accessory Power control.
//!
//! `sentryusb-ble-action` routes commands through the telemetry daemon's warm
//! BLE session instead of competing for the radio.
//!
//! The vehicle exposes no readable state, so this protocol is write-only.

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

    // Allow time for direct-BLE fallback when the warm session is unavailable.
    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "/root/bin/sentryusb-ble-action",
        &[verb],
    )
    .await
    {
        Ok(_) => {
            info!("[keep-accessory] {} ok", verb);
            // Send the same preference-gated notification as automatic changes.
            // Notification failures do not affect the control response.
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

/// Default home geofence radius in meters, allowing for GPS drift.
const DEFAULT_HOME_RADIUS_M: f64 = 120.0;

/// GET /api/system/keep-accessory-config
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
                // Normalize longitudes produced by map world copies.
                let lon = g("KEEP_ACCESSORY_HOME_LON")
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .map(crate::normalize_lon);
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
    /// Preserve sessions leaving the geofence under this stored tag before the
    /// derived Home set and its rate change. Ignored if the geofence is unchanged.
    pub freeze_home_as: Option<String>,
}

/// PUT /api/system/keep-accessory-config
/// Persist supplied fields; the daemon rereads configuration each tick.
pub async fn keep_accessory_config_set(
    State(_s): State<AppState>,
    Json(body): Json<KeepAccessoryConfigBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Center and radius changes both redefine the derived Home set.
    let freeze_as = body.freeze_home_as.clone().filter(|t| !t.trim().is_empty());
    let home_moving =
        body.home_lat.is_some() || body.home_lon.is_some() || body.home_radius_m.is_some();

    // Freeze and persist the same normalized geofence values.
    let new_lat = body.home_lat.filter(|v| v.is_finite()).map(|v| v.clamp(-90.0, 90.0));
    // Map world copies can produce longitudes outside the canonical range.
    let new_lon = body.home_lon.filter(|v| v.is_finite()).map(crate::normalize_lon);
    // Keep the radius within 20 m–2 km.
    let new_radius = body.home_radius_m.map(|r| r.clamp(20.0, 2000.0).round());
    let enabled = body.enabled;

    let mut frozen = 0usize;
    if home_moving {
        if let Some(tag) = freeze_as {
            let store = _s.drives.store.clone();
            match tokio::task::spawn_blocking(move || {
                let new_home =
                    crate::charging::pending_home_geofence(new_lat, new_lon, new_radius);
                crate::charging::freeze_home_sessions(&store, &tag, new_home)
            })
            .await
            {
                Ok(Ok(f)) => {
                    frozen = f.tagged;
                    // The freeze marks copied rates dirty; wake the cloud sweep.
                    if f.rates_dirty {
                        _s.cloud.uploader.nudge();
                    }
                }
                // Preserve history atomically with the requested move.
                Ok(Err(e)) => {
                    return crate::json_error(
                        StatusCode::BAD_REQUEST,
                        &format!("home not moved — could not preserve past charges: {e}"),
                    )
                }
                Err(e) => {
                    return crate::json_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("home not moved — freeze task failed: {e}"),
                    )
                }
            }
        }
    }

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        if let Some(enabled) = enabled {
            active.insert(
                "KEEP_ACCESSORY_ENABLED".to_string(),
                if enabled { "yes" } else { "no" }.to_string(),
            );
        }
        if let Some(lat) = new_lat {
            active.insert("KEEP_ACCESSORY_HOME_LAT".to_string(), format!("{lat:.6}"));
        }
        if let Some(lon) = new_lon {
            active.insert("KEEP_ACCESSORY_HOME_LON".to_string(), format!("{lon:.6}"));
        }
        if let Some(r) = new_radius {
            active.insert(
                "KEEP_ACCESSORY_HOME_RADIUS_M".to_string(),
                (r as i64).to_string(),
            );
        }
        // The root filesystem is normally read-only.
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
            if home_moving {
                // Charging summaries contain geofence-derived Home tags.
                crate::charging::invalidate_charging_list();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({ "ok": true, "frozen": frozen })),
            )
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

/// GPS fix cache shared with Away Mode, respecting the mutable-dir override.
pub(crate) fn gps_file() -> String {
    format!("{}/keep_accessory_gps.json", sentryusb_config::mutable_dir())
}

/// GET /api/system/keep-accessory-gps
/// Return the last raw vehicle GPS fix, or null fields until one is available.
pub async fn keep_accessory_gps_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let body = std::fs::read_to_string(gps_file())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .unwrap_or_else(|| serde_json::json!({ "lat": null, "lon": null, "ts": null }));
    (StatusCode::OK, Json(body))
}
