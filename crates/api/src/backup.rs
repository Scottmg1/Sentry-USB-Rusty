//! Config backup and restore.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;
use tracing::info;

use crate::router::AppState;

const BACKUP_DIR: &str = "/mutable/.config_backups";

#[derive(Serialize)]
struct BackupEntry {
    date: String,
    size: u64,
}

/// POST /api/system/backup
pub async fn create_backup(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let _ = std::fs::create_dir_all(BACKUP_DIR);
    let date = chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S").to_string();
    let backup_path = format!("{}/{}.conf", BACKUP_DIR, date);

    let config_path = sentryusb_config::find_config_path();
    match std::fs::copy(config_path, &backup_path) {
        Ok(_) => {
            info!("[backup] Created backup: {}", date);
            (StatusCode::OK, Json(serde_json::json!({"date": date})))
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Backup failed: {}", e)),
    }
}

/// GET /api/system/backups
pub async fn list_backups(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let mut backups = Vec::new();
    if let Ok(entries) = std::fs::read_dir(BACKUP_DIR) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".conf") {
                let date = name.trim_end_matches(".conf").to_string();
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                backups.push(BackupEntry { date, size });
            }
        }
    }
    backups.sort_by(|a, b| b.date.cmp(&a.date));
    (StatusCode::OK, Json(serde_json::to_value(backups).unwrap_or_default()))
}

/// GET /api/system/backup/{date}
pub async fn get_backup(
    State(_s): State<AppState>,
    Path(date): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Validate date to prevent path traversal
    if date.contains("..") || date.contains('/') || date.contains('\\') {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid date");
    }
    let path = format!("{}/{}.conf", BACKUP_DIR, date);
    match std::fs::read_to_string(&path) {
        Ok(content) => (StatusCode::OK, Json(serde_json::json!({"date": date, "content": content}))),
        Err(_) => crate::json_error(StatusCode::NOT_FOUND, "backup not found"),
    }
}

/// POST /api/system/restore
pub async fn restore_backup(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    #[derive(serde::Deserialize)]
    struct RestoreReq { date: String }

    let req: RestoreReq = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "invalid request"),
    };

    if req.date.contains("..") || req.date.contains('/') {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid date");
    }

    let backup_path = format!("{}/{}.conf", BACKUP_DIR, req.date);
    let config_path = sentryusb_config::find_config_path();

    match std::fs::copy(&backup_path, config_path) {
        Ok(_) => {
            info!("[backup] Restored from: {}", req.date);
            crate::json_ok()
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Restore failed: {}", e)),
    }
}
