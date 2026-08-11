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

/// POST /api/system/shutdown
///
/// Power off the device. Spawned so the HTTP response can flush before
/// the kernel starts tearing things down. Falls back through `poweroff`
/// → `shutdown -h now` → `systemctl poweroff` since some minimal images
/// only ship one of the three.
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
    // A user toggle owns a full gadget cycle, so it takes the cross-process
    // flock archiveloop wraps around its own cycles — otherwise it can race
    // an archive sync or a stall-watchdog recovery and flip the gadget while
    // cam_disk.bin is mounted on the host. gadget_enable/gadget_disable
    // below stay lockless: archiveloop's shims call them while it already
    // holds this flock.
    let result = tokio::task::spawn_blocking(|| -> Result<(), String> {
        let _cycle = sentryusb_gadget::cycle_lock::acquire(Duration::from_secs(30))
            .map_err(|e| format!("USB drives are busy ({}) — try again shortly", e))?;
        // Re-read under the lock: whoever held it may have flipped the state.
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

/// POST /api/system/gadget-enable — idempotent set-to-active.
///
/// Called from the `/root/bin/enable_gadget.sh` shim so archiveloop coordinates
/// with this server instead of driving configfs directly in parallel.
///
/// This handler (and gadget_disable below) must NOT take the gadget-cycle
/// flock: archiveloop already holds it when the shim runs, so locking here
/// would wedge the shim's curl until its --max-time kills the request.
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

/// POST /api/system/trigger-sync
///
/// Force archiveloop to start a sync cycle now, regardless of the
/// connectivity check's current opinion. archiveloop has two distinct
/// wait states the loop can be sitting in when the user clicks "Start
/// Archive":
///
///   1. `wait_for_archive_to_be_reachable` — usual case after a fresh
///      boot or after the car drove away from the home WiFi. Loop
///      polls archive-is-reachable.sh until it succeeds. Consumes
///      `/tmp/archive_is_reachable` to fake a positive result and
///      proceed to the archive step.
///
///   2. `wait_for_archive_to_be_unreachable` — idle steady state after
///      archive completed; loop is waiting for the car to drive away
///      so the next archive cycle can start fresh. Consumes
///      `/tmp/archive_is_unreachable` to fake "user drove away" and
///      proceed back to step 1.
///
/// The Go-era `force_sync.sh` only created the unreachable canary,
/// which is correct for state (2) but a no-op for state (1) — the
/// exact case a user hits when their NAS is briefly down or the
/// reachability check is misconfigured. Create the unreachable canary
/// first (covering state 2), wait a moment for archiveloop to
/// consume it, then create the reachable canary (covering both: state
/// 1 directly, or state 2 after archiveloop transitions out via the
/// first canary). Either way the loop kicks off an archive cycle.
///
/// Travel Mode has a third idle state: the paced sleep between cycles
/// (travel_mode_pace). It watches for and consumes the reachable
/// canary too, cutting the sleep short so "Start Archive" works on
/// the road as well.
pub async fn trigger_sync(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async {
        let unreachable = std::path::Path::new("/tmp/archive_is_unreachable");
        let reachable = std::path::Path::new("/tmp/archive_is_reachable");
        // Step 1: kick a loop sitting in wait_for_unreachable.
        let _ = std::fs::File::create(unreachable);
        // Wait up to ~5s for archiveloop to consume it. If it doesn't,
        // the loop is already past that state (in wait_for_reachable),
        // and a stale canary left lying around would otherwise fire on
        // the next idle cycle and cause a phantom force-sync the user
        // didn't ask for. Clean up either way.
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if !unreachable.exists() {
                break;
            }
        }
        let _ = std::fs::remove_file(unreachable);
        // Step 2: kick a loop sitting in wait_for_reachable. archiveloop
        // consumes this and starts an archive cycle even if the real
        // network probe is currently failing — exactly what a user
        // means when they click "Start Archive Now".
        let _ = std::fs::File::create(reachable);
    });
    crate::json_ok()
}

