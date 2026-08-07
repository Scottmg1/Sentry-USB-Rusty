//! BLE management endpoints: enable/disable toggle, VIN config,
//! connection status, lazy binary install.
//!
//! Companion to the pairing handshake in `system.rs` (`ble_pair` /
//! `ble_status`). These endpoints back the always-visible BLE card in
//! the Device settings tab — the user can flip the master toggle,
//! enter or update the VIN, see a live connection indicator, and
//! trigger an on-demand install of `tesla-control` / `tesla-keygen` if
//! they didn't enable BLE during initial setup.

use std::sync::atomic::{AtomicI64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;

/// Unix-second timestamp of the most recent successful `tesla-control`
/// invocation against the car (any subcommand). Used by `ble_connected`
/// to render a live indicator in the pair card. 0 at process start.
///
/// Writers: `system::ble_status`'s `session-info` probe and (later)
/// the telemetry sampler daemon's per-sample success path.
pub static LAST_BLE_SUCCESS_TS: AtomicI64 = AtomicI64::new(0);

/// Mark a successful tesla-control round-trip. Cheap, lock-free —
/// safe to call from any code path that just got a non-error response
/// from the car.
pub fn mark_ble_success() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    LAST_BLE_SUCCESS_TS.store(now, Ordering::Relaxed);
}

/// Whether the Tesla BLE telemetry sampler should be running.
/// **Telemetry-specific** — does NOT control the BLE keep-awake nudge
/// (that's `is_ble_keep_awake_enabled`). Strictly explicit: only true
/// when `BLE_ENABLED=yes|true|1` is in the config. Existing installs
/// are handled by `migrate_legacy_ble_flag` at API startup, which
/// writes an explicit `BLE_ENABLED=yes` for users who'd been relying
/// on the old "VIN present = implicit yes" behavior.
pub fn is_ble_enabled() -> bool {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) =
            sentryusb_config::get_config_value(&active, &commented, "BLE_ENABLED")
        {
            return matches!(v.as_str(), "yes" | "true" | "1");
        }
    }
    false
}

/// Whether the BLE keep-awake nudge (in `run/awake_start`) should
/// take the BLE path. Separate from telemetry so a user can pick:
/// telemetry only, keep-awake only, both, or neither — all four are
/// valid choices for a paired Pi.
pub fn is_ble_keep_awake_enabled() -> bool {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = sentryusb_config::get_config_value(
            &active,
            &commented,
            "BLE_KEEP_AWAKE_ENABLED",
        ) {
            return matches!(v.as_str(), "yes" | "true" | "1");
        }
    }
    false
}

/// Names a non-BLE keep-awake provider that is configured (present AND
/// non-empty) in `active`, if any.
///
/// Used to refuse enabling BLE keep-awake from the Settings → Network
/// toggle while another provider already owns keep-awake. Switching
/// methods belongs in the setup wizard (which clears the old provider
/// and sets SENTRY_CASE); letting the settings toggle stack a second
/// provider is what produces the "Multiple keep-awake providers"
/// boot-loop on the next setup run. Empty values don't count — same
/// non-empty semantics as the setup validator and the runtime
/// `${VAR:+x}` check in `run/awake_start`.
fn conflicting_keep_awake_provider(
    active: &sentryusb_config::SetupConfig,
) -> Option<&'static str> {
    let is_set = |k: &str| active.get(k).is_some_and(|v| !v.trim().is_empty());
    if is_set("TESSIE_API_TOKEN") {
        Some("Tessie")
    } else if is_set("TESLAFI_API_TOKEN") {
        Some("TeslaFi")
    } else if is_set("KEEP_AWAKE_WEBHOOK_URL") {
        Some("Webhook")
    } else {
        None
    }
}

