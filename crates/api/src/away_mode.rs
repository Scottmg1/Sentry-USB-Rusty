//! Timed and geofence-controlled Wi-Fi access-point management.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::router::AppState;

const FLAG_FILE: &str = "/mutable/sentryusb_away_mode.json";
const AP_PROFILE: &str = "SENTRYUSB_AP";
const HOSTAPD_CONF: &str = "/etc/hostapd/hostapd.conf";
const IFUPDOWN_AP_FILE: &str = "/etc/network/interfaces.d/sentryusb-ap";
/// Default to 5 GHz channel 36 when the station is not associated.
const DEFAULT_AP_CHANNEL: u32 = 36;
const DEFAULT_AP_HW_MODE: &str = "a";

/// Detects whether Wi-Fi is managed by NetworkManager or ifupdown.
fn use_networkmanager() -> bool {
    std::process::Command::new("systemctl")
        .args(["--quiet", "is-active", "NetworkManager.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Waits through NetworkManager's early-boot activation before choosing a backend.
async fn wait_for_networkmanager() -> bool {
    // Do not delay genuine ifupdown systems.
    let nm_managed = std::process::Command::new("systemctl")
        .args(["--quiet", "is-enabled", "NetworkManager.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !nm_managed {
        return false;
    }
    // Allow NetworkManager up to 30 seconds to activate.
    for _ in 0..60 {
        if use_networkmanager() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    warn!("[away-mode] NetworkManager enabled but not active within timeout; using NM path anyway");
    true
}

/// Returns the station channel and band. Shared-radio chipsets require the AP
/// to use the same channel; an unassociated station returns `None`.
async fn detect_sta_channel() -> Option<(u32, &'static str)> {
    let out = sentryusb_shell::run("iw", &["wlan0", "info"]).await.ok()?;
    for line in out.lines() {
        let l = line.trim();
        let Some(rest) = l.strip_prefix("channel ") else { continue };
        let mut parts = rest.split_whitespace();
        let ch: u32 = parts.next()?.parse().ok()?;
        let freq_token = parts.next()?.trim_start_matches('(');
        let freq: u32 = freq_token.parse().ok()?;
        let hw_mode = if freq >= 5000 { "a" } else { "g" };
        return Some((ch, hw_mode));
    }
    None
}

/// Updates hostapd to match the station channel before bringing up `ap0`.
fn update_hostapd_channel(channel: u32, hw_mode: &str) -> Result<(), String> {
    // The helper is safe when root is already writable.
    let _ = std::process::Command::new("/root/bin/remountfs_rw").status();
    let content = std::fs::read_to_string(HOSTAPD_CONF)
        .map_err(|e| format!("read {}: {}", HOSTAPD_CONF, e))?;
    let mut new = String::new();
    let mut saw_channel = false;
    let mut saw_mode = false;
    for line in content.lines() {
        if line.starts_with("channel=") {
            new.push_str(&format!("channel={}", channel));
            saw_channel = true;
        } else if line.starts_with("hw_mode=") {
            new.push_str(&format!("hw_mode={}", hw_mode));
            saw_mode = true;
        } else {
            new.push_str(line);
        }
        new.push('\n');
    }
    if !saw_channel {
        new.push_str(&format!("channel={}\n", channel));
    }
    if !saw_mode {
        new.push_str(&format!("hw_mode={}\n", hw_mode));
    }
    std::fs::write(HOSTAPD_CONF, new)
        .map_err(|e| format!("write {}: {}", HOSTAPD_CONF, e))?;
    Ok(())
}
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_DURATION_MIN: u64 = 24 * 60;

/// Default home geofence radius (meters) when `AWAY_MODE_HOME_RADIUS_M`
/// is unset. Matches keep-accessory's default.
const DEFAULT_HOME_RADIUS_M: f64 = 120.0;
/// Stale GPS data holds the current AP state.
const STALE_FIX_SEC: i64 = 900;
/// Consecutive readings required before a home/away transition.
const CONFIRM_TICKS: u8 = 2;

#[derive(Serialize, Deserialize, Default, Clone)]
struct FlagData {
    #[serde(default)]
    expires_at: String,
    #[serde(default)]
    enabled_at: String,
    #[serde(default)]
    remaining_sec: i64,
    #[serde(default)]
    has_rtc: bool,
}

struct Inner {
    /// `manual` uses the timer; `auto` follows the geofence.
    mode: &'static str, // "manual" | "auto"
    state: &'static str, // "idle" | "active" (manual timer only)
    has_rtc: bool,
    expires_at: Option<SystemTime>,
    enabled_at: Option<SystemTime>,
    /// Last committed geofence decision; `None` means undecided.
    last_is_home: Option<bool>,
    /// Candidate awaiting confirmation.
    pending_is_home: Option<bool>,
    /// Consecutive candidate readings.
    pending_count: u8,
    stop: std::sync::Arc<Notify>,
    /// Watcher generation; prevents a missed notification from leaving duplicates.
    epoch: u64,
}

fn mgr() -> &'static Mutex<Inner> {
    static M: OnceLock<Mutex<Inner>> = OnceLock::new();
    M.get_or_init(|| {
        let has_rtc = std::path::Path::new("/dev/rtc0").exists();
        if has_rtc {
            info!("[away-mode] RTC detected — using timestamp-based expiration");
        } else {
            info!("[away-mode] No RTC — using countdown-based expiration");
        }
        Mutex::new(Inner {
            mode: "manual",
            state: "idle",
            has_rtc,
            expires_at: None,
            enabled_at: None,
            last_is_home: None,
            pending_is_home: None,
            pending_count: 0,
            stop: std::sync::Arc::new(Notify::new()),
            epoch: 0,
        })
    })
}

fn to_rfc3339(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn from_rfc3339(s: &str) -> Option<SystemTime> {
    DateTime::parse_from_rfc3339(s).ok().map(|d| d.with_timezone(&Utc).into())
}

fn remaining_seconds(expires: SystemTime) -> i64 {
    expires
        .duration_since(SystemTime::now())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn write_flag_file(inner: &Inner) {
    let expires = match inner.expires_at {
        Some(e) => e,
        None => return,
    };
    let enabled = inner.enabled_at.unwrap_or(SystemTime::now());
    let flag = FlagData {
        expires_at: to_rfc3339(expires),
        enabled_at: to_rfc3339(enabled),
        remaining_sec: remaining_seconds(expires),
        has_rtc: inner.has_rtc,
    };
    if let Ok(data) = serde_json::to_vec_pretty(&flag) {
        let tmp = format!("{}.tmp", FLAG_FILE);
        if std::fs::write(&tmp, &data).is_ok() {
            let _ = std::fs::rename(&tmp, FLAG_FILE);
        }
    }
}

fn remove_flag_file() {
    let _ = std::fs::remove_file(FLAG_FILE);
    let _ = std::fs::remove_file(format!("{}.tmp", FLAG_FILE));
}

fn away_mode_log(msg: &str) {
    use std::io::Write;
    const LOG_PATH: &str = "/mutable/archiveloop.log";
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(LOG_PATH)
    {
        let ts = chrono::Local::now().format("%a %e %b %H:%M:%S %Z %Y");
        let _ = writeln!(f, "{}: [away-mode] {}", ts, msg);
    }
}

// ─────────────────────────────────────────────────────────────────────
// Automatic geofence mode owns AP state; telemetry supplies GPS fixes.
// ─────────────────────────────────────────────────────────────────────

/// Great-circle distance in meters.
fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_R_M: f64 = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlambda / 2.0).sin().powi(2);
    // Guard the inverse-trigonometric domain against floating-point drift.
    2.0 * EARTH_R_M * a.sqrt().min(1.0).asin()
}

/// Maps distance into home/away/hold with a bounded hysteresis band.
fn band_is_home(dist_m: f64, radius_m: f64) -> Option<bool> {
    let margin = (0.15 * radius_m).clamp(15.0, 60.0);
    if dist_m < radius_m - margin {
        Some(true)
    } else if dist_m > radius_m + margin {
        Some(false)
    } else {
        None
    }
}

/// Confirms a banded reading across consecutive ticks before committing a flip.
fn fold_geofence(
    candidate: Option<bool>,
    last: Option<bool>,
    pending: &mut Option<bool>,
    pending_count: &mut u8,
) -> Option<bool> {
    let Some(cand) = candidate else {
        // A signal gap cancels partial confirmation.
        *pending = None;
        *pending_count = 0;
        return None;
    };
    if Some(cand) == last {
        // A confirmed-state reading cancels any pending flip.
        *pending = None;
        *pending_count = 0;
        return None;
    }
    if *pending == Some(cand) {
        *pending_count = pending_count.saturating_add(1);
    } else {
        *pending = Some(cand);
        *pending_count = 1;
    }
    if *pending_count >= CONFIRM_TICKS {
        *pending = None;
        *pending_count = 0;
        Some(cand)
    } else {
        None
    }
}

/// Seeds the decision from the flag: present means away, absent means home.
fn auto_seed_decision(flag_exists: bool) -> Option<bool> {
    Some(!flag_exists)
}

/// An away flag requires explicitly starting the AP during boot restore.
fn should_start_ap_on_boot(flag_exists: bool) -> bool {
    auto_seed_decision(flag_exists) == Some(false)
}

/// Returns the last GPS fix and its nonnegative age.
fn read_gps_fix() -> Option<(f64, f64, i64)> {
    let s = std::fs::read_to_string(crate::keep_accessory::gps_file()).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    let lat = v.get("lat").and_then(|x| x.as_f64())?;
    let lon = v.get("lon").and_then(|x| x.as_f64())?;
    let ts = v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0);
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some((lat, lon, (now - ts).max(0)))
}

/// Reads the shared home center and Away Mode-specific radius.
fn read_away_geofence() -> (Option<f64>, Option<f64>, f64) {
    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::parse_file(config_path) {
        Ok((active, commented)) => {
            let g = |k: &str| sentryusb_config::get_config_value(&active, &commented, k);
            let lat = g("KEEP_ACCESSORY_HOME_LAT").and_then(|s| s.trim().parse::<f64>().ok());
            // Normalize legacy world-copy longitudes.
            let lon = g("KEEP_ACCESSORY_HOME_LON")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .map(crate::normalize_lon);
            let radius = g("AWAY_MODE_HOME_RADIUS_M")
                .and_then(|s| s.trim().parse::<f64>().ok())
                .filter(|r| *r > 0.0)
                .unwrap_or(DEFAULT_HOME_RADIUS_M);
            (lat, lon, radius)
        }
        Err(_) => (None, None, DEFAULT_HOME_RADIUS_M),
    }
}

/// Reads the config-backed automatic-mode flag shared with telemetry.
fn read_auto_enabled_config() -> bool {
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = sentryusb_config::get_config_value(&active, &commented, "AWAY_MODE_AUTO_ENABLED") {
            return matches!(v.trim(), "yes" | "true" | "1");
        }
    }
    false
}

/// Persists automatic-mode AP state for the NetworkManager dispatcher.
fn write_auto_flag_file() {
    let body = serde_json::json!({
        "mode": "auto",
        "enabled_at": to_rfc3339(SystemTime::now()),
    });
    if let Ok(data) = serde_json::to_vec_pretty(&body) {
        let tmp = format!("{}.tmp", FLAG_FILE);
        if std::fs::write(&tmp, &data).is_ok() {
            let _ = std::fs::rename(&tmp, FLAG_FILE);
        }
    }
}

/// Evaluates one geofence tick and toggles the AP only after a confirmed flip.
async fn auto_eval_tick(my_epoch: u64) {
    let fix = read_gps_fix();
    let (home_lat, home_lon, radius_m) = read_away_geofence();

    // Missing/stale GPS or an unset home geofence holds state.
    let candidate = match (fix, home_lat, home_lon) {
        (Some((la, lo, age)), Some(hla), Some(hlo)) if age <= STALE_FIX_SEC => {
            band_is_home(distance_m(la, lo, hla, hlo), radius_m)
        }
        _ => None,
    };

    // Commit and schedule the toggle under one lock to serialize mode changes.
    let mut inner = mgr().lock().unwrap();
    if inner.mode != "auto" || inner.epoch != my_epoch {
        return; // mode changed or watcher superseded — don't touch the AP
    }
    let last = inner.last_is_home;
    let mut pending = inner.pending_is_home;
    let mut count = inner.pending_count;
    let decision = fold_geofence(candidate, last, &mut pending, &mut count);
    inner.pending_is_home = pending;
    inner.pending_count = count;
    if let Some(is_home) = decision {
        inner.last_is_home = Some(is_home);
        if is_home {
            // Remove the flag before stopping the AP to prevent resurrection.
            remove_flag_file();
            stop_ap_bg();
            away_mode_log("Automatic: at home — stopping AP (rejoining home WiFi)");
            info!("[away-mode] auto: home → AP off");
        } else {
            // Persist away state before starting the AP.
            write_auto_flag_file();
            start_ap_bg();
            away_mode_log("Automatic: away from home — starting AP");
            info!("[away-mode] auto: away → AP on");
        }
    }
}

/// Polls the geofence until mode or watcher generation changes.
fn spawn_auto_watcher(stop: std::sync::Arc<Notify>, my_epoch: u64) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.notified() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            {
                let inner = mgr().lock().unwrap();
                if inner.mode != "auto" || inner.epoch != my_epoch {
                    return;
                }
            }
            auto_eval_tick(my_epoch).await;
        }
    });
}

