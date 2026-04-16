//! User preferences (key-value store).

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;

const PREFS_FILE: &str = "/mutable/.sentryusb_preferences.json";

fn load_prefs() -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(PREFS_FILE)
        .ok()
        .and_then(|d| serde_json::from_str(&d).ok())
        .unwrap_or_default()
}

fn save_prefs(prefs: &serde_json::Map<String, serde_json::Value>) {
    let _ = std::fs::write(PREFS_FILE, serde_json::to_string_pretty(prefs).unwrap_or_default());
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

    let mut prefs = load_prefs();
    prefs.insert(req.key, req.value);
    save_prefs(&prefs);
    crate::json_ok()
}
