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

pub(crate) const PREFS_FILE: &str = "/mutable/.sentryusb_preferences.json";
/// Legacy Go preferences path — read-only fallback so upgrades don't lose data.
pub(crate) const LEGACY_PREFS_FILE: &str = "/mutable/sentryusb-prefs.json";

/// Serializes concurrent preference reads + writes. Held around the
/// RMW in `set_preference` so interleaved PUTs can't lose updates.
static PREFS_LOCK: Mutex<()> = Mutex::new(());

pub(crate) fn load_prefs() -> serde_json::Map<String, serde_json::Value> {
    // Primary path first, legacy path as fallback.
    if let Ok(d) = std::fs::read_to_string(PREFS_FILE) {
        if let Ok(v) = serde_json::from_str(&d) {
            return v;
        }
    }
    std::fs::read_to_string(LEGACY_PREFS_FILE)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
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
    if let Some(parent) = std::path::Path::new(PREFS_FILE).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = format!("{}.tmp", PREFS_FILE);
    if let Err(e) = std::fs::write(&tmp, &data) {
        tracing::warn!("[preferences] failed to write tmp: {}", e);
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, PREFS_FILE) {
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
    State(_s): State<AppState>,
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

    // Hold the lock across the entire load→modify→save so two concurrent
    // PUTs serialize rather than racing on the same baseline snapshot.
    // Poisoned-guard recovery: treat `into_inner` as "lock was dropped
    // while held" — safe because we always restore the file from a
    // complete in-memory map on every save.
    let _guard = PREFS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut prefs = load_prefs();
    prefs.insert(req.key, req.value);
    save_prefs(&prefs);
    crate::json_ok()
}
