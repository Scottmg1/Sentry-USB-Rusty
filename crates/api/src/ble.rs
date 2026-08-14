//! BLE telemetry/keep-awake settings, adapter selection, pairing health, and
//! native keypair setup. The pairing handshake lives in `system.rs`.

use std::sync::atomic::{AtomicI64, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use sentryusb_ble_health::{
    DEFAULT_HEALTH_PATH, HealthInput, HealthRecord, Severity, classify_health, read_at,
};

use crate::router::AppState;

/// Latest successful authenticated API pairing probe, or zero at startup.
pub static LAST_BLE_SUCCESS_TS: AtomicI64 = AtomicI64::new(0);

/// Record an authenticated pairing probe.
pub fn mark_ble_success() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    LAST_BLE_SUCCESS_TS.store(now, Ordering::Relaxed);
}

/// Shared keep-accessory/away-mode home geofence, defaulting to 120 m.
fn home_geofence() -> Option<(f64, f64, f64)> {
    let config_path = sentryusb_config::find_config_path();
    let (active, commented) = sentryusb_config::parse_file(config_path).ok()?;
    let g = |k: &str| sentryusb_config::get_config_value(&active, &commented, k);
    // Range checks also reject NaN/infinity and malformed world coordinates.
    let lat = g("KEEP_ACCESSORY_HOME_LAT")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| (-90.0..=90.0).contains(v))?;
    let lon = g("KEEP_ACCESSORY_HOME_LON")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(crate::normalize_lon)?;
    let radius = g("KEEP_ACCESSORY_HOME_RADIUS_M")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|r| r.is_finite() && *r > 0.0 && *r <= 100_000.0)
        .unwrap_or(120.0);
    Some((lat, lon, radius))
}

/// Great-circle distance in meters (haversine). Local copy, as in `away_mode`.
fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_R_M: f64 = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlambda / 2.0).sin().powi(2);
    2.0 * EARTH_R_M * a.sqrt().min(1.0).asin()
}

/// Whether telemetry is explicitly enabled. Keep-awake is independent; startup
/// migration converts legacy VIN-implied enablement.
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

/// Whether `run/awake_start` uses BLE, independently of telemetry sampling.
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

/// Return a configured non-BLE keep-awake provider. Empty values follow setup
/// and runtime semantics and do not count. This prevents stacking providers.
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

/// Materialize legacy VIN-implied BLE settings once so telemetry and keep-awake
/// can be controlled independently without changing existing behavior.
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

/// Add the sampler keep-awake mode default without overwriting an existing key.
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

/// POST /api/system/ble-enabled writes `BLE_ENABLED=yes|no`; the sampler
/// observes it on its next config read.
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
        // On first enable, prefer an attached external adapter unless configured.
        if enabled && !active.contains_key("BLE_ADAPTER") {
            if let Some(external_id) = first_external_adapter() {
                active.insert("BLE_ADAPTER".to_string(), external_id);
            }
        }
        // Root is normally read-only; remount for the config write.
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

/// Classify adapters from sysfs topology, then manufacturer ID. HCI indexes are
/// unstable when a USB dongle initializes before the onboard UART radio.
pub fn adapter_source(id: &str) -> &'static str {
    // USB topology includes an interface colon; onboard UART paths do not.
    if let Ok(target) = std::fs::read_link(format!("/sys/class/bluetooth/{id}/device")) {
        let s = target.to_string_lossy();
        if s.contains("serial") || s.contains("uart") || s.contains("tty") {
            return "onboard";
        }
        if s.contains("usb") || s.contains(':') {
            return "external";
        }
    }
    // Cypress (305) and Broadcom (15) identify Pi onboard chips when exposed.
    if let Ok(raw) = std::fs::read_to_string(format!("/sys/class/bluetooth/{id}/manufacturer")) {
        match raw.trim().parse::<u32>().unwrap_or(0) {
            15 | 305 => return "onboard",
            v if v > 0 => return "external",
            _ => {}
        }
    }
    // Unknown adapters are safer to present as external.
    "external"
}

/// Return the first external adapter for initial auto-selection.
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

/// GET /api/system/ble-keep-awake-enabled returns enablement and any provider
/// that blocks it.
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

/// POST /api/system/ble-keep-awake-enabled, independent of telemetry. The
/// awake script observes changes on the next nudge.
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
        // Only the wizard may switch providers because it clears prior config.
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

/// POST /api/system/ble-vin writes a 17-character alphanumeric VIN. Pairing,
/// rather than country/model digit assumptions, performs semantic validation.
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