/// One-shot migration: existing prebuilt-image users may have only
/// `TESLA_BLE_VIN` set (their pairing was done before the explicit
/// `BLE_ENABLED` / `BLE_KEEP_AWAKE_ENABLED` toggles existed). For
/// them, the old code treated VIN-present as implicit "yes" for both
/// telemetry and keep-awake. To preserve that behavior across the
/// decoupling change without surprising anyone, we write explicit
/// values once based on what was implied before.
///
/// Rules:
///   * If `BLE_KEEP_AWAKE_ENABLED` already present → skip (migration done)
///   * If `BLE_ENABLED=no` explicitly → set keep-awake=no (user had BLE off)
///   * If `BLE_ENABLED=yes` explicitly → set keep-awake=yes (preserve old coupling)
///   * If `BLE_ENABLED` unset BUT `TESLA_BLE_VIN` set → set both =yes
///     (preserve the old "implicit yes from VIN" behavior)
///   * Else → no migration needed (fresh install, user picks both toggles)
pub fn migrate_legacy_ble_flag() {
    let config_path = sentryusb_config::find_config_path();
    let Ok((mut active, commented)) = sentryusb_config::parse_file(config_path) else {
        return;
    };
    if active.contains_key("BLE_KEEP_AWAKE_ENABLED") {
        return; // already migrated
    }
    let explicit_ble =
        sentryusb_config::get_config_value(&active, &commented, "BLE_ENABLED");
    let has_vin = active.contains_key("TESLA_BLE_VIN");

    let (telemetry, keep_awake) = match explicit_ble.as_deref() {
        Some(v) if matches!(v, "yes" | "true" | "1") => ("yes", "yes"),
        Some(_) => ("no", "no"),
        None if has_vin => ("yes", "yes"),
        None => return, // nothing to migrate
    };

    active.insert("BLE_ENABLED".to_string(), telemetry.to_string());
    active.insert(
        "BLE_KEEP_AWAKE_ENABLED".to_string(),
        keep_awake.to_string(),
    );
    let _ = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status();
    if let Err(e) = sentryusb_config::write_file(config_path, &active) {
        tracing::warn!("BLE config migration write failed: {e}");
    } else {
        tracing::info!(
            "BLE config migrated: BLE_ENABLED={}, BLE_KEEP_AWAKE_ENABLED={}",
            telemetry,
            keep_awake
        );
    }
}

/// One-shot: ensure `BLE_KEEP_AWAKE_VIA_SAMPLER` is present in the
/// config file with a default of `no`. Lets the Raw Configuration
/// editor surface the key as a togglable row out of the box, instead
/// of forcing every tester to add it by hand with the `+` button.
/// Idempotent — skips if the key is already set (so a tester who
/// flipped it doesn't get clobbered back to `no` on restart).
///
/// Accepted values (parsed in `tesla_telemetry::config`):
///   * `no` (default) → legacy spawned `ble-action charge-port-close`
///   * `wake` (or `yes`/`true`/`1`) → sampler-emit + VCSEC wake
///   * `charge-port-close` → sampler-emit + Infotainment charge-port-close
///   * `combo` → sampler-emit + wake → 2s pause → charge-port-close
pub fn ensure_keep_awake_via_sampler_key_present() {
    let config_path = sentryusb_config::find_config_path();
    let Ok((mut active, _commented)) = sentryusb_config::parse_file(config_path) else {
        return;
    };
    if active.contains_key("BLE_KEEP_AWAKE_VIA_SAMPLER") {
        return;
    }
    active.insert("BLE_KEEP_AWAKE_VIA_SAMPLER".to_string(), "no".to_string());
    let _ = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status();
    if let Err(e) = sentryusb_config::write_file(config_path, &active) {
        tracing::warn!("BLE_KEEP_AWAKE_VIA_SAMPLER seed write failed: {e}");
    } else {
        tracing::info!("seeded BLE_KEEP_AWAKE_VIA_SAMPLER=no (togglable in Raw Config editor)");
    }
}

/// GET /api/system/ble-enabled
pub async fn ble_enabled_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::OK,
        Json(serde_json::json!({ "enabled": is_ble_enabled() })),
    )
}

/// POST /api/system/ble-enabled
///
/// Body: `{"enabled": true|false}`. Writes `BLE_ENABLED=yes|no` to
/// the config file. The change is eventually consistent: the
/// keep-awake script and (future) telemetry sampler re-read the
/// config on each loop iteration, so no in-flight processes need to
/// be force-killed here.
pub async fn ble_enabled_set(
    State(_s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let enabled = match body.get("enabled").and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "missing or non-bool `enabled` field",
            );
        }
    };

    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert(
            "BLE_ENABLED".to_string(),
            if enabled { "yes" } else { "no" }.to_string(),
        );
        // On first-time enable: if BLE_ADAPTER isn't already set and
        // an external USB BLE dongle is plugged in, auto-select it.
        // Matches the prebuilt-image flow — user flashes the image,
        // plugs in their dongle, boots the Pi, and runs setup. They
        // shouldn't have to know about the adapter chooser at all;
        // having a dongle inserted at enable time is intent enough.
        // Won't override an existing value, so a user who explicitly
        // picked the onboard radio earlier keeps their choice.
        if enabled && !active.contains_key("BLE_ADAPTER") {
            if let Some(external_id) = first_external_adapter() {
                active.insert("BLE_ADAPTER".to_string(), external_id);
            }
        }
        // The Pi's root partition is normally mounted read-only;
        // `remountfs_rw` flips it to rw for the duration of the
        // write. Match the existing pattern from
        // notifications::auto_enable_mobile_push_in_config.
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => (
            StatusCode::OK,
            Json(serde_json::json!({ "enabled": enabled })),
        ),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write config: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write task panicked: {}", e),
        ),
    }
}

