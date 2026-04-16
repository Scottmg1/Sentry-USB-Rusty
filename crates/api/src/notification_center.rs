//! Notification center: history and type settings.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::router::AppState;

const SETTINGS_FILE: &str = "/mutable/.notification_settings.json";
const HISTORY_FILE: &str = "/mutable/.notification_history.json";

#[derive(Serialize, Deserialize, Clone)]
struct HistoryEntry {
    id: String,
    #[serde(rename = "type")]
    entry_type: String,
    title: String,
    message: String,
    timestamp: String,
}

fn load_history() -> Vec<HistoryEntry> {
    std::fs::read_to_string(HISTORY_FILE)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

fn save_history(history: &[HistoryEntry]) {
    let _ = std::fs::write(HISTORY_FILE, serde_json::to_string_pretty(history).unwrap_or_default());
}

/// GET /api/notifications/settings
pub async fn get_settings(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let data = std::fs::read_to_string(SETTINGS_FILE).unwrap_or_else(|_| "{}".to_string());
    let settings: serde_json::Value = serde_json::from_str(&data).unwrap_or_default();
    (StatusCode::OK, Json(settings))
}

/// PUT /api/notifications/settings
pub async fn update_settings(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    let _ = std::fs::write(SETTINGS_FILE, &body);
    crate::json_ok()
}

/// GET /api/notifications/history
pub async fn get_history(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let history = load_history();
    (StatusCode::OK, Json(serde_json::to_value(history).unwrap_or_default()))
}

/// POST /api/notifications/history
pub async fn append_history(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    let entry: HistoryEntry = match serde_json::from_str(&body) {
        Ok(e) => e,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "invalid entry"),
    };
    let mut history = load_history();
    history.push(entry);
    // Keep last 500 entries
    if history.len() > 500 {
        history.drain(..history.len() - 500);
    }
    save_history(&history);
    crate::json_ok()
}

/// DELETE /api/notifications/history
pub async fn clear_history(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    save_history(&[]);
    crate::json_ok()
}

/// DELETE /api/notifications/history/{id}
pub async fn delete_history_item(
    State(_s): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut history = load_history();
    history.retain(|e| e.id != id);
    save_history(&history);
    crate::json_ok()
}

#[derive(Deserialize)]
pub struct CheckParams {
    #[serde(rename = "type")]
    notification_type: Option<String>,
}

/// GET /api/notifications/settings/check
pub async fn check_notification_type(
    State(_s): State<AppState>,
    Query(params): Query<CheckParams>,
) -> (StatusCode, Json<serde_json::Value>) {
    let ntype = params.notification_type.as_deref().unwrap_or("");

    // Check if the notification provider is configured
    let config_path = sentryusb_config::find_config_path();
    let configured = match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => {
            match ntype {
                "pushover" => active.contains_key("PUSHOVER_USER_KEY"),
                "discord" => active.contains_key("DISCORD_WEBHOOK_URL"),
                "telegram" => active.contains_key("TELEGRAM_BOT_TOKEN"),
                "slack" => active.contains_key("SLACK_WEBHOOK_URL"),
                "gotify" => active.contains_key("GOTIFY_URL"),
                "ntfy" => active.contains_key("NTFY_TOPIC"),
                "ifttt" => active.contains_key("IFTTT_WEBHOOK_KEY"),
                "webhook" => active.contains_key("WEBHOOK_URL"),
                _ => false,
            }
        }
        Err(_) => false,
    };

    (StatusCode::OK, Json(serde_json::json!({"configured": configured})))
}
