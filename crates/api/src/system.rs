//! System actions: reboot, toggle drives, BLE pair, speedtest, SSH, diagnostics, RTC.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use sentryusb_ble_health::{
    DEFAULT_HEALTH_PATH, FaultKind, clear_all_at, read_at, record_fault_at,
};
use crate::router::AppState;

/// POST /api/system/reboot
pub async fn reboot() -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async { let _ = sentryusb_shell::run("reboot", &[]).await; });
    crate::json_ok()
}

/// Starts shutdown asynchronously, trying commands supported by different images.
pub async fn shutdown() -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async {
        if sentryusb_shell::run("poweroff", &[]).await.is_ok() {
            return;
        }
        if sentryusb_shell::run("shutdown", &["-h", "now"]).await.is_ok() {
            return;
        }
        let _ = sentryusb_shell::run("systemctl", &["poweroff"]).await;
    });
    crate::json_ok()
}

/// POST /api/system/toggle-drives
pub async fn toggle_drives(State(_s): State<AppState>, _body: String) -> (StatusCode, Json<serde_json::Value>) {
    // Serialize user toggles with archive and watchdog gadget cycles.
    let result = tokio::task::spawn_blocking(|| -> Result<(), String> {
        let _cycle = sentryusb_gadget::cycle_lock::acquire(Duration::from_secs(30))
            .map_err(|e| format!("USB drives are busy ({}) — try again shortly", e))?;
        // The previous lock holder may have changed state.
        let was_active = sentryusb_gadget::is_active();
        let r = if was_active {
            sentryusb_gadget::disable()
        } else {
            sentryusb_gadget::enable()
        };
        r.map_err(|e| {
            format!("USB gadget {} failed: {}", if was_active { "disable" } else { "enable" }, e)
        })
    })
    .await;
    match result {
        Ok(Ok(())) => crate::json_ok(),
        Ok(Err(msg)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &msg),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget task panicked: {}", e),
        ),
    }
}

/// Idempotently enables the gadget without reacquiring archiveloop's cycle lock.
pub async fn gadget_enable(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if sentryusb_gadget::is_active() {
        return crate::json_ok();
    }
    match tokio::task::spawn_blocking(sentryusb_gadget::enable).await {
        Ok(Ok(())) => crate::json_ok(),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget enable failed: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget task panicked: {}", e),
        ),
    }
}

/// POST /api/system/gadget-disable — idempotent set-to-inactive.
pub async fn gadget_disable(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if !sentryusb_gadget::is_active() {
        return crate::json_ok();
    }
    match tokio::task::spawn_blocking(sentryusb_gadget::disable).await {
        Ok(Ok(())) => crate::json_ok(),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget disable failed: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("USB gadget task panicked: {}", e),
        ),
    }
}

/// Forces archiveloop through either wait state into an archive cycle.
/// The unreachable canary advances idle state; the reachable canary starts sync.
pub async fn trigger_sync(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async {
        let unreachable = std::path::Path::new("/tmp/archive_is_unreachable");
        let reachable = std::path::Path::new("/tmp/archive_is_reachable");
        // Advance a loop waiting for the archive to become unreachable.
        let _ = std::fs::File::create(unreachable);
        // Remove an unconsumed canary so it cannot trigger a later cycle.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !unreachable.exists() {
                break;
            }
        }
        let _ = std::fs::remove_file(unreachable);
        // Start sync even when the ordinary reachability probe is failing.
        let _ = std::fs::File::create(reachable);
    });
    crate::json_ok()
}