/// Classify a BT adapter as "onboard" (Pi built-in radio) or
/// "external" (USB dongle).
///
/// Primary signal is the device symlink under
/// `/sys/class/bluetooth/<id>/device` — points at a USB path
/// (e.g. `4-1:1.0`) for dongles and at `serial0-0` for the Pi's
/// UART-attached onboard radio. This works on every Pi OS kernel
/// regardless of rfkill state.
///
/// The `manufacturer` sysfs file is checked as a secondary signal
/// because it's not populated on all kernels (Pi OS in particular
/// often leaves it missing) — but when present it carries the
/// Bluetooth SIG company ID, which uniquely identifies Cypress
/// (305) and Broadcom (15) Pi chips.
///
/// We can't classify by hci index: a USB dongle plugged in early
/// can grab hci0 and push the onboard radio to hci1. That
/// mislabeling has already caused at least one round of "the
/// onboard radio has issues" testing that was actually hitting the
/// external dongle the whole time.
pub fn adapter_source(id: &str) -> &'static str {
    // 1. Device symlink — works on all Pi OS versions regardless of
    //    rfkill state.
    //      Pi onboard BT → `../../../serial0-0` (UART)
    //      USB dongle    → `../../../4-1:1.0`   (bus-port:intf.alt)
    //    The `:` in the USB form is the giveaway — bare hyphens
    //    appear in `serial0-0` too, so we can't rely on those.
    if let Ok(target) = std::fs::read_link(format!("/sys/class/bluetooth/{id}/device")) {
        let s = target.to_string_lossy();
        if s.contains("serial") || s.contains("uart") || s.contains("tty") {
            return "onboard";
        }
        if s.contains("usb") || s.contains(':') {
            return "external";
        }
    }
    // 2. Manufacturer file — only present on some kernels, but
    //    definitive when it is. Cypress (305) and Broadcom (15) are
    //    Pi onboard chips.
    if let Ok(raw) = std::fs::read_to_string(format!("/sys/class/bluetooth/{id}/manufacturer")) {
        match raw.trim().parse::<u32>().unwrap_or(0) {
            15 | 305 => return "onboard",
            v if v > 0 => return "external",
            _ => {}
        }
    }
    // 3. Default: treat unknowns as external. Safer than mislabeling
    //    something as the built-in radio.
    "external"
}

/// Return the first detected external (non-Pi-onboard) BT adapter,
/// if any. Used by the first-time-enable auto-select to pick the
/// USB dongle without bothering the user, on the assumption that if
/// the user has a dongle plugged in they want to use it. Classifies
/// by silicon vendor rather than hci index — see `adapter_source`.
fn first_external_adapter() -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/bluetooth").ok()?;
    let mut ids: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with("hci")
                && !name.contains(':')
                && adapter_source(&name) == "external"
            {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    ids.sort();
    ids.into_iter().next()
}

/// GET /api/system/ble-keep-awake-enabled
///
/// Returns `{ enabled, blocked_by }`. `blocked_by` names a non-BLE
/// keep-awake provider that's already configured (Tessie/TeslaFi/Webhook),
/// or null. The settings UI uses it to disable and explain the BLE
/// keep-awake toggle, since only one provider can be active at a time.
pub async fn ble_keep_awake_enabled_get(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let blocked_by = {
        let config_path = sentryusb_config::find_config_path();
        sentryusb_config::parse_file(config_path)
            .ok()
            .and_then(|(active, _)| conflicting_keep_awake_provider(&active))
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "enabled": is_ble_keep_awake_enabled(),
            "blocked_by": blocked_by,
        })),
    )
}

/// POST /api/system/ble-keep-awake-enabled
///
/// Body: `{"enabled": true|false}`. Writes `BLE_KEEP_AWAKE_ENABLED`.
/// Independent of `BLE_ENABLED` (telemetry), so users can flip either
/// without affecting the other. The `awake_start` script reads this
/// value on every nudge cycle, so the change is eventually consistent
/// without needing a service restart.
pub async fn ble_keep_awake_enabled_set(
    State(_s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let enabled = match body.get("enabled").and_then(|v| v.as_bool()) {
        Some(b) => b,
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "missing or non-bool `enabled` field",
            );
        }
    };
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<&'static str>> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        // Refuse to enable BLE keep-awake while another provider already
        // owns it — only one keep-awake provider may be active at a time.
        // Switching methods is the wizard's job (it clears the old
        // provider and sets SENTRY_CASE); letting this toggle stack a
        // second provider is what bails the next setup run into a boot
        // loop ("Multiple keep-awake providers configured").
        if enabled {
            if let Some(name) = conflicting_keep_awake_provider(&active) {
                return Ok(Some(name));
            }
        }
        active.insert(
            "BLE_KEEP_AWAKE_ENABLED".to_string(),
            if enabled { "yes" } else { "no" }.to_string(),
        );
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(None)
    })
    .await;
    match result {
        Ok(Ok(None)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "enabled": enabled })),
        ),
        Ok(Ok(Some(name))) => crate::json_error(
            StatusCode::CONFLICT,
            &format!(
                "{name} is already set as your keep-awake provider. Change the \
                 keep-awake method in Setup — only one can be active at a time."
            ),
        ),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write config: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write task panicked: {}", e),
        ),
    }
}