/// Finds the active or first managed Wi-Fi client device, excluding `ap0`.
async fn find_wlan_device() -> Option<String> {
    for cmd in [
        "nmcli -t -f TYPE,DEVICE c show --active | grep 802-11-wireless | grep -v ':ap0$' | cut -c17- | head -n1",
        "nmcli -t -f DEVICE,TYPE device status | grep ':wifi$' | grep -v '^ap0:' | cut -d: -f1 | head -n1",
    ] {
        if let Ok(out) = sentryusb_shell::run("bash", &["-c", cmd]).await {
            let dev = out.trim().to_string();
            if !dev.is_empty() {
                return Some(dev);
            }
        }
    }
    None
}

/// Creates the `ap0` virtual interface when absent.
async fn ensure_ap0() -> Result<(), String> {
    if sentryusb_shell::run("iw", &["dev", "ap0", "info"]).await.is_ok() {
        return Ok(());
    }
    let wlan = find_wlan_device()
        .await
        .ok_or_else(|| "no wifi client device found".to_string())?;
    sentryusb_shell::run("iw", &["dev", &wlan, "interface", "add", "ap0", "type", "__ap"])
        .await
        .map_err(|e| format!("creating ap0 on {}: {}", wlan, e))?;
    // Disable power saving on both sides of the shared radio.
    let _ = sentryusb_shell::run("iw", &[&wlan, "set", "power_save", "off"]).await;
    let _ = sentryusb_shell::run("iw", &["ap0", "set", "power_save", "off"]).await;
    Ok(())
}