/// POST /api/system/ble-pair
pub async fn ble_pair(State(s): State<AppState>, _body: String) -> (StatusCode, Json<serde_json::Value>) {
    // Master kill-switch: when the user has flipped Tesla BLE off in
    // settings, refuse pairing even if a VIN is configured. The
    // setting is the security boundary that protects the car from a
    // Pi-as-proximity-key scenario.
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

        // Send the add-key-to-whitelist request natively via
        // sentryusb-ble-action — no tesla-control, no adapter switcheroo
        // (no stopping bluetooth/telemetry/ble, no bluetoothctl
        // power-cycle).
        //
        // ble-action routes the request through the telemetry daemon's
        // IPC socket first, so it goes out over the connection the daemon
        // already holds — *reusing* the car's BLE slot instead of opening
        // a new one. The old flow stopped the daemon and power-cycled the
        // controller, then tesla-control opened a *fresh* connection —
        // which is exactly what tripped the car's "maximum number of BLE
        // devices" limit, so the request never landed and the console
        // never showed the card prompt. Keeping the daemon connected
        // means no slot churn, no service downtime, and telemetry never
        // pauses. (If the daemon is down — BLE telemetry disabled —
        // ble-action falls back to its own one-shot direct-BLE session.)
        //
        // On success ble-action prints "OK" and exits 0: the request
        // reached the car and it is now prompting for the NFC-card tap.
        // The frontend's subsequent ble-status polls confirm enrolment
        // once the user taps. The per-chunk WithResponse ATT ACK on the
        // write is a real wire-level delivery confirmation, so there's no
        // "chip lied about success" case to separately verify. 120s
        // budget covers the cold scan + connect of the direct fallback.
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
                // ble-action exits non-zero on a BLE/slot failure (car's
                // connection table full, or asleep/out of range) or a
                // config/key error. Detect the slot-exhaustion case — the
                // single most common pairing failure — and give
                // actionable guidance instead of a raw transport error.
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

/// Remount the root filesystem read-write. These images keep `/`
/// read-only to protect the SD card; a plain write to `/root` silently
/// no-ops until this runs. Mirrors the remount the keygen / config /
/// VIN-set paths in `ble.rs` already do before their writes.
fn remount_root_rw() {
    if let Err(e) = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status()
    {
        tracing::warn!("remountfs_rw failed to run: {e}");
    }
}

/// Persist the BLE pairing marker so the pair card survives a reload.
///
/// `paired=true` writes `/root/.ble/paired` and clears the
/// `key_pending_pairing` flag; `paired=false` removes the marker.
/// Always remounts rw first — without it the write no-ops on the
/// read-only-root images and the card reverts to "Pair" on every
/// navigation (the bug this fixes). Logs on failure instead of
/// swallowing it, so a future regression is visible in the journal.
fn set_ble_paired_marker(paired: bool) {
    remount_root_rw();
    // ENOENT on a remove just means the file wasn't there — expected,
    // not worth a warning.
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

/// POST /api/system/ble-reset-pair
///
/// Recovery for a wedged phone↔Pi BLE pairing — the "Pairing rejected by
/// SentryUSB-XXXX" dead end (#324) where the phone has no Bluetooth-settings
/// entry to forget and the only prior fix was SSH. Clears ONLY phone-side
/// state so a fresh claim can succeed:
///   - removes each phone GATT-client bond from BlueZ (`bluetoothctl remove`)
///   - deletes the app PIN (`/root/.sentryusb/ble-pin` + boot copy) → unclaimed
///   - restarts ONLY `sentryusb-ble.service` (the phone-facing GATT server)
///
/// It NEVER touches the car or the sampler: the Tesla's BlueZ entry
/// (advertised name `S<hex>C`, stored keyless — the vehicle uses app-layer
/// crypto, not an LE bond) is preserved, and neither `bluetooth.service` nor
/// the telemetry sampler is restarted, so keep-awake / archiving keep running.
/// The app generates and pushes a fresh PIN during the subsequent re-claim.
pub async fn ble_reset_pair(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // 1) Remove phone bonds, preserving the Tesla peer + keyless entries.
    let removed = remove_phone_bonds().await;

    // 2) Clear the app PIN so the device returns to the unclaimed state and
    //    accepts a fresh claim from the app. Root is ro at runtime.
    remount_root_rw();
    let mut pin_cleared = false;
    for p in ["/root/.sentryusb/ble-pin", "/boot/firmware/BLE_PIN"] {
        match std::fs::remove_file(p) {
            Ok(()) => pin_cleared = true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("ble-reset: failed to remove {p}: {e}"),
        }
    }

    // 3) Restart ONLY the phone-facing GATT server. Never the sampler or
    //    bluetooth.service — the Tesla session must stay up.
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

/// Remove every BlueZ phone-client bond, preserving the Tesla peer.
///
/// The phone bonds carry an LTK/LinkKey that goes stale after a Pi rebuild
/// or a phone reset — the desync behind #324. The Tesla advertises as
/// `S<hex>C` and is stored keyless (vehicle BLE doesn't LE-bond), so we skip
/// any peer whose name matches that shape OR that carries no bond key.
/// `bluetoothctl remove` drops the bond from the live daemon and deletes the
/// on-disk dir without restarting bluetoothd, so the car link is untouched.
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
                continue; // preserve the car + keyless cache entries
            }
            let _ = sentryusb_shell::run_with_timeout(
                Duration::from_secs(10),
                "bluetoothctl",
                &["remove", &mac],
            )
            .await;
            // If the daemon didn't know the bond, the dir survives the
            // `remove` — delete it directly so a stale LTK can't linger.
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

/// True when the peer is a Tesla — advertised name `S<hex>C` (e.g.
/// `Se04d38788e92e221C`). Used to protect the car's BlueZ entry from the
/// phone-bond cleanup.
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

/// True when the peer's `info` carries an actual pairing key. Keyless cache
/// entries (e.g. the Tesla) aren't the stale-LTK problem and are left alone.
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

    // VIN is read up front so every response path can include it —
    // the BLE pair card uses this to pre-populate the VIN input
    // regardless of whether pairing is complete.
    let config_path = sentryusb_config::find_config_path();
    let vin = match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => active.get("TESLA_BLE_VIN").cloned().unwrap_or_default(),
        Err(_) => String::new(),
    };
    // "Installed" now means the native BLE action binary plus the
    // keypair are present. tesla-control / tesla-keygen are gone —
    // pairing, keygen and every command are native (sentryusb-ble-action
    // + the tesla_ble crate). ble-action ships in the image, so this
    // tracks "the keypair has been generated" (the install/enable step),
    // keeping the frontend's install → pair flow intact.
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

    // Quick check (no BLE probe)
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

    // Full pairing probe via sentryusb-ble-action, which reuses the
    // telemetry daemon's warm connection (IPC) instead of spawning a
    // competing tesla-control process that fought the radio and — on
    // any timeout — wrongly deleted the paired marker. The binary
    // prints one stdout token; run_with_timeout only surfaces stdout,
    // so we match on that. 20s covers a cold direct-fallback scan when
    // the daemon is down.
    let probe = sentryusb_shell::run_with_timeout(
        Duration::from_secs(20),
        "/root/bin/sentryusb-ble-action",
        &["session-info"],
    )
    .await;
    let token = probe.as_deref().map(str::trim).unwrap_or("");

    match token {
        "PAIRED" => {
            // Feed the live "connected" indicator on the BLE card.
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
            // The ONLY case that clears the marker: the car explicitly
            // rejected our key (KEY_NOT_ON_WHITELIST). Re-pair needed.
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
            // UNREACHABLE, a non-zero exit (config error / binary
            // missing), or an empty/garbled reply: pairing is unknown,
            // not disproven. Leave the marker as-is so a busy radio or a
            // sleeping car can't flip the card to "not paired".
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

/// GET /api/system/speedtest — stream 64MB of random data for bandwidth testing.
///
/// The 64 KB chunk is filled once at first request and reused for the
/// lifetime of the process. Bandwidth tests don't need cryptographic
/// uniqueness per byte — they just need network throughput pressure —
/// so pre-filling eliminates ~8.2M `rand::random::<u64>()` calls per
/// invocation (1000 chunks × 8192 random u64s) which were the actual
/// bottleneck, not the allocation.
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
    // RTC presence is a hardware fact that doesn't change at runtime.
    // The Dashboard hits this on every load — let the browser short-
    // circuit subsequent requests for 5 min and save a round trip.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=300")],
        Json(serde_json::json!({
            "available": rtc_exists,
            "time": rtc_time,
        })),
    )
}

/// GET /api/system/clock-status
///
/// Reports whether the Pi's system clock can be trusted for
/// timestamping samples + matching them to drives later. Used by the
/// BLE pair card to show a short "clock not synced" hint ONLY when
/// all of:
///   * The system clock looks bogus (year < 2025 = unset / Jan-1-2000
///     fallback / etc.)
///   * No RTC battery is installed (with RTC, clock survives reboots)
///   * No NTP sync has happened yet
///
/// Note: the telemetry sampler can now self-correct the system clock
/// from any successful BLE state-poll response (Tesla embeds a
/// GPS-derived timestamp in every state reply). So even without RTC
/// or WiFi, the clock comes good as soon as the car responds once.
/// The warning is now informational ("we're waiting on the first
/// reading") rather than blocking.
///
/// Response shape:
/// ```json
/// {
///   "synced": true,            // year >= 2025 OR systemd-timesyncd marker
///   "has_rtc": true,           // /dev/rtc0 exists
///   "ntp_synced": true,        // /run/systemd/timesync/synchronized exists
///   "show_warning": false      // !synced && !has_rtc && !ntp_synced
/// }
/// ```
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

    // NTP sync state flips at most a handful of times per boot. A 10s
    // cache cuts repeat polling without hiding state changes that
    // matter to the BLE warning UI.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=10")],
        Json(serde_json::json!({
            "synced": synced,
            "has_rtc": has_rtc,
            "ntp_synced": ntp_synced,
            // The single boolean the UI cares about — don't pester
            // RTC users, only warn when clock is bad AND there's no
            // hardware fallback.
            "show_warning": !synced && !has_rtc,
        })),
    )
}

/// GET /api/system/ssh-pubkey
pub async fn get_ssh_pubkey(State(_s): State<AppState>) -> impl IntoResponse {
    let pub_key = std::fs::read_to_string("/root/.ssh/id_ed25519.pub")
        .or_else(|_| std::fs::read_to_string("/root/.ssh/id_rsa.pub"))
        .unwrap_or_default();
    // The pubkey only changes when generate_ssh_key runs; cache an
    // hour and let users explicitly reload when they regenerate.
    (
        StatusCode::OK,
        [(axum::http::header::CACHE_CONTROL, "private, max-age=3600")],
        Json(serde_json::json!({"public_key": pub_key.trim()})),
    )
}

/// POST /api/system/ssh-keygen
pub async fn generate_ssh_key(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // Production images run with a read-only root, so writing to
    // /root/.ssh fails (EROFS) without remounting first. remountfs_rw is
    // the canonical helper; the mount fallback covers dev images where
    // it isn't installed.
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
