//! System actions: reboot, toggle drives, BLE pair, speedtest, SSH, diagnostics, RTC.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use crate::router::AppState;

/// POST /api/system/reboot
pub async fn reboot(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async { let _ = sentryusb_shell::run("reboot", &[]).await; });
    crate::json_ok()
}

/// POST /api/system/toggle-drives
pub async fn toggle_drives(State(_s): State<AppState>, _body: String) -> (StatusCode, Json<serde_json::Value>) {
    let gadget_active = std::path::Path::new("/sys/kernel/config/usb_gadget/sentryusb").exists();
    if gadget_active {
        let _ = sentryusb_shell::run("bash", &["/root/bin/disable_gadget.sh"]).await;
    } else {
        let _ = sentryusb_shell::run("bash", &["/root/bin/enable_gadget.sh"]).await;
    }
    crate::json_ok()
}

/// POST /api/system/trigger-sync
pub async fn trigger_sync(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async { let _ = sentryusb_shell::run("bash", &["/root/bin/force_sync.sh"]).await; });
    crate::json_ok()
}

/// POST /api/system/ble-pair
pub async fn ble_pair(State(s): State<AppState>, _body: String) -> (StatusCode, Json<serde_json::Value>) {
    let config_path = sentryusb_config::find_config_path();
    let vin = match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => active.get("TESLA_BLE_VIN").cloned().unwrap_or_default(),
        Err(_) => String::new(),
    };

    if vin.is_empty() {
        return crate::json_error(StatusCode::BAD_REQUEST, "TESLA_BLE_VIN not configured");
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        hub.broadcast("ble_status", &serde_json::json!({"status": "pairing"}));
        let vin_upper = vin.to_uppercase();

        // Stop BLE daemon and bluetoothd for exclusive hci0 access
        let _ = sentryusb_shell::run("systemctl", &["stop", "sentryusb-ble"]).await;
        let _ = sentryusb_shell::run("systemctl", &["stop", "bluetooth"]).await;

        let result = sentryusb_shell::run_with_timeout(
            Duration::from_secs(120),
            "/root/bin/tesla-control",
            &["-ble", "-vin", &vin_upper, "add-key-request", "/root/.ble/key_public.pem", "owner", "cloud_key"],
        ).await;

        // Restart services
        let _ = sentryusb_shell::run("systemctl", &["start", "bluetooth"]).await;
        let _ = sentryusb_shell::run("systemctl", &["start", "sentryusb-ble"]).await;

        match result {
            Ok(output) => {
                hub.broadcast("ble_status", &serde_json::json!({"status": "waiting", "output": output}));
            }
            Err(e) => {
                let mut msg = e.to_string();
                if let Some(idx) = msg.find("stderr: ") {
                    msg = msg[idx + 8..].to_string();
                }
                hub.broadcast("ble_status", &serde_json::json!({"status": "error", "error": msg}));
            }
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "pairing_started"})))
}

/// GET /api/system/ble-status
pub async fn ble_status(
    State(_s): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let pub_exists = std::path::Path::new("/root/.ble/key_public.pem").exists();
    let priv_exists = std::path::Path::new("/root/.ble/key_private.pem").exists();

    if !pub_exists || !priv_exists {
        return (StatusCode::OK, Json(serde_json::json!({"status": "not_paired"})));
    }

    let config_path = sentryusb_config::find_config_path();
    let vin = match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => active.get("TESLA_BLE_VIN").cloned().unwrap_or_default(),
        Err(_) => String::new(),
    };

    if vin.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!({"status": "keys_generated"})));
    }

    // Quick check (no BLE probe)
    if params.get("quick").map(|v| v.as_str()) == Some("true") {
        if std::path::Path::new("/root/.ble/paired").exists() {
            return (StatusCode::OK, Json(serde_json::json!({"status": "paired"})));
        }
        if std::path::Path::new("/root/.ble/key_pending_pairing").exists() {
            return (StatusCode::OK, Json(serde_json::json!({"status": "keys_generated"})));
        }
        let _ = std::fs::write("/root/.ble/paired", "1");
        return (StatusCode::OK, Json(serde_json::json!({"status": "paired"})));
    }

    // Full BLE session-info probe
    let result = sentryusb_shell::run_with_timeout(
        Duration::from_secs(15),
        "/root/bin/tesla-control",
        &["-ble", "-vin", &vin.to_uppercase(), "session-info", "/root/.ble/key_private.pem", "infotainment"],
    ).await;

    if result.is_err() {
        let _ = std::fs::remove_file("/root/.ble/paired");
        return (StatusCode::OK, Json(serde_json::json!({"status": "keys_generated", "note": "Car not reachable or key not paired"})));
    }

    let _ = std::fs::write("/root/.ble/paired", "1");
    let _ = std::fs::remove_file("/root/.ble/key_pending_pairing");
    (StatusCode::OK, Json(serde_json::json!({"status": "paired"})))
}

/// GET /api/system/speedtest — stream 64MB of random data for bandwidth testing
pub async fn speedtest(State(_s): State<AppState>) -> impl IntoResponse {
    use axum::body::Body;

    let stream = tokio_stream::iter((0..1000).map(|_| {
        let mut buf = vec![0u8; 65536];
        // Fill with pseudo-random (doesn't need to be cryptographic)
        for chunk in buf.chunks_mut(8) {
            let val = rand::random::<u64>();
            let bytes = val.to_le_bytes();
            let len = chunk.len().min(8);
            chunk[..len].copy_from_slice(&bytes[..len]);
        }
        Ok::<_, std::convert::Infallible>(buf)
    }));

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        Body::from_stream(stream),
    )
}

/// GET /api/system/rtc-status
pub async fn get_rtc_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let rtc_exists = std::path::Path::new("/dev/rtc0").exists();
    let mut rtc_time = String::new();
    if rtc_exists {
        if let Ok(out) = sentryusb_shell::run("hwclock", &["-r"]).await {
            rtc_time = out.trim().to_string();
        }
    }
    (StatusCode::OK, Json(serde_json::json!({
        "available": rtc_exists,
        "time": rtc_time,
    })))
}

/// GET /api/system/ssh-pubkey
pub async fn get_ssh_pubkey(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let pub_key = std::fs::read_to_string("/root/.ssh/id_ed25519.pub")
        .or_else(|_| std::fs::read_to_string("/root/.ssh/id_rsa.pub"))
        .unwrap_or_default();
    (StatusCode::OK, Json(serde_json::json!({"public_key": pub_key.trim()})))
}

/// POST /api/system/ssh-keygen
pub async fn generate_ssh_key(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let key_path = "/root/.ssh/id_ed25519";
    let _ = std::fs::remove_file(key_path);
    let _ = std::fs::remove_file(format!("{}.pub", key_path));
    let _ = std::fs::create_dir_all("/root/.ssh");

    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(15),
        "ssh-keygen",
        &["-t", "ed25519", "-f", key_path, "-N", "", "-C", "sentryusb"],
    ).await {
        Ok(_) => {
            let pub_key = std::fs::read_to_string(format!("{}.pub", key_path)).unwrap_or_default();
            (StatusCode::OK, Json(serde_json::json!({"public_key": pub_key.trim()})))
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to generate SSH key: {}", e)),
    }
}