/// POST /api/system/ble-vin
///
/// Body: `{"vin": "5YJ3E1EA4LF..."}`. Writes `TESLA_BLE_VIN` to the
/// config file after lightweight validation. The setup wizard used to
/// be the only place to collect this; now the always-visible BLE card
/// in Device settings lets a user enter (or update) the VIN at any
/// time.
///
/// Validation is intentionally permissive — exact 17-char Tesla VINs
/// are required, but we don't enforce the country/model digit ranges
/// since Tesla can extend the VIN format on future vehicles. The
/// pairing call itself is the real validator: it will refuse a VIN
/// the car doesn't recognize.
pub async fn ble_vin_set(
    State(_s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let vin_raw = match body.get("vin").and_then(|v| v.as_str()) {
        Some(s) => s.trim().to_string(),
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "missing or non-string `vin` field",
            );
        }
    };

    let vin = vin_raw.to_uppercase();
    if vin.len() != 17 || !vin.chars().all(|c| c.is_ascii_alphanumeric()) {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "VIN must be exactly 17 alphanumeric characters",
        );
    }

    let vin_for_write = vin.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert("TESLA_BLE_VIN".to_string(), vin_for_write);
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => (StatusCode::OK, Json(serde_json::json!({ "vin": vin }))),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("failed to write config: {}", e),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("config write task panicked: {}", e),
        ),
    }
}

/// GET /api/system/ble-connected
///
/// Reports the unix-second timestamp of the most recent successful
/// BLE round-trip, plus a derived `seconds_ago`. The timestamp is
/// taken as the maximum of two sources:
///   * `LAST_BLE_SUCCESS_TS` — webui process's own probes (clicking
///     pair, settings-page session-info polls).
///   * `MAX(ts) FROM telemetry_samples` — the out-of-process sampler
///     daemon's autonomous activity.
/// Without the second source the indicator would say "Disconnected"
/// while the sampler is happily writing rows every 15 s.
///
/// Callers (the BlePairButton card) interpret the freshness:
///   * `seconds_ago < 60`  → "Connected"
///   * `< 600`            → "Last seen Ns ago"
///   * `>= 600` or null   → "Disconnected"
///
/// `sample_count_10min` lets the UI render a "5 samples / 10m" hint
/// so the user can tell that data is flowing, not just that the radio
/// pinged once a long time ago.
pub async fn ble_connected(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let probe_ts = LAST_BLE_SUCCESS_TS.load(Ordering::Relaxed);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let since = now - 600;

    let store = s.drives.store.clone();
    let (sampler_ts, sample_count_10min) = tokio::task::spawn_blocking(move || {
        store.with_read_conn(|conn| {
            let max_ts: Option<i64> = conn
                .query_row(
                    "SELECT MAX(ts) FROM telemetry_samples",
                    [],
                    |r| r.get(0),
                )
                .ok()
                .flatten();
            let count: i64 = conn
                .query_row(
                    "SELECT count(*) FROM telemetry_samples WHERE ts >= ?1",
                    (since,),
                    |r| r.get(0),
                )
                .unwrap_or(0);
            (max_ts.unwrap_or(0), count)
        })
    })
    .await
    .unwrap_or((0, 0));

    let last = probe_ts.max(sampler_ts);
    let seconds_ago = if last == 0 {
        None
    } else {
        Some((now - last).max(0))
    };

    // Surface "why the gap" context so the UI can explain a stale
    // connection instead of just saying "Disconnected". The keep-awake
    // nudge claims the BLE radio (writes "keep_awake" into
    // /tmp/ble_radio_owner) — typically because archiveloop is in the
    // middle of an archive cycle and is poking the car to prevent USB
    // power-off. While that owner is set, the sampler can't take new
    // samples, so the freshness pill should say "paused" not "broken".
    let radio_owner = read_radio_owner();
    let archiving = crate::drives_handler::is_archiving();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "last_success_ts": last,
            "seconds_ago": seconds_ago,
            "sample_count_10min": sample_count_10min,
            "radio_owner": radio_owner,
            "archiving": archiving,
        })),
    )
}