/// Waits for NetworkManager to make the newly created `ap0` usable.
async fn wait_for_ap0_ready() -> bool {
    for _ in 0..20 {
        if let Ok(out) =
            sentryusb_shell::run("nmcli", &["-t", "-f", "DEVICE,STATE", "device", "status"]).await
        {
            if let Some(state) = out.lines().find_map(|l| l.strip_prefix("ap0:")) {
                let state = state.trim();
                if state.starts_with("disconnected") || state.starts_with("connected") {
                    return true;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

fn start_ap_bg() {
    tokio::spawn(async {
        if wait_for_networkmanager().await {
            if let Err(e) = ensure_ap0().await {
                away_mode_log(&format!("Failed to create ap0: {}", e));
                warn!("[away-mode] Failed to create ap0: {}", e);
            }
            // Activation races NetworkManager unless `ap0` is ready.
            if !wait_for_ap0_ready().await {
                away_mode_log(
                    "ap0 did not reach a ready state within timeout; attempting AP activation anyway",
                );
                warn!("[away-mode] ap0 not ready within timeout; activating anyway");
            }
            match sentryusb_shell::run("nmcli", &["con", "up", AP_PROFILE]).await {
                Ok(_) => {
                    away_mode_log("AP started");
                    info!("[away-mode] AP started");
                }
                Err(e) => {
                    away_mode_log(&format!("Failed to bring up AP: {}", e));
                    warn!("[away-mode] Failed to bring up AP: {}", e);
                }
            }
        } else {
            // Shared-radio chipsets require AP and station to share a channel.
            let (channel, hw_mode) = detect_sta_channel()
                .await
                .unwrap_or((DEFAULT_AP_CHANNEL, DEFAULT_AP_HW_MODE));
            if let Err(e) = update_hostapd_channel(channel, hw_mode) {
                away_mode_log(&format!("Failed to set hostapd channel: {}", e));
                warn!("[away-mode] update_hostapd_channel: {}", e);
            }
            match sentryusb_shell::run("ifup", &["ap0"]).await {
                Ok(_) => {
                    away_mode_log(&format!(
                        "AP started (ifupdown, channel {} {})",
                        channel, hw_mode
                    ));
                    info!(
                        "[away-mode] AP started via ifupdown channel={} hw_mode={}",
                        channel, hw_mode
                    );
                }
                Err(e) => {
                    away_mode_log(&format!("ifup ap0 failed: {}", e));
                    warn!("[away-mode] ifup ap0 failed: {}", e);
                }
            }
        }
    });
}

fn stop_ap_bg() {
    tokio::spawn(async {
        if use_networkmanager() {
            match sentryusb_shell::run("nmcli", &["con", "down", AP_PROFILE]).await {
                Ok(_) => {
                    away_mode_log("AP stopped");
                    info!("[away-mode] AP stopped");
                }
                Err(e) => {
                    // Already-down profiles also return an error.
                    away_mode_log(&format!("nmcli con down failed (AP may already be down): {}", e));
                    warn!("[away-mode] nmcli con down failed: {}", e);
                }
            }
        } else {
            // Stop daemons explicitly in case ifupdown hooks are missing.
            match sentryusb_shell::run("ifdown", &["ap0"]).await {
                Ok(_) => {
                    away_mode_log("AP stopped (ifupdown)");
                    info!("[away-mode] AP stopped via ifupdown");
                }
                Err(e) => {
                    away_mode_log(&format!("ifdown ap0 failed (AP may already be down): {}", e));
                    warn!("[away-mode] ifdown ap0 failed: {}", e);
                }
            }
            let _ = sentryusb_shell::run("systemctl", &["stop", "hostapd"]).await;
            let _ = sentryusb_shell::run("systemctl", &["stop", "dnsmasq"]).await;
        }
        // Remove `ap0` so the shared radio can scan normally.
        let _ = sentryusb_shell::run("iw", &["dev", "ap0", "del"]).await;
    });
}

/// Requires a complete AP profile, including both ifupdown files when applicable.
async fn ap_profile_exists() -> bool {
    if use_networkmanager() {
        sentryusb_shell::run("nmcli", &["-t", "con", "show", AP_PROFILE])
            .await
            .map(|o| !o.trim().is_empty())
            .unwrap_or(false)
    } else {
        std::path::Path::new(HOSTAPD_CONF).exists()
            && std::path::Path::new(IFUPDOWN_AP_FILE).exists()
    }
}

async fn get_ap_info() -> (String, String) {
    if use_networkmanager() {
        let out = match sentryusb_shell::run(
            "nmcli",
            &["-t", "-f", "802-11-wireless.ssid,ipv4.addresses", "con", "show", AP_PROFILE],
        )
        .await
        {
            Ok(o) => o,
            Err(_) => return (String::new(), String::new()),
        };
        let mut ssid = String::new();
        let mut ip = String::new();
        for line in out.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("802-11-wireless.ssid:") {
                ssid = rest.to_string();
            } else if let Some(rest) = line.strip_prefix("ipv4.addresses:") {
                ip = rest.to_string();
                if let Some(idx) = ip.find('/') {
                    ip.truncate(idx);
                }
            }
        }
        (ssid, ip)
    } else {
        // Read ifupdown configuration directly on the status hot path.
        let mut ssid = String::new();
        let mut ip = String::new();
        if let Ok(content) = std::fs::read_to_string(HOSTAPD_CONF) {
            for line in content.lines() {
                if let Some(rest) = line.strip_prefix("ssid=") {
                    ssid = rest.trim().to_string();
                    break;
                }
            }
        }
        if let Ok(content) = std::fs::read_to_string(IFUPDOWN_AP_FILE) {
            for line in content.lines() {
                let l = line.trim();
                if let Some(rest) = l.strip_prefix("address ") {
                    ip = rest.trim().to_string();
                    break;
                }
            }
        }
        (ssid, ip)
    }
}

fn spawn_watcher(stop: std::sync::Arc<Notify>, my_epoch: u64) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.notified() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            let expired = {
                let mut inner = mgr().lock().unwrap();
                if inner.state != "active" || inner.epoch != my_epoch {
                    return;
                }
                let expired = inner.expires_at.map_or(true, |e| SystemTime::now() >= e);
                if expired {
                    inner.state = "idle";
                    inner.expires_at = None;
                    inner.enabled_at = None;
                    remove_flag_file();
                    true
                } else {
                    // Persist the countdown for systems without a reliable RTC.
                    write_flag_file(&inner);
                    false
                }
            };
            if expired {
                away_mode_log("Timer expired, disabling");
                info!("[away-mode] Timer expired");
                stop_ap_bg();
                return;
            }
        }
    });
}