/// POST /api/system/ble-pair
pub async fn ble_pair(State(s): State<AppState>, _body: String) -> (StatusCode, Json<serde_json::Value>) {
    // BLE enablement is the security boundary for vehicle key operations.
    if !crate::ble::is_ble_enabled() {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "BLE is disabled in settings — enable it before pairing",
        );
    }

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

        // Prefer the telemetry daemon's existing vehicle connection to avoid
        // consuming another BLE slot; the action binary can connect directly.
        // Successful WithResponse writes confirm delivery of the pairing request.
        let result = sentryusb_shell::run_with_timeout(
            Duration::from_secs(120),
            "/root/bin/sentryusb-ble-action",
            &["pair"],
        ).await;

        match result {
            Ok(output) => {
                hub.broadcast(
                    "ble_status",
                    &serde_json::json!({"status": "waiting", "output": output.trim()}),
                );
            }
            Err(e) => {
                // Translate slot exhaustion into actionable guidance.
                let detail = e.to_string();
                let lower = detail.to_lowercase();
                let msg = if lower.contains("maximum number of ble")
                    || lower.contains("slot likely held")
                    || lower.contains("connect timed out")
                    || lower.contains("could not reach vehicle")
                {
                    "Couldn't reach the car to send the pairing request — its Bluetooth \
                     connection slots are likely full. Turn off Bluetooth on any phones near \
                     the car (and move other paired phones away), make sure the car is awake \
                     and within a few metres, then retry pairing."
                        .to_string()
                } else {
                    detail
                };
                hub.broadcast(
                    "ble_status",
                    &serde_json::json!({"status": "error", "error": msg}),
                );
            }
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "pairing_started"})))
}

/// Remounts the protected root filesystem before persistent writes.
fn remount_root_rw() {
    if let Err(e) = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status()
    {
        tracing::warn!("remountfs_rw failed to run: {e}");
    }
}

/// Persists pairing state on the read-only-root image after remounting.
fn set_ble_paired_marker(paired: bool) {
    remount_root_rw();
    // Missing markers are already unpaired.
    let ignore_missing = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    if paired {
        if let Err(e) = std::fs::write("/root/.ble/paired", "1") {
            tracing::warn!("failed to write /root/.ble/paired: {e}");
        }
        if let Err(e) = std::fs::remove_file("/root/.ble/key_pending_pairing") {
            if !ignore_missing(&e) {
                tracing::warn!("failed to clear /root/.ble/key_pending_pairing: {e}");
            }
        }
    } else if let Err(e) = std::fs::remove_file("/root/.ble/paired") {
        if !ignore_missing(&e) {
            tracing::warn!("failed to remove /root/.ble/paired: {e}");
        }
    }
}

/// Clears phone-side bonds and claim state without disrupting the vehicle link.
/// The app generates and pushes a fresh PIN during the subsequent re-claim.
pub async fn ble_reset_pair(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // Preserve vehicle and other keyless cache entries.
    let removed = remove_phone_bonds().await;

    // Clear the app PIN so a fresh claim can succeed.
    remount_root_rw();
    let mut pin_cleared = false;
    for p in ["/root/.sentryusb/ble-pin", "/boot/firmware/BLE_PIN"] {
        match std::fs::remove_file(p) {
            Ok(()) => pin_cleared = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("ble-reset: failed to remove {p}: {e}"),
        }
    }

    // Restart only the phone-facing GATT server.
    let restarted = sentryusb_shell::run_with_timeout(
        Duration::from_secs(20),
        "systemctl",
        &["restart", "sentryusb-ble.service"],
    )
    .await
    .is_ok();

    (StatusCode::OK, Json(serde_json::json!({
        "status": "reset",
        "removed_bonds": removed,
        "pin_cleared": pin_cleared,
        "ble_service_restarted": restarted,
    })))
}

/// Removes bonded phone clients while preserving the keyless vehicle peer.
async fn remove_phone_bonds() -> Vec<String> {
    let mut removed = Vec::new();
    let adapters = match std::fs::read_dir("/var/lib/bluetooth") {
        Ok(d) => d,
        Err(_) => return removed,
    };
    for adapter in adapters.flatten() {
        let apath = adapter.path();
        if !apath.is_dir() {
            continue;
        }
        let peers = match std::fs::read_dir(&apath) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for peer in peers.flatten() {
            let ppath = peer.path();
            let mac = peer.file_name().to_string_lossy().to_string();
            if !is_mac_dir(&mac) {
                continue;
            }
            let info = std::fs::read_to_string(ppath.join("info")).unwrap_or_default();
            if is_tesla_peer(&info) || !has_bond_key(&info) {
                continue;
            }
            let _ = sentryusb_shell::run_with_timeout(
                Duration::from_secs(10),
                "bluetoothctl",
                &["remove", &mac],
            )
            .await;
            // Remove on-disk state if BlueZ did not know the bond.
            let _ = std::fs::remove_dir_all(&ppath);
            removed.push(mac);
        }
    }
    removed
}

