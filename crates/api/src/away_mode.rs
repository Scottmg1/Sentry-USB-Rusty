//! Away Mode: WiFi AP control with timed expiration.
//!
//! Mirrors `server/api/awaymode.go`. Key behaviors restored:
//!  - RTC detection at startup (Pi 5 has /dev/rtc0); response includes `has_rtc`.
//!  - Persistent 30s countdown so Pis without an RTC recover accurately across
//!    reboots via `remaining_sec` in the flag file.
//!  - RestoreFromFile: on startup, resume the active session if time remains.
//!  - Response shape matches Go: {state, has_rtc, ap_ssid, ap_ip, expires_at,
//!    enabled_at, remaining_sec}.
//!  - AP connection profile name is `SENTRYUSB_AP` (Go's convention).

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
const POLL_INTERVAL: Duration = Duration::from_secs(30);
const MAX_DURATION_MIN: u64 = 24 * 60;

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
    state: &'static str, // "idle" | "active"
    has_rtc: bool,
    expires_at: Option<SystemTime>,
    enabled_at: Option<SystemTime>,
    stop: std::sync::Arc<Notify>,
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
            state: "idle",
            has_rtc,
            expires_at: None,
            enabled_at: None,
            stop: std::sync::Arc::new(Notify::new()),
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

fn start_ap_bg() {
    tokio::spawn(async {
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
    });
}

fn stop_ap_bg() {
    tokio::spawn(async {
        let _ = sentryusb_shell::run("nmcli", &["con", "down", AP_PROFILE]).await;
        away_mode_log("AP stopped");
        info!("[away-mode] AP stopped");
    });
}

async fn get_ap_info() -> (String, String) {
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
}

fn spawn_watcher(stop: std::sync::Arc<Notify>) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.notified() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            let expired = {
                let mut inner = mgr().lock().unwrap();
                if inner.state != "active" {
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
                    // Persist remaining_sec so no-RTC Pis recover after reboot.
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

/// Call at server startup. Resumes an active session if the flag file still
/// has time remaining.
pub fn restore_from_file() {
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
    let notify = {
        let mut inner = mgr().lock().unwrap();
        inner.state = "active";
        inner.enabled_at = Some(enabled_at);
        inner.expires_at = Some(SystemTime::now() + remaining);
        inner.stop = std::sync::Arc::new(Notify::new());
        inner.stop.clone()
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
    spawn_watcher(notify);
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
    // Verify the AP profile exists before we promise anything.
    if sentryusb_shell::run("nmcli", &["-t", "con", "show", AP_PROFILE])
        .await
        .map(|o| o.trim().is_empty())
        .unwrap_or(true)
    {
        return crate::json_error(
            StatusCode::PRECONDITION_FAILED,
            "AP not configured. Run setup with AP settings first.",
        );
    }

    let duration = Duration::from_secs(minutes * 60);
    enum Action {
        Extended,
        Started(std::sync::Arc<Notify>),
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
            write_flag_file(&inner);
            away_mode_log(&format!(
                "Enabled (duration: {}m, rtc: {})",
                minutes, inner.has_rtc
            ));
            info!("[away-mode] Enabled (duration: {}m)", minutes);
            (Action::Started(inner.stop.clone()), status_snapshot_sync(&inner))
        }
    };

    if let Action::Started(notify) = action {
        start_ap_bg();
        spawn_watcher(notify);
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
        if inner.state == "idle" {
            return crate::json_ok();
        }
        inner.stop.notify_waiters();
        inner.state = "idle";
        inner.expires_at = None;
        inner.enabled_at = None;
        remove_flag_file();
    }
    away_mode_log("Disabled by user");
    info!("[away-mode] Disabled by user");
    stop_ap_bg();
    crate::json_ok()
}

/// GET /api/away-mode/status
pub async fn status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let mut snap = {
        let inner = mgr().lock().unwrap();
        status_snapshot_sync(&inner)
    };
    let (ssid, ip) = get_ap_info().await;
    if !ssid.is_empty() {
        snap["ap_ssid"] = serde_json::Value::String(ssid);
        snap["ap_ip"] = serde_json::Value::String(ip);
    }
    (StatusCode::OK, Json(snap))
}

fn status_snapshot_sync(inner: &Inner) -> serde_json::Value {
    let mut v = serde_json::json!({
        "state": inner.state,
        "has_rtc": inner.has_rtc,
    });
    if inner.state == "active" {
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

// Unused helpers to silence dead-code warnings if added later.
#[allow(dead_code)]
fn _touch() {
    let _ = UNIX_EPOCH;
}