/// Restores automatic geofence state or a still-active manual timer at startup.
pub fn restore_from_file() {
    if read_auto_enabled_config() {
        let flag_exists = std::path::Path::new(FLAG_FILE).exists();
        let (notify, epoch) = {
            let mut inner = mgr().lock().unwrap();
            inner.mode = "auto";
            inner.state = "idle"; // auto doesn't use the timer state
            inner.expires_at = None;
            inner.enabled_at = None;
            // Match memory to the AP state restored by the dispatcher.
            inner.last_is_home = auto_seed_decision(flag_exists);
            inner.pending_is_home = None;
            inner.pending_count = 0;
            inner.stop = std::sync::Arc::new(Notify::new());
            inner.epoch += 1;
            (inner.stop.clone(), inner.epoch)
        };
        // Retain any persisted away decision.
        away_mode_log("Automatic Away Mode active on boot — geofence watcher started");
        info!("[away-mode] Automatic mode — geofence watcher started");
        // Away boots have no station-up event or geofence flip to start the AP.
        if should_start_ap_on_boot(flag_exists) {
            away_mode_log("Automatic: booted while away — starting AP");
            info!("[away-mode] auto: booted while away → starting AP");
            start_ap_bg();
        }
        spawn_auto_watcher(notify, epoch);
        return;
    }

    let data = match std::fs::read_to_string(FLAG_FILE) {
        Ok(d) => d,
        Err(_) => return,
    };
    let flag: FlagData = match serde_json::from_str(&data) {
        Ok(f) => f,
        Err(e) => {
            warn!("[away-mode] Invalid flag file, removing: {}", e);
            remove_flag_file();
            return;
        }
    };

    let has_rtc = { mgr().lock().unwrap().has_rtc };
    let remaining = if has_rtc {
        match from_rfc3339(&flag.expires_at) {
            Some(e) => e.duration_since(SystemTime::now()).unwrap_or(Duration::ZERO),
            None => {
                remove_flag_file();
                return;
            }
        }
    } else {
        Duration::from_secs(flag.remaining_sec.max(0) as u64)
    };

    if remaining.is_zero() {
        info!("[away-mode] Flag file expired, cleaning up");
        remove_flag_file();
        stop_ap_bg();
        return;
    }

    let enabled_at = from_rfc3339(&flag.enabled_at).unwrap_or_else(SystemTime::now);
    let (notify, epoch) = {
        let mut inner = mgr().lock().unwrap();
        inner.state = "active";
        inner.enabled_at = Some(enabled_at);
        inner.expires_at = Some(SystemTime::now() + remaining);
        inner.stop = std::sync::Arc::new(Notify::new());
        inner.epoch += 1;
        (inner.stop.clone(), inner.epoch)
    };

    away_mode_log(&format!(
        "Restored from flag file ({}s remaining, rtc: {})",
        remaining.as_secs(),
        has_rtc
    ));
    info!(
        "[away-mode] Restored from flag file ({}s remaining)",
        remaining.as_secs()
    );
    start_ap_bg();
    spawn_watcher(notify, epoch);
}