/// `XX:XX:XX:XX:XX:XX` — a BlueZ peer directory name.
fn is_mac_dir(s: &str) -> bool {
    let parts: Vec<&str> = s.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The peer's advertised `Name=` from its BlueZ `info` file, if present.
fn info_name(info: &str) -> Option<&str> {
    info.lines().find_map(|l| l.strip_prefix("Name=").map(str::trim))
}

/// Recognizes the vehicle's `S<hex>C` advertised-name format.
fn is_tesla_peer(info: &str) -> bool {
    match info_name(info) {
        Some(name) => {
            let b = name.as_bytes();
            b.len() >= 10
                && b[0] == b'S'
                && b[b.len() - 1] == b'C'
                && b[1..b.len() - 1].iter().all(u8::is_ascii_hexdigit)
        }
        None => false,
    }
}

/// Returns true when a BlueZ peer has persisted bond material.
fn has_bond_key(info: &str) -> bool {
    info.contains("[LinkKey]")
        || info.contains("[LongTermKey]")
        || info.contains("[PeripheralLongTermKey]")
        || info.contains("[SlaveLongTermKey]")
}

/// GET /api/system/ble-status
pub async fn ble_status(
    State(_s): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let pub_exists = std::path::Path::new("/root/.ble/key_public.pem").exists();
    let priv_exists = std::path::Path::new("/root/.ble/key_private.pem").exists();

    // Every response includes the VIN for the pairing form.
    let config_path = sentryusb_config::find_config_path();
    let vin = match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => active.get("TESLA_BLE_VIN").cloned().unwrap_or_default(),
        Err(_) => String::new(),
    };
    // Installation requires the native action binary and generated private key.
    let binaries_installed = std::path::Path::new("/root/bin/sentryusb-ble-action").exists()
        && priv_exists;

    if !pub_exists || !priv_exists {
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "not_paired",
            "vin": vin,
            "binaries_installed": binaries_installed,
        })));
    }

    if vin.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "keys_generated",
            "vin": "",
            "binaries_installed": binaries_installed,
        })));
    }

    let health_path = std::path::Path::new(DEFAULT_HEALTH_PATH);
    let repair_required = match read_at(health_path) {
        Ok(record) => record.is_some_and(|record| record.fault == FaultKind::RepairRequired),
        Err(e) => {
            tracing::warn!("could not read BLE health record for pairing status: {e}");
            false
        }
    };

    // Avoid a vehicle probe for lightweight status polling.
    if params.get("quick").map(|v| v.as_str()) == Some("true") {
        if repair_required {
            return (StatusCode::OK, Json(serde_json::json!({
                "status": "repair_required",
                "vin": vin,
                "binaries_installed": binaries_installed,
                "note": "Re-pair from the BLE card and tap your key card on the center console",
            })));
        }
        if std::path::Path::new("/root/.ble/paired").exists() {
            return (StatusCode::OK, Json(serde_json::json!({
                "status": "paired",
                "vin": vin,
                "binaries_installed": binaries_installed,
            })));
        }
        if std::path::Path::new("/root/.ble/key_pending_pairing").exists() {
            return (StatusCode::OK, Json(serde_json::json!({
                "status": "keys_generated",
                "vin": vin,
                "binaries_installed": binaries_installed,
            })));
        }
        set_ble_paired_marker(true);
        return (StatusCode::OK, Json(serde_json::json!({
            "status": "paired",
            "vin": vin,
            "binaries_installed": binaries_installed,
        })));
    }

    // The action binary reuses daemon IPC when available and emits one status token.
    let probe = sentryusb_shell::run_with_timeout(
        Duration::from_secs(20),
        "/root/bin/sentryusb-ble-action",
        &["session-info"],
    )
    .await;
    let token = probe.as_deref().map(str::trim).unwrap_or("");

    match token {
        "PAIRED" => {
            // Refresh the live BLE-success timestamp.
            crate::ble::mark_ble_success();
            if let Err(e) = clear_all_at(health_path) {
                tracing::warn!("could not clear BLE repair-required health after pairing: {e}");
            }
            set_ble_paired_marker(true);
            (StatusCode::OK, Json(serde_json::json!({
                "status": "paired",
                "vin": vin,
                "binaries_installed": binaries_installed,
            })))
        }
        "NOT_PAIRED" => {
            // Clear pairing only after an explicit vehicle rejection.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0);
            if let Err(e) = record_fault_at(health_path, FaultKind::RepairRequired, now) {
                tracing::warn!("could not persist BLE repair-required health: {e}");
            }
            set_ble_paired_marker(false);
            (StatusCode::OK, Json(serde_json::json!({
                "status": "keys_generated",
                "vin": vin,
                "binaries_installed": binaries_installed,
                "note": "Car reports this key is not paired -- re-pair from the BLE card and tap your card on the console",
            })))
        }
        _ => {
            // Transport or configuration failure cannot disprove pairing.
            if let Err(e) = &probe {
                tracing::warn!(
                    "ble session-info probe could not run ({e:#}); leaving paired marker unchanged"
                );
            }
            let paired = std::path::Path::new("/root/.ble/paired").exists();
            (StatusCode::OK, Json(serde_json::json!({
                "status": if paired { "paired" } else { "keys_generated" },
                "vin": vin,
                "binaries_installed": binaries_installed,
                "note": "Could not verify pairing right now (car asleep or BLE radio busy); status unchanged",
            })))
        }
    }
}

