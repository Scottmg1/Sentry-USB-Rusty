//! Notification center: history and type settings.
//!
//! Mirrors `server/api/notification_center.go`:
//! - History events carry id, unix-ts, type, title, message, providers,
//!   per-provider results.
//! - Newest-first ordering, max 500 entries.
//! - Query params: `limit`, `offset`, `type` (filter).
//! - Settings are stored in the user-preferences map with `notify_<type>` keys.
//! - Read fallback: primary `/mutable/sentryusb-notifications.json`, legacy
//!   `/mutable/.notification_history.json` (older Rust port wrote there).

use std::collections::HashMap;
use std::sync::RwLock;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::router::AppState;

const HISTORY_PATH: &str = "/mutable/sentryusb-notifications.json";
const LEGACY_HISTORY_PATH: &str = "/mutable/.notification_history.json";
const MAX_HISTORY: usize = 500;

static HISTORY_LOCK: RwLock<()> = RwLock::new(());

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct NotificationEvent {
    #[serde(default)]
    pub id: String,
    #[serde(rename = "ts", default)]
    pub timestamp: i64,
    #[serde(rename = "type", default)]
    pub event_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub providers: Vec<String>,
    #[serde(default)]
    pub results: HashMap<String, String>,
}

fn load_history_locked() -> Vec<NotificationEvent> {
    let raw = std::fs::read_to_string(HISTORY_PATH)
        .or_else(|_| std::fs::read_to_string(LEGACY_HISTORY_PATH))
        .unwrap_or_default();
    if raw.is_empty() {
        return Vec::new();
    }
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_history_locked(events: &[NotificationEvent]) -> std::io::Result<()> {
    let data = serde_json::to_vec_pretty(events)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    std::fs::write(HISTORY_PATH, data)
}

// --- Settings: toggle flags stored in preferences ---

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct NotificationSettings {
    pub archive_start: bool,
    pub archive_complete: bool,
    pub archive_error: bool,
    pub temperature: bool,
    #[serde(rename = "keep_awake_failure")]
    pub keep_awake: bool,
    pub update: bool,
    pub drives: bool,
    pub rtc_battery: bool,
    pub music_sync: bool,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        NotificationSettings {
            archive_start: true,
            archive_complete: true,
            archive_error: true,
            temperature: true,
            keep_awake: true,
            update: true,
            drives: true,
            rtc_battery: true,
            music_sync: true,
        }
    }
}

fn bool_pref(prefs: &serde_json::Map<String, serde_json::Value>, key: &str, default: bool) -> bool {
    match prefs.get(key) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s == "true",
        _ => default,
    }
}

fn load_settings() -> NotificationSettings {
    let prefs = crate::preferences::load_prefs();
    NotificationSettings {
        archive_start: bool_pref(&prefs, "notify_archive_start", true),
        archive_complete: bool_pref(&prefs, "notify_archive_complete", true),
        archive_error: bool_pref(&prefs, "notify_archive_error", true),
        temperature: bool_pref(&prefs, "notify_temperature", true),
        keep_awake: bool_pref(&prefs, "notify_keep_awake_failure", true),
        update: bool_pref(&prefs, "notify_update", true),
        drives: bool_pref(&prefs, "notify_drives", true),
        rtc_battery: bool_pref(&prefs, "notify_rtc_battery", true),
        music_sync: bool_pref(&prefs, "notify_music_sync", true),
    }
}

fn save_settings(s: &NotificationSettings) {
    let mut prefs = crate::preferences::load_prefs();
    let put = |prefs: &mut serde_json::Map<String, serde_json::Value>, k: &str, v: bool| {
        prefs.insert(k.to_string(), serde_json::Value::String(
            if v { "true".to_string() } else { "false".to_string() },
        ));
    };
    put(&mut prefs, "notify_archive_start", s.archive_start);
    put(&mut prefs, "notify_archive_complete", s.archive_complete);
    put(&mut prefs, "notify_archive_error", s.archive_error);
    put(&mut prefs, "notify_temperature", s.temperature);
    put(&mut prefs, "notify_keep_awake_failure", s.keep_awake);
    put(&mut prefs, "notify_update", s.update);
    put(&mut prefs, "notify_drives", s.drives);
    put(&mut prefs, "notify_rtc_battery", s.rtc_battery);
    put(&mut prefs, "notify_music_sync", s.music_sync);
    crate::preferences::save_prefs(&prefs);
}