#[derive(Deserialize)]
pub struct EnableRequest {
    duration_min: Option<u64>,
}

/// POST /api/away-mode/enable
pub async fn enable(
    State(_s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let req: EnableRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "invalid request body"),
    };
    let minutes = req.duration_min.unwrap_or(0);
    if minutes == 0 {
        return crate::json_error(StatusCode::BAD_REQUEST, "duration_min must be positive");
    }
    if minutes > MAX_DURATION_MIN {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "duration_min cannot exceed 24 hours (1440)",
        );
    }
    // Manual and automatic AP ownership are mutually exclusive.
    let in_auto = { mgr().lock().unwrap().mode == "auto" };
    if in_auto {
        return crate::json_error(
            StatusCode::CONFLICT,
            "Away Mode is in Automatic mode — switch to Manual to use the timer.",
        );
    }
    // Do not acknowledge activation without a configured AP.
    if !ap_profile_exists().await {
        return crate::json_error(
            StatusCode::PRECONDITION_FAILED,
            "AP not configured. Run setup with AP settings first.",
        );
    }

    let duration = Duration::from_secs(minutes * 60);
    enum Action {
        Extended,
        Started(std::sync::Arc<Notify>, u64),
    }
    let (action, snap) = {
        let mut inner = mgr().lock().unwrap();
        if inner.state == "active" {
            inner.expires_at = Some(SystemTime::now() + duration);
            write_flag_file(&inner);
            away_mode_log(&format!("Extended (duration: {}m)", minutes));
            (Action::Extended, status_snapshot_sync(&inner))
        } else {
            inner.stop.notify_waiters();
            inner.state = "active";
            let now = SystemTime::now();
            inner.enabled_at = Some(now);
            inner.expires_at = Some(now + duration);
            inner.stop = std::sync::Arc::new(Notify::new());
            inner.epoch += 1;
            write_flag_file(&inner);
            away_mode_log(&format!(
                "Enabled (duration: {}m, rtc: {})",
                minutes, inner.has_rtc
            ));
            info!("[away-mode] Enabled (duration: {}m)", minutes);
            (
                Action::Started(inner.stop.clone(), inner.epoch),
                status_snapshot_sync(&inner),
            )
        }
    };

    if let Action::Started(notify, epoch) = action {
        start_ap_bg();
        spawn_watcher(notify, epoch);
    }

    let mut snap = snap;
    let (ssid, ip) = get_ap_info().await;
    if !ssid.is_empty() {
        snap["ap_ssid"] = serde_json::Value::String(ssid);
        snap["ap_ip"] = serde_json::Value::String(ip);
    }
    (StatusCode::OK, Json(snap))
}

