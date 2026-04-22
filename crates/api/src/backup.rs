//! Config backup and restore.
//!
//! Ported from `server/api/backup.go`. A backup is a JSON envelope containing
//! the `sentryusb.conf` contents plus the user preferences, SSH keys, rclone
//! config, Tesla BLE pairing keys, and notification-device credentials — the
//! stuff the user doesn't want to re-set up after an SD-card reflash. Change
//! detection via SHA-256 hash avoids filling the backup dir with identical
//! copies.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::router::AppState;

const LOCAL_BACKUP_DIR: &str = "/mutable/backups";
const ARCHIVE_BACKUP_DIR: &str = "/mnt/archive/backups";
const LAST_HASH_FILE: &str = "/mutable/backups/.last_hash";
const BACKUP_VERSION: u32 = 1;

// Paths matching Go's `createBackupData` (backup.go:72-99).
const SSH_PRIVATE_KEY: &str = "/root/.ssh/id_rsa";
const SSH_PUBLIC_KEY: &str = "/root/.ssh/id_rsa.pub";
const RCLONE_CONFIG: &str = "/root/.config/rclone/rclone.conf";
const BLE_PRIVATE_KEY: &str = "/root/.ble/key_private.pem";
const BLE_PUBLIC_KEY: &str = "/root/.ble/key_public.pem";
const NOTIFICATION_CREDS: &str = "/root/.sentryusb/notification-credentials.json";

#[derive(Serialize, Deserialize, Default)]
struct BackupData {
    version: u32,
    date: String,
    timestamp: String,
    hostname: String,
    config: String,
    #[serde(default)]
    preferences: HashMap<String, String>,
    #[serde(default)]
    drive_data_included: bool,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    ssh_private_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ssh_public_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    rclone_config: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ble_private_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    ble_public_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    notification_credentials: String,
}

#[derive(Serialize)]
struct BackupEntry {
    date: String,
    timestamp: String,
    location: String,
    size: u64,
    filename: String,
}

fn backup_filename(date: &str) -> String {
    format!("sentryusb-backup-{}.json", date)
}

fn read_file_if_exists(path: &str) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// Flatten the preferences Map<String, Value> to Map<String, String>, matching
/// Go's `map[string]string`. JSON values stringify via their literal form for
/// primitives; objects/arrays are serialized.
fn prefs_as_strings() -> HashMap<String, String> {
    let prefs = crate::preferences::load_prefs();
    let mut out = HashMap::with_capacity(prefs.len());
    for (k, v) in prefs {
        let s = match v {
            serde_json::Value::String(s) => s,
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        };
        out.insert(k, s);
    }
    out
}

async fn build_backup_data_async() -> Result<BackupData, String> {
    let config_path = sentryusb_config::find_config_path();
    let config = std::fs::read_to_string(config_path)
        .map_err(|e| format!("failed to read config: {}", e))?;
    let hostname = sentryusb_shell::run("hostname", &[])
        .await
        .unwrap_or_default()
        .trim()
        .to_string();
    let now = chrono::Utc::now();
    Ok(BackupData {
        version: BACKUP_VERSION,
        date: now.format("%Y-%m-%d").to_string(),
        timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        hostname,
        config,
        preferences: prefs_as_strings(),
        drive_data_included: false,
        ssh_private_key: read_file_if_exists(SSH_PRIVATE_KEY),
        ssh_public_key: read_file_if_exists(SSH_PUBLIC_KEY),
        rclone_config: read_file_if_exists(RCLONE_CONFIG),
        ble_private_key: read_file_if_exists(BLE_PRIVATE_KEY),
        ble_public_key: read_file_if_exists(BLE_PUBLIC_KEY),
        notification_credentials: read_file_if_exists(NOTIFICATION_CREDS),
    })
}

/// Hex SHA-256 of all backup-relevant data with time-varying fields excluded
/// so the hash is stable across identical snapshots. Preferences are sorted
/// by key so hashing order is deterministic. Mirrors Go `computeBackupHash`.
fn compute_backup_hash(data: &BackupData) -> String {
    use ring::digest::{Context, SHA256};
    let mut ctx = Context::new(&SHA256);
    ctx.update(data.config.as_bytes());
    let mut keys: Vec<&String> = data.preferences.keys().collect();
    keys.sort();
    for k in keys {
        ctx.update(k.as_bytes());
        if let Some(v) = data.preferences.get(k) {
            ctx.update(v.as_bytes());
        }
    }
    ctx.update(data.ssh_private_key.as_bytes());
    ctx.update(data.ssh_public_key.as_bytes());
    ctx.update(data.rclone_config.as_bytes());
    ctx.update(data.ble_private_key.as_bytes());
    ctx.update(data.ble_public_key.as_bytes());
    ctx.update(data.notification_credentials.as_bytes());
    hex::encode(ctx.finish().as_ref())
}