/// Read the first line of `/tmp/ble_radio_owner` — the owner-name
/// string written by `awake_start` ("keep_awake") or the telemetry
/// sampler ("telemetry"). Returns None when the file is missing
/// (no one holds the radio).
fn read_radio_owner() -> Option<String> {
    let contents = std::fs::read_to_string("/tmp/ble_radio_owner").ok()?;
    let first = contents.lines().next()?.trim();
    if first.is_empty() { None } else { Some(first.to_string()) }
}

/// GET /api/system/ble-latest-sample
///
/// Live gate snapshot the telemetry daemon overwrites each Active tick;
/// mirrors `GATE_STATUS_PATH` in the daemon.
const GATE_STATUS_PATH: &str = "/mutable/sentryusb-ble-gate.txt";

/// `(sentry, charging, shift)` from the gate file, `None` each if unwritten.
/// `"unknown"` is kept (not nulled) — it means the daemon couldn't read it.
fn read_gate_status() -> (Option<String>, Option<String>, Option<String>) {
    let Ok(body) = std::fs::read_to_string(GATE_STATUS_PATH) else {
        return (None, None, None);
    };
    let mut sentry = None;
    let mut charging = None;
    let mut shift = None;
    for line in body.lines() {
        if let Some(v) = line.strip_prefix("sentry_mode=") {
            sentry = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("charging_state=") {
            charging = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("shift_state=") {
            shift = Some(v.trim().to_string());
        }
    }
    (sentry, charging, shift)
}

/// Returns the most recent row from `telemetry_samples` so the UI can
/// show the user exactly what the Pi is pulling from the car right
/// now. Null fields just stay null in the response — e.g. a
/// `body_controller` sample only carries `ts` and `source`.
///
/// Used by the "Show output" panel on the BLE pair card. Polled
/// every 5 s while the panel is open.
/// How far back the latest-sample queries look. `telemetry_samples` is
/// keyed on `ts` (WITHOUT ROWID), so an `ORDER BY ts DESC LIMIT 1` with a
/// `ts >= floor` bound is a short reverse B-tree slice that stops at the
/// floor — without it, a column the car never reports (TPMS on some
/// models, `location_name`) walks the entire table on every poll of this
/// endpoint (every 5s while the panel is open). Data older than this is
/// stale enough that showing it as "current" would mislead anyway.
const LATEST_SAMPLE_WINDOW_SECS: i64 = 30 * 86_400;

pub async fn ble_latest_sample(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = s.drives.store.clone();
    let floor = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - LATEST_SAMPLE_WINDOW_SECS;
    let result = tokio::task::spawn_blocking(move || {
        store.with_read_conn(|conn| {
            // Pull two things:
            //   1. The "envelope" — `ts` + `source` of the most recent
            //      sample, used by the UI to render "polled Xs ago"
            //      and to label what kind of sample was last taken
            //      (state vs body_controller).
            //   2. Per-field latest-non-null for every value field.
            //      This way the UI shows the last-known reading for
            //      each field even when the most-recent sample didn't
            //      include it (e.g. body_controller samples carry no
            //      battery/temps/HVAC — without this trick, those
            //      fields would go blank the moment the sampler dropped
            //      to Quiet mode, even though we have fresh values
            //      from the last state poll).
            //
            // Trade-off acknowledged: the displayed values can be
            // older than the "polled Xs ago" envelope timestamp.
            // The footer hint in the UI explains the body_controller
            // case; for state polls everything will match.
            //
            // battery_temp_c intentionally not selected — Tesla doesn't
            // expose battery cell temp via the state API, so the column
            // is always NULL.
            // "Envelope" timestamp = when we last successfully read
            // *real* telemetry from the car. We filter to
            // source='state' so body_controller pings don't make the
            // UI say "polled 2s ago" when the actual battery/temps
            // are from a state poll 10 min ago. The body_controller
            // pings still count for the "Connected" indicator
            // (handled by ble_connected) — different concern.
            let envelope = conn
                .query_row(
                    "SELECT ts, source FROM telemetry_samples \
                     WHERE source = 'state' AND ts >= ?1 \
                     ORDER BY ts DESC LIMIT 1",
                    (floor,),
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .ok();
            let latest_col_f64 = |col: &str| -> Option<f64> {
                conn.query_row(
                    &format!(
                        "SELECT {col} FROM telemetry_samples \
                         WHERE {col} IS NOT NULL AND ts >= ?1 \
                         ORDER BY ts DESC LIMIT 1"
                    ),
                    (floor,),
                    |r| r.get(0),
                )
                .ok()
            };
            // Like `latest_col_f64`, but also returns the `ts` of the row
            // the value came from. The endpoint surfaces this per-field
            // age so the UI can flag a value that's much older than the
            // envelope timestamp — fixes "interior 19°C, updated 10s ago"
            // when the 19°C reading is actually hours stale (a failed
            // climate poll leaves the old value as latest-non-null while a
            // charge/body poll advances the envelope). Pairs (value, ts).
            let latest_col_f64_aged = |col: &str| -> Option<(f64, i64)> {
                conn.query_row(
                    &format!(
                        "SELECT {col}, ts FROM telemetry_samples \
                         WHERE {col} IS NOT NULL AND ts >= ?1 \
                         ORDER BY ts DESC LIMIT 1"
                    ),
                    (floor,),
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok()
            };
            let latest_col_i64 = |col: &str| -> Option<i64> {
                conn.query_row(
                    &format!(
                        "SELECT {col} FROM telemetry_samples \
                         WHERE {col} IS NOT NULL AND ts >= ?1 \
                         ORDER BY ts DESC LIMIT 1"
                    ),
                    (floor,),
                    |r| r.get(0),
                )
                .ok()
            };
            let latest_col_string = |col: &str| -> Option<String> {
                conn.query_row(
                    &format!(
                        "SELECT {col} FROM telemetry_samples \
                         WHERE {col} IS NOT NULL AND ts >= ?1 \
                         ORDER BY ts DESC LIMIT 1"
                    ),
                    (floor,),
                    |r| r.get(0),
                )
                .ok()
            };
            // Latest body-controller poll ts (independent of the
            // state envelope above). The UI uses this to tell
            // "car is in Quiet mode, last state poll was X ago by
            // design" apart from "data is genuinely stale because
            // BLE is failing." If body-controller polls are landing
            // recently but state polls aren't, the car is asleep —
            // expected and not alarming.
            let body_controller_ts: Option<i64> = conn
                .query_row(
                    "SELECT ts FROM telemetry_samples \
                     WHERE source = 'body_controller' AND ts >= ?1 \
                     ORDER BY ts DESC LIMIT 1",
                    (floor,),
                    |r| r.get(0),
                )
                .ok();
            envelope.map(|(ts, source)| {
                (
                    ts,
                    source,
                    // Volatile, freshness-sensitive fields carry their own
                    // row ts so the UI can show per-field age.
                    latest_col_f64_aged("battery_pct"),
                    latest_col_f64_aged("interior_temp_c"),
                    latest_col_f64_aged("exterior_temp_c"),
                    latest_col_i64("hvac_on"),
                    // Tires are polled together; tire_fl's ts represents
                    // the whole TPMS set's age.
                    latest_col_f64_aged("tire_fl_psi"),
                    latest_col_f64("tire_fr_psi"),
                    latest_col_f64("tire_rl_psi"),
                    latest_col_f64("tire_rr_psi"),
                    latest_col_f64("odometer_mi"),
                    latest_col_string("location_name"),
                    body_controller_ts,
                )
            })
        })
    })
    .await
    .ok()
    .flatten();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Live sentry/charge/shift from the daemon's gate file (not the DB).
    let (sentry_mode, charging_state, shift_state) = read_gate_status();

    match result {
        Some((
            ts,
            source,
            battery_pct,
            interior_temp_c,
            exterior_temp_c,
            hvac_on,
            tire_fl_psi,
            tire_fr_psi,
            tire_rl_psi,
            tire_rr_psi,
            odometer_mi,
            location_name,
            body_controller_ts,
        )) => {
            // Age of each aged (value, ts) pair, in seconds — how old the
            // shown value actually is, independent of the envelope's
            // "seconds_ago". The UI flags a field whose age greatly
            // exceeds the envelope so a stale temp can't read as current.
            let age = |pair: &Option<(f64, i64)>| pair.map(|(_, t)| (now - t).max(0));
            (
            StatusCode::OK,
            Json(serde_json::json!({
                "ts": ts,
                "seconds_ago": (now - ts).max(0),
                "battery_pct": battery_pct.map(|(v, _)| v),
                "interior_temp_c": interior_temp_c.map(|(v, _)| v),
                "exterior_temp_c": exterior_temp_c.map(|(v, _)| v),
                "hvac_on": hvac_on.map(|v| v != 0),
                "tire_fl_psi": tire_fl_psi.map(|(v, _)| v),
                "tire_fr_psi": tire_fr_psi,
                "tire_rl_psi": tire_rl_psi,
                "tire_rr_psi": tire_rr_psi,
                "odometer_mi": odometer_mi,
                "location_name": location_name,
                // Per-field age (seconds) for the volatile values the
                // dashboard shows. Null when the field has no value.
                "field_secs_ago": {
                    "battery_pct": age(&battery_pct),
                    "interior_temp_c": age(&interior_temp_c),
                    "exterior_temp_c": age(&exterior_temp_c),
                    "tires": age(&tire_fl_psi),
                },
                // Live gate inputs from the daemon's gate file, not the DB.
                "sentry_mode": sentry_mode,
                "charging_state": charging_state,
                "shift_state": shift_state,
                "source": source,
                // Null if we've never seen a body-controller poll. The
                // UI uses recent body-controller-but-stale-state to
                // tell Quiet mode (car asleep, expected) apart from
                // truly-stale (everything failing).
                "body_controller_seconds_ago": body_controller_ts
                    .map(|t| (now - t).max(0)),
            })),
        )
        }
        None => (
            StatusCode::OK,
            Json(serde_json::json!({ "ts": null })),
        ),
    }
}

/// POST /api/system/ble-force-poll
///
/// Triggers a one-shot full state poll right now, bypassing the
/// sampler's normal schedule. Useful when the user just changed
/// something on the car (e.g. flipped Sentry on, plugged in a
/// charger) and wants to verify the Pi can see fresh data without
/// waiting for the next scheduled tick.
///
/// Sends SIGUSR1 to the telemetry daemon as a "do a state poll
/// now" signal. The daemon listens and runs a one-cycle Active
/// poll regardless of current phase. If the daemon doesn't respond
/// (process not running, or signal handler not installed) the
/// endpoint still returns 200 — the UI will just see no new
/// sample within ~10s, which is its own feedback.
///
/// Implementation: we don't fork tesla-control from the api crate
/// directly (would fight with the sampler for the BLE radio lock).
/// Instead we ping the sampler to do it, which already knows how
/// to coordinate the lock and persist results correctly.
pub async fn ble_force_poll(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Best-effort: signal the daemon. Errors are non-fatal — the
    // UI will discover the result via the normal poll cycle. Async
    // (tokio process under the hood) so the handler doesn't block a
    // runtime worker on the fork+wait.
    let _ = sentryusb_shell::run(
        "systemctl",
        &["kill", "-s", "SIGUSR1", "sentryusb-telemetry"],
    )
    .await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "queued": true })),
    )
}