/// POST /api/away-mode/disable
pub async fn disable(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    {
        let mut inner = mgr().lock().unwrap();
        // Automatic mode must be exited through the mode endpoint.
        if inner.mode == "auto" {
            return crate::json_ok();
        }
        if inner.state == "idle" {
            return crate::json_ok();
        }
        inner.stop.notify_waiters();
        inner.state = "idle";
        inner.expires_at = None;
        inner.enabled_at = None;
        inner.epoch += 1;
        remove_flag_file();
    }
    away_mode_log("Disabled by user");
    info!("[away-mode] Disabled by user");
    stop_ap_bg();
    crate::json_ok()
}

/// GET /api/away-mode/status
pub async fn status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let (mut snap, mode) = {
        let inner = mgr().lock().unwrap();
        (status_snapshot_sync(&inner), inner.mode)
    };
    let (ssid, ip) = get_ap_info().await;
    // A configured AP profile always has an SSID.
    snap["ap_configured"] = serde_json::Value::Bool(!ssid.is_empty());
    if !ssid.is_empty() {
        snap["ap_ssid"] = serde_json::Value::String(ssid);
        snap["ap_ip"] = serde_json::Value::String(ip);
    }
    // Automatic mode also exposes its geofence and GPS freshness.
    if mode == "auto" {
        let (home_lat, home_lon, radius) = read_away_geofence();
        snap["geofence_configured"] =
            serde_json::Value::Bool(home_lat.is_some() && home_lon.is_some());
        snap["home_lat"] = serde_json::json!(home_lat);
        snap["home_lon"] = serde_json::json!(home_lon);
        snap["home_radius_m"] = serde_json::json!(radius);
        snap["ble_ready"] = serde_json::Value::Bool(crate::ble::is_ble_enabled());
        match read_gps_fix() {
            Some((_, _, age)) => {
                snap["gps_stale"] = serde_json::Value::Bool(age > STALE_FIX_SEC);
                snap["last_fix_age_sec"] = serde_json::json!(age);
            }
            None => {
                snap["gps_stale"] = serde_json::Value::Bool(true);
                snap["last_fix_age_sec"] = serde_json::Value::Null;
            }
        }
    }
    (StatusCode::OK, Json(snap))
}