fn read_last_hash() -> String {
    std::fs::read_to_string(LAST_HASH_FILE)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn write_last_hash(hash: &str) {
    if let Some(dir) = Path::new(LAST_HASH_FILE).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(LAST_HASH_FILE, format!("{}\n", hash));
}

fn write_backup_to_dir(dir: &str, data: &BackupData) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("failed to create backup dir {}: {}", dir, e))?;
    let filename = backup_filename(&data.date);
    let path = format!("{}/{}", dir.trim_end_matches('/'), filename);
    let tmp = format!("{}.tmp", path);
    let json_bytes = serde_json::to_vec_pretty(data)
        .map_err(|e| format!("failed to marshal backup: {}", e))?;
    std::fs::write(&tmp, &json_bytes)
        .map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("failed to write backup: {}", e) })?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("failed to finalize backup: {}", e))?;
    info!("[backup] Wrote backup to {} ({} bytes)", path, json_bytes.len());
    Ok(())
}

async fn sync_backup_to_rsync(data: &BackupData) -> Result<(), String> {
    let config_path = sentryusb_config::find_config_path();
    let (active, _) = sentryusb_config::parse_file(config_path)
        .map_err(|e| e.to_string())?;
    let server = active.get("RSYNC_SERVER").cloned().unwrap_or_default();
    let user = active.get("RSYNC_USER").cloned().unwrap_or_default();
    let rsync_path = active.get("RSYNC_PATH").cloned().unwrap_or_default();
    if server.is_empty() || user.is_empty() {
        return Err("rsync not configured".to_string());
    }

    let tmp_dir = "/tmp/sentryusb-backup-sync";
    let _ = std::fs::create_dir_all(tmp_dir);
    let filename = backup_filename(&data.date);
    let tmp_path = format!("{}/{}", tmp_dir, filename);
    let json_bytes = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
    std::fs::write(&tmp_path, &json_bytes).map_err(|e| e.to_string())?;

    // Ensure remote backups/ dir exists. Best-effort.
    let user_at_server = format!("{}@{}", user, server);
    let remote_dir = format!("{}/backups", rsync_path);
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(10), "ssh",
        &[
            "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=no", "-o", "BatchMode=yes",
            &user_at_server, "mkdir", "-p", &remote_dir,
        ],
    ).await;

    let dest = format!("{}@{}:{}/backups/{}", user, server, rsync_path, filename);
    let res = sentryusb_shell::run_with_timeout(
        Duration::from_secs(60), "rsync",
        &["-avh", "--no-perms", "--omit-dir-times", "--timeout=60", &tmp_path, &dest],
    ).await;
    let _ = std::fs::remove_file(&tmp_path);
    res.map(|_| ()).map_err(|e| e.to_string())
}

async fn sync_backup_to_rclone(data: &BackupData) -> Result<(), String> {
    let config_path = sentryusb_config::find_config_path();
    let (active, _) = sentryusb_config::parse_file(config_path)
        .map_err(|e| e.to_string())?;
    let drive = active.get("RCLONE_DRIVE").cloned().unwrap_or_default();
    let rclone_path = active.get("RCLONE_PATH").cloned().unwrap_or_default();
    if drive.is_empty() {
        return Err("rclone not configured".to_string());
    }

    let tmp_dir = "/tmp/sentryusb-backup-sync";
    let _ = std::fs::create_dir_all(tmp_dir);
    let filename = backup_filename(&data.date);
    let tmp_path = format!("{}/{}", tmp_dir, filename);
    let json_bytes = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
    std::fs::write(&tmp_path, &json_bytes).map_err(|e| e.to_string())?;

    let dest = format!("{}:{}/backups/", drive, rclone_path);
    let res = sentryusb_shell::run_with_timeout(
        Duration::from_secs(60), "rclone",
        &["--config", "/root/.config/rclone/rclone.conf", "copy", &tmp_path, &dest],
    ).await;
    let _ = std::fs::remove_file(&tmp_path);
    res.map(|_| ()).map_err(|e| e.to_string())
}

fn list_backups_in_dir(dir: &str, location: &str) -> Vec<BackupEntry> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("sentryusb-backup-") || !name.ends_with(".json") {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        let path = format!("{}/{}", dir.trim_end_matches('/'), name);
        let raw = match std::fs::read_to_string(&path) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let bd: BackupData = match serde_json::from_str(&raw) {
            Ok(b) => b,
            Err(_) => continue,
        };
        out.push(BackupEntry {
            date: bd.date,
            timestamp: bd.timestamp,
            location: location.to_string(),
            size,
            filename: name,
        });
    }
    out
}