/// GET /api/system/ble-connected combines authenticated API probes with recent
/// state samples. Unauthenticated body-controller pings do not prove pairing
/// health. The response also counts state samples from the last ten minutes.
fn authenticated_activity(conn: &rusqlite::Connection, since: i64) -> (i64, i64) {
    let max_ts: Option<i64> = conn
        .query_row(
            "SELECT MAX(ts) FROM telemetry_samples WHERE source = 'state'",
            [],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    let count = conn
        .query_row(
            "SELECT count(*) FROM telemetry_samples WHERE source = 'state' AND ts >= ?1",
            (since,),
            |row| row.get(0),
        )
        .unwrap_or(0);
    (max_ts.unwrap_or(0), count)
}

fn health_is_configured(
    enabled: bool,
    paired_marker: bool,
    last_authenticated_success_ts: i64,
    has_health_record: bool,
) -> bool {
    enabled && (paired_marker || last_authenticated_success_ts > 0 || has_health_record)
}

fn ble_health_allows_controls(
    enabled: bool,
    record: Option<HealthRecord>,
    last_authenticated_success_ts: i64,
    radio_owner: Option<&str>,
    archiving: bool,
    now_ts: i64,
) -> bool {
    enabled
        && classify_health(&HealthInput {
            record,
            last_authenticated_success_ts,
            radio_owner,
            archiving,
            now_ts,
        })
        .severity
            == Severity::Green
}

/// Fail-closed server-side health gate for BLE vehicle controls. This uses
/// the same durable fault record and authenticated-state freshness model as
/// `/api/system/ble-connected`, rather than trusting browser state.
pub(crate) fn charging_controls_health_is_green(
    conn: &rusqlite::Connection,
    now_ts: i64,
) -> bool {
    let probe_ts = LAST_BLE_SUCCESS_TS.load(Ordering::Relaxed);
    let sampler_ts = authenticated_activity(conn, now_ts - 600).0;
    let last_authenticated_success_ts = probe_ts.max(sampler_ts);
    let record = match read_at(std::path::Path::new(DEFAULT_HEALTH_PATH)) {
        Ok(record) => record,
        Err(e) => {
            tracing::warn!("could not read BLE health record for charging controls: {e}");
            return false;
        }
    };
    let radio_owner = read_radio_owner();
    ble_health_allows_controls(
        is_ble_enabled(),
        record,
        last_authenticated_success_ts,
        radio_owner.as_deref(),
        crate::drives_handler::is_archiving(),
        now_ts,
    )
}

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
        store.with_read_conn(|conn| authenticated_activity(conn, since))
    })
    .await
    .unwrap_or((0, 0));

    let last = probe_ts.max(sampler_ts);
    let seconds_ago = if last == 0 {
        None
    } else {
        Some((now - last).max(0))
    };

    // A keep-awake radio owner explains paused sampling without implying failure.
    let radio_owner = read_radio_owner();
    let archiving = crate::drives_handler::is_archiving();
    let health_record = match read_at(std::path::Path::new(DEFAULT_HEALTH_PATH)) {
        Ok(record) => record,
        Err(e) => {
            tracing::warn!("could not read BLE health record; using freshness fallback: {e}");
            None
        }
    };
    let configured = health_is_configured(
        is_ble_enabled(),
        std::path::Path::new("/root/.ble/paired").exists(),
        last,
        health_record.is_some(),
    );
    let health = classify_health(&HealthInput {
        record: health_record,
        last_authenticated_success_ts: last,
        radio_owner: radio_owner.as_deref(),
        archiving,
        now_ts: now,
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "last_success_ts": last,
            "seconds_ago": seconds_ago,
            "sample_count_10min": sample_count_10min,
            "radio_owner": radio_owner,
            "archiving": archiving,
            "configured": configured,
            "health": health,
        })),
    )
}

/// Read the current keep-awake or telemetry radio owner.
fn read_radio_owner() -> Option<String> {
    let contents = std::fs::read_to_string("/tmp/ble_radio_owner").ok()?;
    let first = contents.lines().next()?.trim();
    if first.is_empty() { None } else { Some(first.to_string()) }
}

/// GET /api/system/ble-latest-sample, combining stored samples with the daemon's
/// live gate snapshot.
const GATE_STATUS_PATH: &str = "/mutable/sentryusb-ble-gate.txt";