#[derive(Deserialize)]
pub struct ModeBody {
    mode: String,
}

/// Persists and applies a `manual` or `auto` mode transition.
pub async fn set_mode(
    State(_s): State<AppState>,
    Json(body): Json<ModeBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let want_auto = match body.mode.as_str() {
        "auto" => true,
        "manual" => false,
        other => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                &format!("invalid mode {other:?} (expected \"manual\" or \"auto\")"),
            );
        }
    };

    // Persist the mode before changing in-process ownership.
    let persist = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert(
            "AWAY_MODE_AUTO_ENABLED".to_string(),
            if want_auto { "yes" } else { "no" }.to_string(),
        );
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;
    match persist {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("config write failed: {e}"),
            );
        }
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("config task failed: {e}"),
            );
        }
    }

    if want_auto {
        let (notify, epoch) = {
            let mut inner = mgr().lock().unwrap();
            // Cancel the previous watcher generation.
            inner.stop.notify_waiters();
            inner.mode = "auto";
            inner.state = "idle";
            inner.expires_at = None;
            inner.enabled_at = None;
            inner.last_is_home = None;
            inner.pending_is_home = None;
            inner.pending_count = 0;
            inner.stop = std::sync::Arc::new(Notify::new());
            inner.epoch += 1;
            (inner.stop.clone(), inner.epoch)
        };
        away_mode_log("Switched to Automatic (geofence) mode");
        info!("[away-mode] mode → auto");
        spawn_auto_watcher(notify, epoch);
    } else {
        {
            let mut inner = mgr().lock().unwrap();
            // Leave manual mode idle with no watcher.
            inner.stop.notify_waiters();
            inner.mode = "manual";
            inner.state = "idle";
            inner.expires_at = None;
            inner.enabled_at = None;
            inner.last_is_home = None;
            inner.pending_is_home = None;
            inner.pending_count = 0;
            inner.epoch += 1;
            remove_flag_file();
        }
        away_mode_log("Switched to Manual mode — AP stopped");
        info!("[away-mode] mode → manual");
        stop_ap_bg();
    }

    let snap = {
        let inner = mgr().lock().unwrap();
        status_snapshot_sync(&inner)
    };
    (StatusCode::OK, Json(snap))
}

/// Returns the shared home center and Away Mode-specific radius.
pub async fn config_get(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let (home_lat, home_lon, home_radius_m) = read_away_geofence();
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "home_lat": home_lat,
            "home_lon": home_lon,
            "home_radius_m": home_radius_m,
        })),
    )
}

#[derive(Deserialize)]
pub struct AwayConfigBody {
    pub home_lat: Option<f64>,
    pub home_lon: Option<f64>,
    pub home_radius_m: Option<f64>,
}

