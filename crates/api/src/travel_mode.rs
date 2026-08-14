//! Travel Mode: keep the USB gadget presented to the car at all times.
//!
//! The archive loop may snapshot and upload over a travel connection while
//! skipping gadget disconnection, preserving continuous recording.
//!
//! Three flags are persisted in `sentryusb.conf`:
//!
//! * `TRAVEL_MODE_ENABLED` (`yes`/`no`) — the master toggle.
//! * `TRAVEL_MODE_HALF_SNAPSHOTS` (`yes`/`no`) — pace travel-mode
//!   snapshot+archive cycles at half the user's `SNAPSHOT_INTERVAL`
//!   to reduce the window before footage is captured.
//! * `TRAVEL_MODE_FAST_RETRY` (`yes`/`no`) — after a failed archive cycle,
//!   retry in about one minute instead of the full interval.
//!
//! Archiveloop rereads these settings each cycle; no restart is required.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;

/// Fallback when `SNAPSHOT_INTERVAL` is absent from the config — must match
/// archiveloop's `${SNAPSHOT_INTERVAL:-3480}` default.
const DEFAULT_SNAPSHOT_INTERVAL_SEC: u64 = 3480;

#[derive(Debug, PartialEq)]
struct TravelSettings {
    enabled: bool,
    half_snapshots: bool,
    fast_retry: bool,
    /// The user's configured snapshot interval — the UI shows this as the
    /// "default" cadence and `interval / 2` as the "half" cadence.
    snapshot_interval_sec: u64,
}

fn flag_is_on(v: &str) -> bool {
    matches!(v.trim(), "yes" | "true" | "1")
}

/// Parse the Travel Mode settings from a specific config file. Split out
/// from [`read_settings`] so it can be unit-tested against a temp conf.
fn read_settings_at(config_path: &str) -> TravelSettings {
    let mut settings = TravelSettings {
        enabled: false,
        half_snapshots: false,
        fast_retry: false,
        snapshot_interval_sec: DEFAULT_SNAPSHOT_INTERVAL_SEC,
    };
    if let Ok((active, commented)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = sentryusb_config::get_config_value(&active, &commented, "TRAVEL_MODE_ENABLED") {
            settings.enabled = flag_is_on(&v);
        }
        if let Some(v) = sentryusb_config::get_config_value(&active, &commented, "TRAVEL_MODE_HALF_SNAPSHOTS") {
            settings.half_snapshots = flag_is_on(&v);
        }
        if let Some(v) = sentryusb_config::get_config_value(&active, &commented, "TRAVEL_MODE_FAST_RETRY") {
            settings.fast_retry = flag_is_on(&v);
        }
        if let Some(v) = sentryusb_config::get_config_value(&active, &commented, "SNAPSHOT_INTERVAL") {
            if let Ok(n) = v.trim().parse::<u64>() {
                if n > 0 {
                    settings.snapshot_interval_sec = n;
                }
            }
        }
    }
    settings
}

/// Travel Mode settings from the active config file.
fn read_settings() -> TravelSettings {
    read_settings_at(sentryusb_config::find_config_path())
}

fn settings_json(s: &TravelSettings) -> serde_json::Value {
    serde_json::json!({
        "enabled": s.enabled,
        "half_snapshots": s.half_snapshots,
        "fast_retry": s.fast_retry,
        "snapshot_interval_sec": s.snapshot_interval_sec,
    })
}

/// GET /api/travel-mode/status →
/// `{"enabled": bool, "half_snapshots": bool, "fast_retry": bool, "snapshot_interval_sec": u64}`.
pub async fn status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(settings_json(&read_settings())))
}

#[derive(Deserialize)]
pub struct TravelBody {
    enabled: bool,
    /// If absent, preserve the persisted cadence flag.
    #[serde(default)]
    half_snapshots: Option<bool>,
    /// If absent, preserve the persisted fast-retry flag.
    #[serde(default)]
    fast_retry: Option<bool>,
}