/// GET /api/system/ble-adapters
///
/// Returns the list of Bluetooth adapters the kernel is currently
/// aware of (parsed from `/sys/class/bluetooth/hci*`), plus the
/// currently-configured `BLE_ADAPTER` value. Used by the BLE pair
/// card to show a "Use external adapter" switch when the user
/// plugs in a USB BLE dongle.
///
/// Response shape:
/// ```json
/// {
///   "current": "hci0",
///   "default": "hci0",
///   "available": [
///     { "id": "hci0", "source": "onboard", "address": "88:A2:9E:..." },
///     { "id": "hci1", "source": "external", "address": "00:1A:7D:..." }
///   ]
/// }
/// ```
///
/// `source` is classified by the silicon vendor (read from
/// `/sys/class/bluetooth/<id>/manufacturer`) — `onboard` for
/// Cypress/Broadcom (the Pi built-in chips), `external` for
/// anything else. Index-based labeling (`hci0 = onboard`) is
/// unreliable: a USB dongle plugged in early can grab hci0 and
/// push the onboard radio to hci1, leading to "the onboard radio
/// has issues" reports that actually point at the dongle.
pub async fn ble_adapters(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut adapters: Vec<serde_json::Value> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/bluetooth") {
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("hci") && !name.contains(':') {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        ids.sort();
        for id in ids {
            let address = std::fs::read_to_string(format!(
                "/sys/class/bluetooth/{id}/address"
            ))
            .ok()
            .map(|s| s.trim().to_string());
            let source = adapter_source(&id);
            adapters.push(serde_json::json!({
                "id": id,
                "source": source,
                "address": address,
            }));
        }
    }

    let current = current_ble_adapter();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "current": current,
            "default": "hci0",
            "available": adapters,
        })),
    )
}