/// Streams 64 MB using one process-cached random 64 KiB chunk.
static SPEEDTEST_CHUNK: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

fn speedtest_chunk() -> &'static Vec<u8> {
    SPEEDTEST_CHUNK.get_or_init(|| {
        let mut buf = vec![0u8; 65536];
        for chunk in buf.chunks_mut(8) {
            let val = rand::random::<u64>();
            let bytes = val.to_le_bytes();
            let len = chunk.len().min(8);
            chunk[..len].copy_from_slice(&bytes[..len]);
        }
        buf
    })
}

pub async fn speedtest(State(_s): State<AppState>) -> impl IntoResponse {
    use axum::body::Body;

    let chunk = speedtest_chunk();
    let stream = tokio_stream::iter(
        (0..1000).map(move |_| Ok::<_, std::convert::Infallible>(chunk.clone()))
    );

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
pub async fn get_rtc_status(State(_s): State<AppState>) -> impl IntoResponse {
    let rtc_exists = std::path::Path::new("/dev/rtc0").exists();
    let mut rtc_time = String::new();
    if rtc_exists {
        if let Ok(out) = sentryusb_shell::run("hwclock", &["-r"]).await {
            rtc_time = out.trim().to_string();
        }
    }
    // RTC presence is stable at runtime, so cache it for five minutes.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=300")],
        Json(serde_json::json!({
            "available": rtc_exists,
            "time": rtc_time,
        })),
    )
}

/// Reports whether the clock is credible from NTP, RTC, or a recent epoch.
pub async fn get_clock_status(
    State(_s): State<AppState>,
) -> impl IntoResponse {
    let ntp_synced =
        std::path::Path::new("/run/systemd/timesync/synchronized").exists();
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 2025-01-01 00:00:00 UTC = 1735689600.
    let year_looks_recent = secs > 1_735_689_600;
    let synced = ntp_synced || year_looks_recent;
    let has_rtc = std::path::Path::new("/dev/rtc0").exists();

    // A short cache preserves visible NTP transitions while reducing polling.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=10")],
        Json(serde_json::json!({
            "synced": synced,
            "has_rtc": has_rtc,
            "ntp_synced": ntp_synced,
            // RTC users do not need the unsynchronized-clock warning.
            "show_warning": !synced && !has_rtc,
        })),
    )
}

/// GET /api/system/ssh-pubkey
pub async fn get_ssh_pubkey(State(_s): State<AppState>) -> impl IntoResponse {
    let pub_key = std::fs::read_to_string("/root/.ssh/id_ed25519.pub")
        .or_else(|_| std::fs::read_to_string("/root/.ssh/id_rsa.pub"))
        .unwrap_or_default();
    // Public keys change only through explicit regeneration.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=3600")],
        Json(serde_json::json!({"public_key": pub_key.trim()})),
    )
}

/// POST /api/system/ssh-keygen
pub async fn generate_ssh_key(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // Remount protected images; retain a fallback for development images.
    let _ = sentryusb_shell::run(
        "bash",
        &["-c", "/root/bin/remountfs_rw 2>/dev/null || mount -o remount,rw / 2>/dev/null || true"],
    )
    .await;

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