/// Persists the shared home center and Away Mode-specific radius.
pub async fn config_set(
    State(_s): State<AppState>,
    Json(body): Json<AwayConfigBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // A center change invalidates charging's shared-home cache; radii are separate.
    let home_moving = body.home_lat.is_some() || body.home_lon.is_some();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        if let Some(lat) = body.home_lat.filter(|v| v.is_finite()) {
            let lat = lat.clamp(-90.0, 90.0);
            active.insert("KEEP_ACCESSORY_HOME_LAT".to_string(), format!("{lat:.6}"));
        }
        if let Some(lon) = body.home_lon.filter(|v| v.is_finite()) {
            // Normalize Leaflet world-copy longitudes.
            let lon = crate::normalize_lon(lon);
            active.insert("KEEP_ACCESSORY_HOME_LON".to_string(), format!("{lon:.6}"));
        }
        if let Some(r) = body.home_radius_m {
            // Match keep-accessory's 20 m–2 km bounds.
            let r = r.clamp(20.0, 2000.0).round() as i64;
            active.insert("AWAY_MODE_HOME_RADIUS_M".to_string(), r.to_string());
        }
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(())
    })
    .await;

    match result {
        Ok(Ok(())) => {
            info!("[away-mode] geofence config updated");
            if home_moving {
                crate::charging::invalidate_charging_list();
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_zero_for_same_point() {
        assert!(distance_m(37.5, -122.3, 37.5, -122.3) < 0.01);
    }

    #[test]
    fn distance_roughly_111km_per_degree_lat() {
        let d = distance_m(37.0, -122.0, 38.0, -122.0);
        assert!((d - 111_200.0).abs() < 1_000.0, "got {d}m");
    }

    #[test]
    fn band_inside_is_home_outside_is_away_middle_holds() {
        assert_eq!(band_is_home(50.0, 120.0), Some(true));
        assert_eq!(band_is_home(300.0, 120.0), Some(false));
        assert_eq!(band_is_home(120.0, 120.0), None);
        assert_eq!(band_is_home(110.0, 120.0), None);
    }

    #[test]
    fn fold_needs_two_ticks_to_commit() {
        let mut pending = None;
        let mut count = 0u8;
        assert_eq!(
            fold_geofence(Some(false), None, &mut pending, &mut count),
            None
        );
        assert_eq!(count, 1);
        assert_eq!(
            fold_geofence(Some(false), None, &mut pending, &mut count),
            Some(false)
        );
        assert_eq!(count, 0);
    }

    #[test]
    fn fold_holds_on_no_fix_and_clears_pending() {
        let mut pending = Some(false);
        let mut count = 1u8;
        assert_eq!(fold_geofence(None, Some(true), &mut pending, &mut count), None);
        assert_eq!(pending, None);
        assert_eq!(count, 0);
    }

    #[test]
    fn fold_no_op_when_already_in_state() {
        let mut pending = None;
        let mut count = 0u8;
        assert_eq!(
            fold_geofence(Some(true), Some(true), &mut pending, &mut count),
            None
        );
    }

    #[test]
    fn fold_jitter_does_not_commit() {
        let mut pending = None;
        let mut count = 0u8;
        let last = Some(true);
        assert_eq!(fold_geofence(Some(false), last, &mut pending, &mut count), None);
        assert_eq!(fold_geofence(Some(true), last, &mut pending, &mut count), None);
        assert_eq!(fold_geofence(Some(false), last, &mut pending, &mut count), None);
        assert_eq!(count, 1);
    }

    #[test]
    fn distance_is_finite_for_extreme_points() {
        for (a, b, c, d) in [
            (0.0, 0.0, 0.0, 180.0),
            (90.0, 0.0, -90.0, 0.0),
            (0.0, 0.0, 0.0, 0.0),
        ] {
            assert!(
                distance_m(a, b, c, d).is_finite(),
                "distance {a},{b} -> {c},{d} must be finite"
            );
        }
    }

    #[test]
    fn auto_seed_decision_reflects_flag_file() {
        assert_eq!(auto_seed_decision(true), Some(false));
        assert_eq!(auto_seed_decision(false), Some(true));
    }

    #[test]
    fn boot_starts_ap_only_when_away() {
        assert!(should_start_ap_on_boot(true));
        assert!(!should_start_ap_on_boot(false));
    }
}

fn status_snapshot_sync(inner: &Inner) -> serde_json::Value {
    let mut v = serde_json::json!({
        "mode": inner.mode,
        "state": inner.state,
        "has_rtc": inner.has_rtc,
    });
    if inner.mode == "auto" {
        // The AP is on only for a confirmed away decision.
        v["is_home"] = match inner.last_is_home {
            Some(h) => serde_json::Value::Bool(h),
            None => serde_json::Value::Null,
        };
        v["ap_on"] = serde_json::Value::Bool(matches!(inner.last_is_home, Some(false)));
    } else if inner.state == "active" {
        if let Some(exp) = inner.expires_at {
            v["expires_at"] = serde_json::Value::String(to_rfc3339(exp));
            v["remaining_sec"] =
                serde_json::Value::Number((remaining_seconds(exp).max(0)).into());
        }
        if let Some(en) = inner.enabled_at {
            v["enabled_at"] = serde_json::Value::String(to_rfc3339(en));
        }
    }
    v
}