/// Read `BLE_ADAPTER` from the config. Returns `"hci0"` when unset
/// or unparseable — mirrors the resolution the telemetry sampler
/// and sentryusb-ble.py both do, so all three surfaces agree on
/// which adapter is "current".
fn current_ble_adapter() -> String {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("BLE_ADAPTER") {
            let trimmed = v.trim();
            if trimmed.starts_with("hci") {
                return trimmed.to_string();
            }
        }
    }
    "hci0".to_string()
}

/// POST /api/system/ble-adapter
///
/// Body: `{"adapter": "hci1"}`. Writes `BLE_ADAPTER` to the config
/// file and restarts both BLE services (`sentryusb-telemetry` for
/// the Tesla sampler, `sentryusb-ble` for the iOS GATT peripheral)
/// so the new adapter takes effect immediately without a Pi reboot.
///
/// Validation: must start with `hci` and match a real entry under
/// `/sys/class/bluetooth/`. An invalid value would just be ignored
/// at the consumer level, but defending here gives the UI a clear
/// error message instead of a silent revert.
pub async fn ble_adapter_set(
    State(_s): State<AppState>,
    Json(body): Json<serde_json::Value>,
) -> (StatusCode, Json<serde_json::Value>) {
    let adapter = match body.get("adapter").and_then(|v| v.as_str()) {
        Some(s) => s.trim().to_string(),
        None => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "missing or non-string `adapter` field",
            );
        }
    };

    if !adapter.starts_with("hci") {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "adapter must start with 'hci' (e.g. hci0, hci1)",
        );
    }

    // Verify the adapter actually exists in /sys
    if !std::path::Path::new(&format!("/sys/class/bluetooth/{adapter}")).exists() {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            &format!("adapter {adapter} not found — is the dongle plugged in?"),
        );
    }

    let adapter_for_write = adapter.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert("BLE_ADAPTER".to_string(), adapter_for_write);
        // /root is read-only at rest; flip to rw for the write.
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to write config: {e}"),
            );
        }
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("config write task panicked: {e}"),
            );
        }
    }

    // Restart both BLE services so the new adapter is picked up.
    // Best-effort — log on failure but don't error the request, the
    // config write already succeeded and a Pi reboot would also work.
    // Async so a slow service restart doesn't pin a runtime worker.
    if let Err(e) = sentryusb_shell::run("systemctl", &["restart", "sentryusb-telemetry"]).await {
        tracing::warn!("restart sentryusb-telemetry after adapter change failed: {e:#}");
    }
    if let Err(e) = sentryusb_shell::run("systemctl", &["restart", "sentryusb-ble"]).await {
        tracing::warn!("restart sentryusb-ble after adapter change failed: {e:#}");
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({ "adapter": adapter })),
    )
}

