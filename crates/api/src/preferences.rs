//! User preferences (key-value store).
//!
//! Concurrency: the load→modify→save flow used by [`set_preference`] is
//! racy without a lock — two concurrent PUTs would both read the same
//! baseline, each insert their own key, and the second write would
//! silently clobber the first. Go guarded this with `prefsMu.RWMutex`;
//! we do the same here with a process-wide `Mutex<()>` held for the
//! duration of the RMW.
//!
//! Durability: saves go through tmp+rename so a power cut mid-write
//! can't leave the preferences file half-formed (parseable as empty,
//! losing every stored flag).

use std::sync::Mutex;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;

/// Preferences store path (`/mutable` on the Pi; honors the
/// `SENTRYUSB_MUTABLE_DIR` dev override for off-Pi runs).
pub(crate) fn prefs_file() -> String {
    format!("{}/.sentryusb_preferences.json", sentryusb_config::mutable_dir())
}
/// Legacy Go preferences path — read-only fallback so upgrades don't lose data.
fn legacy_prefs_file() -> String {
    format!("{}/sentryusb-prefs.json", sentryusb_config::mutable_dir())
}

/// Serializes concurrent preference reads + writes. Held around the
/// RMW in `set_preference` so interleaved PUTs can't lose updates.
static PREFS_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn load_prefs() -> serde_json::Map<String, serde_json::Value> {
    // Primary path first, legacy path as fallback.
    if let Ok(d) = std::fs::read_to_string(prefs_file()) {
        if let Ok(v) = serde_json::from_str(&d) {
            return v;
        }
    }
    std::fs::read_to_string(legacy_prefs_file())
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

/// Load → modify → save under [`PREFS_LOCK`], propagating write failures.
/// `f` returns whether it changed anything; nothing is written if it didn't.
/// Use over `save_prefs` (which only logs) when the caller must know the
/// write landed.
pub(crate) fn update_prefs<F>(f: F) -> anyhow::Result<bool>
where
    F: FnOnce(&mut serde_json::Map<String, serde_json::Value>) -> anyhow::Result<bool>,
{
    let _guard = PREFS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut prefs = load_prefs();
    if !f(&mut prefs)? {
        return Ok(false);
    }
    save_prefs_checked(&prefs)?;
    Ok(true)
}

/// Same write as [`save_prefs`] but the caller sees the failure.
fn save_prefs_checked(prefs: &serde_json::Map<String, serde_json::Value>) -> anyhow::Result<()> {
    let data = serde_json::to_string_pretty(prefs)?;
    let prefs_path = prefs_file();
    if let Some(parent) = std::path::Path::new(&prefs_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = format!("{}.tmp", prefs_path);
    std::fs::write(&tmp, &data)?;
    if let Err(e) = std::fs::rename(&tmp, &prefs_path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

pub(crate) fn save_prefs(prefs: &serde_json::Map<String, serde_json::Value>) {
    // Atomic tmp+rename — a direct `fs::write` leaves the file in an
    // intermediate zero-length state if the kernel panics mid-write,
    // which on next boot would silently reset every toggle (away-mode
    // notifications, update channel, etc.) to its default.
    //
    // On a fresh first install the wizard saves prefs (e.g. the new
    // community wraps/chimes flags) BEFORE the /mutable partition has
    // been created and mounted — at that point the parent directory
    // doesn't exist yet and the write fails with ENOENT, leaving a
    // noisy warning in journalctl. Pre-create the parent so the write
    // succeeds onto rootfs as a placeholder; once /mutable is mounted
    // any subsequent save lands on the persistent partition.
    let data = serde_json::to_string_pretty(prefs).unwrap_or_default();
    let prefs_path = prefs_file();
    if let Some(parent) = std::path::Path::new(&prefs_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = format!("{}.tmp", prefs_path);
    if let Err(e) = std::fs::write(&tmp, &data) {
        tracing::warn!("[preferences] failed to write tmp: {}", e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &prefs_path) {
        let _ = std::fs::remove_file(&tmp);
        tracing::warn!("[preferences] failed to rename into place: {}", e);
    }
}

#[derive(Deserialize)]
pub struct PrefQuery {
    key: Option<String>,
}

/// GET /api/config/preference
pub async fn get_preference(
    State(_s): State<AppState>,
    Query(params): Query<PrefQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let prefs = load_prefs();
    if let Some(key) = &params.key {
        let val = prefs.get(key).cloned().unwrap_or(serde_json::Value::Null);
        (StatusCode::OK, Json(serde_json::json!({"key": key, "value": val})))
    } else {
        (StatusCode::OK, Json(serde_json::Value::Object(prefs)))
    }
}

/// PUT /api/config/preference
pub async fn set_preference(
    State(s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    #[derive(Deserialize)]
    struct SetReq {
        key: String,
        value: serde_json::Value,
    }

    let req: SetReq = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "invalid request body"),
    };
    let key = req.key.clone();

    {
        // Hold the lock across the entire load→modify→save so two concurrent
        // PUTs serialize rather than racing on the same baseline snapshot.
        // Poisoned-guard recovery: treat `into_inner` as "lock was dropped
        // while held" — safe because we always restore the file from a
        // complete in-memory map on every save.
        let _guard = PREFS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut prefs = load_prefs();
        prefs.insert(req.key, req.value);
        save_prefs(&prefs);
    }

    // Rate-config keys feed cloud sync: queue the document for push
    // and wake the sweep loop. Pull-applied configs bypass this handler
    // (the sync engine writes prefs directly), so there's no echo loop.
    if RATE_CONFIG_KEYS.contains(&key.as_str()) {
        if let Err(e) = s.drives.store.mark_rate_config_dirty() {
            tracing::warn!("[preferences] mark_rate_config_dirty failed: {}", e);
        }
        s.cloud.uploader.nudge();
    }
    crate::json_ok()
}

/// The preference keys that make up the per-Pi charging rate-config
/// document synced to/from the cloud (charging.rs `RateConfig::load`
/// reads exactly these).
pub const RATE_CONFIG_KEYS: &[&str] =
    &["charging_currency", "charging_default_rate", "charging_tag_rates"];

/// `RateConfigAccess` over the local preferences store — wired into the
/// cloud uploader at spawn (main.rs). Reads/writes only the
/// RATE_CONFIG_KEYS subset, under the same PREFS_LOCK as the API.
pub struct PrefsRateConfig;

impl sentryusb_cloud_uploader::RateConfigAccess for PrefsRateConfig {
    fn load_doc(&self) -> serde_json::Value {
        let prefs = load_prefs();
        let mut doc = serde_json::Map::new();
        for k in RATE_CONFIG_KEYS {
            if let Some(v) = prefs.get(*k) {
                doc.insert((*k).to_string(), v.clone());
            }
        }
        serde_json::Value::Object(doc)
    }

    fn store_doc(&self, doc: &serde_json::Value) -> anyhow::Result<()> {
        let Some(obj) = doc.as_object() else {
            anyhow::bail!("rate config doc is not an object");
        };
        let _guard = PREFS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let mut prefs = load_prefs();
        for k in RATE_CONFIG_KEYS {
            match obj.get(*k) {
                Some(v) => {
                    prefs.insert((*k).to_string(), v.clone());
                }
                None => {
                    prefs.remove(*k);
                }
            }
        }
        save_prefs(&prefs);
        Ok(())
    }
}