#[derive(Deserialize, Default)]
pub struct BackupQuery {
    /// `force=1` skips hash-based change detection. Matches Go.
    #[serde(default)]
    pub force: Option<String>,
}

/// POST /api/system/backup
///
/// Query: `force=1` to bypass change detection. Always writes a local copy
/// as a safety net even when the primary destination is an archive server,
/// so a flaky network can't leave you with no backup at all.
pub async fn create_backup(
    State(_s): State<AppState>,
    Query(q): Query<BackupQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let data = match build_backup_data_async().await {
        Ok(d) => d,
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to create backup: {}", e),
            );
        }
    };

    let force = q.force.as_deref() == Some("1");
    let current_hash = compute_backup_hash(&data);
    if !force && current_hash == read_last_hash() && !current_hash.is_empty() {
        let short = &current_hash[..12.min(current_hash.len())];
        info!("[backup] Skipped — no changes detected (hash {})", short);
        return (StatusCode::OK, Json(serde_json::json!({
            "success": true,
            "skipped": true,
            "reason": "no changes detected",
            "date": data.date,
        })));
    }

    let prefs = crate::preferences::load_prefs();
    let location = prefs
        .get("backup_location")
        .and_then(|v| v.as_str())
        .unwrap_or("archive")
        .to_string();

    let primary: Result<(), String> = if location == "ssd" {
        write_backup_to_dir(LOCAL_BACKUP_DIR, &data)
    } else {
        let config_path = sentryusb_config::find_config_path();
        let archive_system = sentryusb_config::parse_file(config_path)
            .ok()
            .and_then(|(active, _)| active.get("ARCHIVE_SYSTEM").cloned())
            .unwrap_or_default();
        match archive_system.as_str() {
            "cifs" | "nfs" => {
                if Path::new("/mnt/archive").exists() {
                    write_backup_to_dir(ARCHIVE_BACKUP_DIR, &data)
                } else {
                    Err("archive not mounted at /mnt/archive".to_string())
                }
            }
            "rsync" => sync_backup_to_rsync(&data).await,
            "rclone" => sync_backup_to_rclone(&data).await,
            _ => {
                info!("[backup] No archive system configured, falling back to local SSD");
                write_backup_to_dir(LOCAL_BACKUP_DIR, &data)
            }
        }
    };

    // Safety-net local copy when primary is an archive target.
    if location != "ssd" {
        if let Err(e) = write_backup_to_dir(LOCAL_BACKUP_DIR, &data) {
            warn!("[backup] Warning: failed to write local backup copy: {}", e);
        }
    }

    if let Err(e) = primary {
        return crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Backup failed: {}", e),
        );
    }

    write_last_hash(&current_hash);
    (StatusCode::OK, Json(serde_json::json!({
        "success": true,
        "date": data.date,
        "location": location,
    })))
}

/// GET /api/system/backups
///
/// Merges local and archive listings, deduping by date (archive wins over
/// local when both exist). Matches Go `listBackups`.
pub async fn list_backups(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let mut all: Vec<BackupEntry> = Vec::new();
    all.extend(list_backups_in_dir(LOCAL_BACKUP_DIR, "ssd"));
    if Path::new(ARCHIVE_BACKUP_DIR).exists() {
        all.extend(list_backups_in_dir(ARCHIVE_BACKUP_DIR, "archive"));
    }

    // Dedupe by date: prefer archive copy if both exist.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for i in 0..all.len() {
        let d = all[i].date.clone();
        if let Some(&prev_idx) = seen.get(&d) {
            if all[i].location == "archive" {
                all[prev_idx] = BackupEntry {
                    date: all[i].date.clone(),
                    timestamp: all[i].timestamp.clone(),
                    location: all[i].location.clone(),
                    size: all[i].size,
                    filename: all[i].filename.clone(),
                };
            }
            all[i].date.clear(); // mark for removal
        } else {
            seen.insert(d, i);
        }
    }
    let mut result: Vec<BackupEntry> = all.into_iter().filter(|b| !b.date.is_empty()).collect();
    result.sort_by(|a, b| b.date.cmp(&a.date));
    (StatusCode::OK, Json(serde_json::to_value(result).unwrap_or_default()))
}

/// GET /api/system/backup/{date}
///
/// Tries the archive dir first (newer / offsite copy), then the local SSD
/// fallback. Returns the raw JSON with an `attachment` Content-Disposition.
pub async fn get_backup(
    State(_s): State<AppState>,
    AxPath(date): AxPath<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if date.is_empty() || date.contains("..") || date.contains('/') || date.contains('\\') {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid date").into_response();
    }
    let filename = backup_filename(&date);
    for dir in [ARCHIVE_BACKUP_DIR, LOCAL_BACKUP_DIR] {
        let path = format!("{}/{}", dir.trim_end_matches('/'), filename);
        if let Ok(data) = std::fs::read(&path) {
            let mut r = axum::response::Response::new(axum::body::Body::from(data));
            r.headers_mut()
                .insert("content-type", "application/json".parse().unwrap());
            r.headers_mut().insert(
                "content-disposition",
                format!("attachment; filename={}", filename).parse().unwrap(),
            );
            return r;
        }
    }
    crate::json_error(StatusCode::NOT_FOUND, &format!("backup not found for date: {}", date)).into_response()
}