/// Gate values and their own timestamp; database age is independent. Preserve
/// `unknown` to distinguish a failed read from no value.
fn parse_gate_status(
    body: &str,
) -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
    let mut sentry = None;
    let mut charging = None;
    let mut shift = None;
    let mut updated = None;
    for line in body.lines() {
        if let Some(v) = line.strip_prefix("sentry_mode=") {
            sentry = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("charging_state=") {
            charging = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("shift_state=") {
            shift = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("updated=") {
            updated = v.trim().parse::<i64>().ok();
        }
    }
    (sentry, charging, shift, updated)
}

fn read_gate_status() -> (Option<String>, Option<String>, Option<String>, Option<i64>) {
    let Ok(body) = std::fs::read_to_string(GATE_STATUS_PATH) else {
        return (None, None, None, None);
    };
    parse_gate_status(&body)
}

const DASHBOARD_SHIFT_FRESH_SECS: i64 = 120;

fn fresh_gate_shift_state(
    shift_state: Option<String>,
    updated_ts: Option<i64>,
    now_ts: i64,
) -> (Option<String>, Option<i64>) {
    let age = updated_ts.and_then(|updated| now_ts.checked_sub(updated));
    let fresh = age
        .map(|seconds| (0..=DASHBOARD_SHIFT_FRESH_SECS).contains(&seconds))
        .unwrap_or(false);
    (if fresh { shift_state } else { None }, age)
}

/// Latest-sample lookback. The timestamp bound keeps per-field reverse-index
/// searches finite for columns a vehicle never reports and excludes stale data.
const LATEST_SAMPLE_WINDOW_SECS: i64 = 30 * 86_400;

/// Cached sample data; age fields remain clock-derived per response.
type LatestSample = (
    i64,                  // envelope ts
    String,               // envelope source
    Option<(f64, i64)>,   // battery_pct + its row ts
    Option<(f64, i64)>,   // interior_temp_c + ts
    Option<(f64, i64)>,   // exterior_temp_c + ts
    Option<i64>,          // hvac_on
    Option<(f64, i64)>,   // tire_fl_psi + ts
    Option<f64>,          // tire_fr_psi
    Option<f64>,          // tire_rl_psi
    Option<f64>,          // tire_rr_psi
    Option<f64>,          // odometer_mi
    Option<String>,       // location_name
    // Coordinates remain internal; derive at_home against the current geofence.
    Option<f64>,          // latitude
    Option<f64>,          // longitude
    Option<i64>,          // body_controller ts
);

/// Cache below the sampler cadence to limit database reads from panel polling.
static LATEST_SAMPLE_CACHE: crate::ttl_cache::StaleWhileRevalidate<(), Option<LatestSample>> =
    crate::ttl_cache::StaleWhileRevalidate::new(std::time::Duration::from_secs(10));

pub async fn ble_latest_sample(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = s.drives.store.clone();
    let floor = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        - LATEST_SAMPLE_WINDOW_SECS;
    let result = LATEST_SAMPLE_CACHE
        .get((), move || {
        // The outer Option distinguishes read failure from a successful empty result.
        Some(store.with_read_conn(|conn| {
            // Pair an authenticated state-sample envelope with each field's
            // latest non-null value. Field ages expose older retained readings;
            // battery_temp_c is omitted because Tesla never supplies it here.
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
            // Return value timestamps so retained readings cannot appear current.
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
            // Recent body-controller traffic distinguishes expected Quiet mode
            // from complete BLE failure.
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
                    // Volatile fields carry their source-row timestamps.
                    latest_col_f64_aged("battery_pct"),
                    latest_col_f64_aged("interior_temp_c"),
                    latest_col_f64_aged("exterior_temp_c"),
                    latest_col_i64("hvac_on"),
                    // TPMS fields share one polling timestamp.
                    latest_col_f64_aged("tire_fl_psi"),
                    latest_col_f64("tire_fr_psi"),
                    latest_col_f64("tire_rl_psi"),
                    latest_col_f64("tire_rr_psi"),
                    latest_col_f64("odometer_mi"),
                    latest_col_string("location_name"),
                    latest_col_f64("latitude"),
                    latest_col_f64("longitude"),
                    body_controller_ts,
                )
            })
        }))
        })
        .await
        .flatten();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Gate state comes from the daemon file, not historical samples.
    let (sentry_mode, charging_state, shift_state, gate_updated_ts) = read_gate_status();
    let (shift_state, shift_state_seconds_ago) =
        fresh_gate_shift_state(shift_state, gate_updated_ts, now);

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
            latitude,
            longitude,
            body_controller_ts,
        )) => {
            // Field ages remain independent of the sample-envelope age.
            let age = |pair: &Option<(f64, i64)>| pair.map(|(_, t)| (now - t).max(0));
            // Return only a derived at-home boolean. Read the uncached geofence
            // only when a fix exists so moves apply on the next request.
            let at_home = latitude
                .zip(longitude)
                .and_then(|(la, lo)| {
                    home_geofence()
                        .map(|(hla, hlo, r)| distance_m(la, crate::normalize_lon(lo), hla, hlo) <= r)
                })
                .unwrap_or(false);
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
                "at_home": at_home,
                // Volatile dashboard-field ages.
                "field_secs_ago": {
                    "battery_pct": age(&battery_pct),
                    "interior_temp_c": age(&interior_temp_c),
                    "exterior_temp_c": age(&exterior_temp_c),
                    "tires": age(&tire_fl_psi),
                },
                "sentry_mode": sentry_mode,
                "charging_state": charging_state,
                "shift_state": shift_state,
                "shift_state_seconds_ago": shift_state_seconds_ago,
                "source": source,
                // Body-controller age distinguishes Quiet mode from total failure.
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

/// POST /api/system/ble-force-poll signals the sampler with SIGUSR1 for one
/// coordinated full-state poll. Delivery is best-effort; absence of a fresh
/// sample is the UI-visible failure signal.
pub async fn ble_force_poll(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Signal asynchronously and let normal polling reveal the result.
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

/// GET /api/system/ble-adapters returns sysfs adapters and current selection:
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
/// Source classification uses topology/manufacturer rather than unstable indexes.
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

/// Resolve BLE_ADAPTER with the same hci0 fallback as both daemons.
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

/// POST /api/system/ble-adapter validates a real hci sysfs entry, persists it,
/// and restarts both BLE services.
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
        // Root is read-only at rest.
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

    // Restarts are best-effort after a successful durable write; reboot also applies it.
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

/// POST /api/system/ble-install performs idempotent native keypair setup in the
/// background and emits `ble_install_status` progress.
pub async fn ble_install(
    State(s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Native binaries ship with the image; the keypair is the durable install state.
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
    use sentryusb_ble_health::{FaultKind, HealthRecord};

    #[test]
    fn body_controller_ping_does_not_count_as_authenticated_activity() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE telemetry_samples (ts INTEGER PRIMARY KEY, source TEXT NOT NULL);\
             INSERT INTO telemetry_samples (ts, source) VALUES (100, 'state');\
             INSERT INTO telemetry_samples (ts, source) VALUES (999, 'body_controller');",
        )
        .unwrap();
        assert_eq!(authenticated_activity(&conn, 900), (100, 0));
    }

    #[test]
    fn dashboard_health_is_only_configured_for_an_active_or_previously_paired_install() {
        assert!(!health_is_configured(false, true, 0, false));
        assert!(!health_is_configured(true, false, 0, false));
        assert!(health_is_configured(true, true, 0, false));
        assert!(health_is_configured(true, false, 100, false));
        assert!(health_is_configured(true, false, 0, true));
    }

    #[test]
    fn charging_controls_require_green_classified_health() {
        assert!(ble_health_allows_controls(true, None, 1_000, None, false, 1_030));
        assert!(!ble_health_allows_controls(true, None, 1_000, None, false, 1_060));
        assert!(!ble_health_allows_controls(false, None, 1_000, None, false, 1_030));
        assert!(!ble_health_allows_controls(
            true,
            Some(HealthRecord {
                fault: FaultKind::RepairRequired,
                since_ts: 1_020,
            }),
            1_030,
            None,
            false,
            1_040,
        ));
    }

    #[test]
    fn gate_status_parser_keeps_its_own_update_timestamp() {
        let parsed = parse_gate_status(
            "sentry_mode=Off\ncharging_state=Complete\nshift_state=Park\nupdated=1700000000\n",
        );
        assert_eq!(
            parsed,
            (
                Some("Off".to_string()),
                Some("Complete".to_string()),
                Some("Park".to_string()),
                Some(1_700_000_000),
            )
        );
    }

    #[test]
    fn gate_shift_freshness_suppresses_untrusted_timestamps() {
        assert_eq!(
            fresh_gate_shift_state(Some("Park".to_string()), Some(980), 1_000),
            (Some("Park".to_string()), Some(20)),
        );
        assert_eq!(
            fresh_gate_shift_state(Some("Drive".to_string()), Some(879), 1_000),
            (None, Some(121)),
        );
        assert_eq!(
            fresh_gate_shift_state(Some("Unknown".to_string()), None, 1_000),
            (None, None),
        );
        assert_eq!(
            fresh_gate_shift_state(Some("Park".to_string()), Some(1_001), 1_000),
            (None, Some(-1)),
        );
        assert_eq!(
            fresh_gate_shift_state(Some("Drive".to_string()), Some(i64::MIN), i64::MAX),
            (None, None),
        );
    }

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
        let c = cfg(&[("TESSIE_API_TOKEN", ""), ("TESLAFI_API_TOKEN", "   ")]);
        assert_eq!(conflicting_keep_awake_provider(&c), None);
    }

    #[test]
    fn bare_vin_alone_is_not_a_conflict() {
        let c = cfg(&[("TESLA_BLE_VIN", "5YJ3E1EA4JF000001")]);
        assert_eq!(conflicting_keep_awake_provider(&c), None);
    }
}