/// POST /api/system/ble-install
///
/// Idempotent lazy install of `tesla-control` and `tesla-keygen`,
/// plus first-time keypair generation. Used when the user opts into
/// BLE from the settings page on a Pi that didn't have BLE selected
/// during initial setup.
///
/// Runs in a background task so the HTTP response can return
/// immediately. Progress lands on the `ble_install_status` WebSocket
/// topic so the BlePairButton card can show a spinner with the
/// current step.
pub async fn ble_install(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // "Installed" = the BLE keypair exists. tesla-control / tesla-keygen
    // are no longer downloaded — install now just ensures bluez and
    // generates the native keypair (the only durable artifact the step
    // produces). The native sentryusb-ble-action binary ships in the
    // image alongside the API server, so it's always present here.
    let already_installed = std::path::Path::new("/root/.ble/key_private.pem").exists();

    let hub = s.hub.clone();
    tokio::spawn(async move {
        if already_installed {
            hub.broadcast(
                "ble_install_status",
                &serde_json::json!({ "status": "done", "already_installed": true }),
            );
            return;
        }
        hub.broadcast(
            "ble_install_status",
            &serde_json::json!({ "status": "installing" }),
        );
        let hub_progress = hub.clone();
        let result =
            sentryusb_setup::archive::install_tesla_ble_binaries(move |msg| {
                hub_progress.broadcast(
                    "ble_install_status",
                    &serde_json::json!({ "status": "progress", "message": msg }),
                );
            })
            .await;

        match result {
            Ok(()) => {
                hub.broadcast(
                    "ble_install_status",
                    &serde_json::json!({ "status": "done", "already_installed": false }),
                );
            }
            Err(e) => {
                hub.broadcast(
                    "ble_install_status",
                    &serde_json::json!({ "status": "error", "error": e.to_string() }),
                );
            }
        }
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "started",
            "already_installed": already_installed,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> sentryusb_config::SetupConfig {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn detects_real_tessie_token_as_conflict() {
        let c = cfg(&[("TESSIE_API_TOKEN", "real-token")]);
        assert_eq!(conflicting_keep_awake_provider(&c), Some("Tessie"));
    }

    #[test]
    fn detects_real_webhook_as_conflict() {
        let c = cfg(&[("KEEP_AWAKE_WEBHOOK_URL", "http://ha.local/hook")]);
        assert_eq!(conflicting_keep_awake_provider(&c), Some("Webhook"));
    }

    #[test]
    fn empty_or_whitespace_token_is_not_a_conflict() {
        // The exact state the wizard leaves behind when switching to BLE.
        let c = cfg(&[("TESSIE_API_TOKEN", ""), ("TESLAFI_API_TOKEN", "   ")]);
        assert_eq!(conflicting_keep_awake_provider(&c), None);
    }

    #[test]
    fn bare_vin_alone_is_not_a_conflict() {
        let c = cfg(&[("TESLA_BLE_VIN", "5YJ3E1EA4JF000001")]);
        assert_eq!(conflicting_keep_awake_provider(&c), None);
    }
}