fn save_prefs_from_strings(src: &HashMap<String, String>) {
    let mut prefs = crate::preferences::load_prefs();
    for (k, v) in src {
        prefs.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    crate::preferences::save_prefs(&prefs);
}

fn write_with_mode(path: &str, contents: &str, _mode: u32) -> std::io::Result<()> {
    std::fs::write(path, contents)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(_mode);
        let _ = std::fs::set_permissions(path, perms);
    }
    Ok(())
}

/// POST /api/system/restore
///
/// Body: the JSON envelope produced by `create_backup`. Writes all bundled
/// credential files back to their standard locations with correct modes.
/// Matches Go `restoreBackup` (backup.go:432-523).
pub async fn restore_backup(
    State(_s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let backup: BackupData = match serde_json::from_str(&body) {
        Ok(b) => b,
        Err(e) => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                &format!("Invalid backup JSON: {}", e),
            );
        }
    };
    if backup.version == 0 || backup.config.is_empty() {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "Invalid backup: missing version or config data",
        );
    }

    // Remount filesystem read-write for the config write.
    let _ = sentryusb_shell::run("bash", &["-c", "/root/bin/remountfs_rw"]).await;

    let config_path = sentryusb_config::find_config_path();
    if let Err(e) = std::fs::write(config_path, &backup.config) {
        return crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Failed to write config: {}", e),
        );
    }
    info!("[backup] Restored config to {}", config_path);

    if !backup.preferences.is_empty() {
        save_prefs_from_strings(&backup.preferences);
        info!("[backup] Restored {} preferences", backup.preferences.len());
    }

    if !backup.ssh_private_key.is_empty() {
        let _ = std::fs::create_dir_all("/root/.ssh");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                "/root/.ssh",
                std::fs::Permissions::from_mode(0o700),
            );
        }
        match write_with_mode(SSH_PRIVATE_KEY, &backup.ssh_private_key, 0o600) {
            Ok(()) => info!("[backup] Restored SSH private key"),
            Err(e) => warn!("[backup] Failed to restore SSH private key: {}", e),
        }
        if !backup.ssh_public_key.is_empty() {
            if let Err(e) = write_with_mode(SSH_PUBLIC_KEY, &backup.ssh_public_key, 0o644) {
                warn!("[backup] Failed to restore SSH public key: {}", e);
            }
        }
    }

    if !backup.rclone_config.is_empty() {
        let _ = std::fs::create_dir_all("/root/.config/rclone");
        match write_with_mode(RCLONE_CONFIG, &backup.rclone_config, 0o600) {
            Ok(()) => info!("[backup] Restored rclone config"),
            Err(e) => warn!("[backup] Failed to restore rclone config: {}", e),
        }
    }

    if !backup.ble_private_key.is_empty() {
        let _ = std::fs::create_dir_all("/root/.ble");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                "/root/.ble",
                std::fs::Permissions::from_mode(0o700),
            );
        }
        match write_with_mode(BLE_PRIVATE_KEY, &backup.ble_private_key, 0o600) {
            Ok(()) => {
                info!("[backup] Restored BLE private key");
                if !backup.ble_public_key.is_empty() {
                    let _ = write_with_mode(BLE_PUBLIC_KEY, &backup.ble_public_key, 0o644);
                }
                // Mark as paired so the app doesn't prompt for re-pair.
                let _ = std::fs::write("/root/.ble/paired", "1");
            }
            Err(e) => warn!("[backup] Failed to restore BLE private key: {}", e),
        }
    }

    if !backup.notification_credentials.is_empty() {
        let _ = std::fs::create_dir_all("/root/.sentryusb");
        match write_with_mode(NOTIFICATION_CREDS, &backup.notification_credentials, 0o600) {
            Ok(()) => info!("[backup] Restored notification credentials"),
            Err(e) => warn!("[backup] Failed to restore notification credentials: {}", e),
        }
    }

    // Reparse the restored config so the wizard can re-populate fields.
    let active: HashMap<String, String> = sentryusb_config::parse_file(config_path)
        .map(|(a, _)| a.into_iter().collect())
        .unwrap_or_default();

    (StatusCode::OK, Json(serde_json::json!({
        "success": true,
        "date": backup.date,
        "hostname": backup.hostname,
        "config": active,
    })))
}