/// POST /api/travel-mode — body
/// `{"enabled": bool, "half_snapshots"?: bool, "fast_retry"?: bool}`.
///
/// Persist flags for archiveloop's next cycle, remounting the read-only root
/// filesystem for the write.
pub async fn set(
    State(_s): State<AppState>,
    Json(body): Json<TravelBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let want = body.enabled;
    let want_half = body.half_snapshots;
    let want_fast_retry = body.fast_retry;

    let persist = tokio::task::spawn_blocking(move || -> anyhow::Result<TravelSettings> {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = sentryusb_config::parse_file(config_path)?;
        active.insert(
            "TRAVEL_MODE_ENABLED".to_string(),
            if want { "yes" } else { "no" }.to_string(),
        );
        if let Some(half) = want_half {
            active.insert(
                "TRAVEL_MODE_HALF_SNAPSHOTS".to_string(),
                if half { "yes" } else { "no" }.to_string(),
            );
        }
        if let Some(fast) = want_fast_retry {
            active.insert(
                "TRAVEL_MODE_FAST_RETRY".to_string(),
                if fast { "yes" } else { "no" }.to_string(),
            );
        }
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        sentryusb_config::write_file(config_path, &active)?;
        Ok(read_settings_at(config_path))
    })
    .await;

    match persist {
        Ok(Ok(settings)) => {
            let mut body = settings_json(&settings);
            body["ok"] = serde_json::Value::Bool(true);
            (StatusCode::OK, Json(body))
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
    use std::io::Write;

    fn temp_conf(contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sentryusb-travel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sentryusb.conf");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    #[test]
    fn reads_yes_as_enabled() {
        let p = temp_conf("export TRAVEL_MODE_ENABLED=yes\n");
        assert!(read_settings_at(p.to_str().unwrap()).enabled);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_no_as_disabled() {
        let p = temp_conf("export TRAVEL_MODE_ENABLED=no\n");
        assert!(!read_settings_at(p.to_str().unwrap()).enabled);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_unset_as_disabled() {
        let p = temp_conf("export SOMETHING_ELSE=1\n");
        let s = read_settings_at(p.to_str().unwrap());
        assert!(!s.enabled);
        assert!(!s.half_snapshots);
        assert!(!s.fast_retry);
        assert_eq!(s.snapshot_interval_sec, DEFAULT_SNAPSHOT_INTERVAL_SEC);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_fast_retry() {
        let p = temp_conf("export TRAVEL_MODE_ENABLED=yes\nexport TRAVEL_MODE_FAST_RETRY=yes\n");
        let s = read_settings_at(p.to_str().unwrap());
        assert!(s.enabled);
        assert!(s.fast_retry);
        let p2 = temp_conf("export TRAVEL_MODE_FAST_RETRY=no\n");
        assert!(!read_settings_at(p2.to_str().unwrap()).fast_retry);
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }

    #[test]
    fn quoted_value_is_parsed() {
        let p = temp_conf("export TRAVEL_MODE_ENABLED='yes'\n");
        assert!(read_settings_at(p.to_str().unwrap()).enabled);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn reads_half_snapshots_and_interval() {
        let p = temp_conf(
            "export TRAVEL_MODE_ENABLED=yes\nexport TRAVEL_MODE_HALF_SNAPSHOTS=yes\nexport SNAPSHOT_INTERVAL=480\n",
        );
        let s = read_settings_at(p.to_str().unwrap());
        assert!(s.enabled);
        assert!(s.half_snapshots);
        assert_eq!(s.snapshot_interval_sec, 480);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bad_interval_falls_back_to_default() {
        let p = temp_conf("export SNAPSHOT_INTERVAL=banana\n");
        assert_eq!(
            read_settings_at(p.to_str().unwrap()).snapshot_interval_sec,
            DEFAULT_SNAPSHOT_INTERVAL_SEC
        );
        let p2 = temp_conf("export SNAPSHOT_INTERVAL=0\n");
        assert_eq!(
            read_settings_at(p2.to_str().unwrap()).snapshot_interval_sec,
            DEFAULT_SNAPSHOT_INTERVAL_SEC
        );
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(&p2);
    }
}
