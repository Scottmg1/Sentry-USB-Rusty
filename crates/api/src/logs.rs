//! Log file viewer.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;

/// Known log files and their paths.
fn log_path(name: &str) -> Option<&'static str> {
    match name {
        "archiveloop" => Some("/mutable/archiveloop.log"),
        "syslog" => Some("/var/log/syslog"),
        "kern" => Some("/var/log/kern.log"),
        "auth" => Some("/var/log/auth.log"),
        "daemon" => Some("/var/log/daemon.log"),
        "sentryusb" => Some("/var/log/sentryusb.log"),
        "sentryusb-ble" => Some("/var/log/sentryusb-ble.log"),
        _ => None,
    }
}

#[derive(Deserialize)]
pub struct LogQuery {
    lines: Option<usize>,
}

/// GET /api/logs/{name}
pub async fn get_log(
    State(_s): State<AppState>,
    Path(name): Path<String>,
    Query(params): Query<LogQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Validate log name (prevent path traversal)
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid log name");
    }

    let path = match log_path(&name) {
        Some(p) => p.to_string(),
        None => format!("/var/log/{}", name),
    };

    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let max_lines = params.lines.unwrap_or(1000);
            let lines: Vec<&str> = content.lines().collect();
            let start = lines.len().saturating_sub(max_lines);
            let tail = lines[start..].join("\n");
            (StatusCode::OK, Json(serde_json::json!({"log": tail, "total_lines": lines.len()})))
        }
        Err(e) => crate::json_error(StatusCode::NOT_FOUND, &format!("log not found: {}", e)),
    }
}