/// GET /api/notifications/settings
pub async fn get_settings(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(serde_json::to_value(load_settings()).unwrap_or_default()))
}

/// PUT /api/notifications/settings
pub async fn update_settings(
    State(_s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let settings: NotificationSettings = match serde_json::from_str(&body) {
        Ok(s) => s,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "Invalid request body"),
    };
    save_settings(&settings);
    crate::json_ok()
}

// --- History ---

#[derive(Deserialize)]
pub struct HistoryQuery {
    #[serde(rename = "type")]
    pub event_type: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

/// GET /api/notifications/history
pub async fn get_history(
    State(_s): State<AppState>,
    Query(q): Query<HistoryQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _guard = HISTORY_LOCK.read().unwrap_or_else(|p| p.into_inner());
    let mut events = load_history_locked();
    if let Some(t) = &q.event_type {
        if !t.is_empty() {
            events.retain(|e| &e.event_type == t);
        }
    }
    let total = events.len();
    let limit = q.limit.unwrap_or(50);
    let offset = q.offset.unwrap_or(0);
    let page: Vec<NotificationEvent> = if offset >= events.len() {
        Vec::new()
    } else {
        let end = (offset + limit).min(events.len());
        events[offset..end].to_vec()
    };
    (StatusCode::OK, Json(serde_json::json!({
        "events": page,
        "total": total,
        "limit": limit,
        "offset": offset,
    })))
}

/// POST /api/notifications/history
pub async fn append_history(
    State(_s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut event: NotificationEvent = match serde_json::from_str(&body) {
        Ok(e) => e,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "Invalid event data"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    if event.timestamp == 0 {
        event.timestamp = now;
    }
    if event.id.is_empty() {
        // base36 of nanos — matches Go's strconv.FormatInt(…, 36).
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        event.id = to_base36(nanos);
    }

    let _guard = HISTORY_LOCK.write().unwrap_or_else(|p| p.into_inner());
    let mut events = load_history_locked();
    // Prepend (newest-first) and trim.
    events.insert(0, event.clone());
    if events.len() > MAX_HISTORY {
        events.truncate(MAX_HISTORY);
    }
    if let Err(e) = save_history_locked(&events) {
        return crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to save notification history: {}", e),
        );
    }
    (StatusCode::OK, Json(serde_json::to_value(event).unwrap_or_default()))
}

/// DELETE /api/notifications/history
pub async fn clear_history(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let _guard = HISTORY_LOCK.write().unwrap_or_else(|p| p.into_inner());
    if let Err(e) = save_history_locked(&[]) {
        return crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to clear notification history: {}", e),
        );
    }
    crate::json_ok()
}

/// DELETE /api/notifications/history/{id}
pub async fn delete_history_item(
    State(_s): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if id.is_empty() {
        return crate::json_error(StatusCode::BAD_REQUEST, "Missing notification ID");
    }
    let _guard = HISTORY_LOCK.write().unwrap_or_else(|p| p.into_inner());
    let mut events = load_history_locked();
    events.retain(|e| e.id != id);
    if let Err(e) = save_history_locked(&events) {
        return crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to save notification history: {}", e),
        );
    }
    crate::json_ok()
}

#[derive(Deserialize)]
pub struct CheckParams {
    #[serde(rename = "type")]
    pub notification_type: Option<String>,
}

/// GET /api/notifications/settings/check?type=archive_start
///
/// Go returns `{type, enabled}` reflecting the user's toggle state. We match
/// that contract (the prior Rust impl returned `{configured}` based on config
/// keys, which is a different question).
pub async fn check_notification_type(
    State(_s): State<AppState>,
    Query(params): Query<CheckParams>,
) -> (StatusCode, Json<serde_json::Value>) {
    let ntype = match params.notification_type.as_deref() {
        Some(s) if !s.is_empty() => s,
        _ => return crate::json_error(StatusCode::BAD_REQUEST, "Missing type parameter"),
    };
    let s = load_settings();
    let enabled = match ntype {
        "archive_start" => s.archive_start,
        "archive_complete" => s.archive_complete,
        "archive_error" => s.archive_error,
        "temperature" => s.temperature,
        "keep_awake_failure" => s.keep_awake,
        "update" => s.update,
        "drives" => s.drives,
        "rtc_battery" => s.rtc_battery,
        "music_sync" => s.music_sync,
        _ => true,
    };
    (StatusCode::OK, Json(serde_json::json!({"type": ntype, "enabled": enabled})))
}

fn to_base36(mut n: u64) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(ALPHABET[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}
