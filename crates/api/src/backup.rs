//! System backup and restore.
//!
//! A backup is a JSON envelope plus an optional app-data bundle. The JSON
//! envelope contains `sentryusb.conf`, user preferences, SSH keys, rclone
//! config, Tesla BLE pairing keys, cloud/push credentials, and other small
//! setup state. The app-data bundle contains the SQLite application database
//! plus small mutable/root state that should survive an SD-card reflash. Large
//! virtual disk images, snapshots, and dashcam/video files are deliberately not
//! included; those continue to live in the normal archive destination.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::router::AppState;

const LOCAL_BACKUP_DIR: &str = "/mutable/backups";
const LOCAL_APP_DATA_BACKUP_DIR: &str = "/backingfiles/backups";
const ARCHIVE_BACKUP_DIR: &str = "/mnt/archive/backups";
const LAST_HASH_FILE: &str = "/mutable/backups/.last_hash";
const BACKUP_VERSION: u32 = 2;

const APP_DATA_EXPORT_DIR: &str = "/backingfiles/.backup-app-data";
const APP_DATA_EXPORT_GZ: &str = "/backingfiles/.backup-app-data.tar.gz";
const APP_DATA_RESTORE_DIR: &str = "/backingfiles/.restore-app-data";
const PENDING_DB_RESTORE: &str = "/backingfiles/drive-data.db.restore-pending";
const PENDING_DB_RESTORE_MARKER: &str = "/backingfiles/.restore-drive-data-pending";

// Paths included in a backup.
//
// The Rust wizard generates ed25519 keys (smaller, faster, modern) at
// /root/.ssh/id_ed25519 — the Go-era code generated RSA at
// /root/.ssh/id_rsa. Backups need to find whichever was generated, AND
// restores need to write the key back to the path matching its type.
// Always check ed25519 first since that's what new installs produce;
// fall back to RSA so restoring an old Go-era backup still works.
const SSH_ED25519_PRIVATE_KEY: &str = "/root/.ssh/id_ed25519";
const SSH_ED25519_PUBLIC_KEY: &str = "/root/.ssh/id_ed25519.pub";
const SSH_RSA_PRIVATE_KEY: &str = "/root/.ssh/id_rsa";
const SSH_RSA_PUBLIC_KEY: &str = "/root/.ssh/id_rsa.pub";
const RCLONE_CONFIG: &str = "/root/.config/rclone/rclone.conf";
const BLE_PRIVATE_KEY: &str = "/root/.ble/key_private.pem";
const BLE_PUBLIC_KEY: &str = "/root/.ble/key_public.pem";
const NOTIFICATION_CREDS: &str = "/root/.sentryusb/notification-credentials.json";

/// Durable state paths copied into the app-data bundle when present. Keep this
/// tight: no virtual disk images, snapshots, TeslaCam media, logs, session
/// cookies, or archive payloads.
const APP_DATA_FILES: &[&str] = &[
    "/backingfiles/drive-data.json",
    "/mutable/.sentryusb_preferences.json",
    "/mutable/sentryusb-prefs.json",
    "/mutable/sentryusb-notifications.json",
    "/mutable/.notification_history.json",
    "/mutable/LockChime.wav",
    "/mutable/keep_accessory_gps.json",
    "/mutable/sentryusb_away_mode.json",
    "/mutable/.drive-data-last-sync",
    "/mutable/.beaconed",
    "/root/sentryusb.conf",
    "/root/.sentryusb_version",
];

const APP_DATA_DIRS: &[&str] = &[
    "/mutable/LockChime",
    "/mutable/Wraps",
    "/mutable/.wraps_deleted",
    "/mutable/LicensePlate",
    "/mutable/configs",
    "/mutable/etc",
    "/mutable/var/lib",
    "/mutable/varlib",
    "/mutable/.bluetooth",
    "/root/.sentryusb",
    "/root/.ble",
    "/root/.ssh",
    "/root/.config/rclone",
];

/// Read whichever SSH keypair exists on disk. ed25519 wins when both are
/// present (newer install ran ssh-keygen on top of an old RSA key). Returns
/// `(private_pem, public_pem)`; either may be empty if no keypair is set up.
fn read_ssh_keypair() -> (String, String) {
    if std::path::Path::new(SSH_ED25519_PRIVATE_KEY).exists() {
        return (
            read_file_if_exists(SSH_ED25519_PRIVATE_KEY),
            read_file_if_exists(SSH_ED25519_PUBLIC_KEY),
        );
    }
    if std::path::Path::new(SSH_RSA_PRIVATE_KEY).exists() {
        return (
            read_file_if_exists(SSH_RSA_PRIVATE_KEY),
            read_file_if_exists(SSH_RSA_PUBLIC_KEY),
        );
    }
    (String::new(), String::new())
}

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
    #[serde(default)]
    app_data_included: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    app_data_filename: String,

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
    app_data_included: bool,
    app_data_size: u64,
    app_data_filename: String,
    total_size: u64,
}

#[derive(Serialize)]
struct AppDataManifest {
    version: u32,
    date: String,
    timestamp: String,
    includes: Vec<String>,
    missing: Vec<String>,
}

#[derive(Default)]
struct AppDataRestoreResult {
    restored_paths: Vec<String>,
    db_restore_pending: bool,
}

fn backup_filename(date: &str) -> String {
    format!("sentryusb-backup-{}.json", date)
}

fn app_data_filename(date: &str) -> String {
    format!("sentryusb-backup-{}.app-data.tar.gz", date)
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
    let (ssh_private_key, ssh_public_key) = read_ssh_keypair();
    Ok(BackupData {
        version: BACKUP_VERSION,
        date: now.format("%Y-%m-%d").to_string(),
        timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        hostname,
        config,
        preferences: prefs_as_strings(),
        drive_data_included: false,
        app_data_included: false,
        app_data_filename: String::new(),
        ssh_private_key,
        ssh_public_key,
        rclone_config: read_file_if_exists(RCLONE_CONFIG),
        ble_private_key: read_file_if_exists(BLE_PRIVATE_KEY),
        ble_public_key: read_file_if_exists(BLE_PUBLIC_KEY),
        notification_credentials: read_file_if_exists(NOTIFICATION_CREDS),
    })
}

/// Hex SHA-256 of all backup-relevant data with time-varying fields excluded
/// so the hash is stable across identical snapshots. Preferences are sorted
/// by key so hashing order is deterministic.
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
    ctx.update(if data.drive_data_included { b"drive:1" } else { b"drive:0" });
    ctx.update(if data.app_data_included { b"app:1" } else { b"app:0" });
    ctx.update(data.app_data_filename.as_bytes());
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

fn bundle_rel_path(path: &str) -> PathBuf {
    Path::new(path.trim_start_matches('/')).to_path_buf()
}

fn copy_file_atomic(src: &str, dest: &str) -> Result<(), String> {
    if let Some(parent) = Path::new(dest).parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let tmp = format!("{}.tmp", dest);
    std::fs::copy(src, &tmp)
        .map_err(|e| { let _ = std::fs::remove_file(&tmp); format!("copy {} -> {}: {}", src, dest, e) })?;
    std::fs::rename(&tmp, dest).map_err(|e| format!("finalize {}: {}", dest, e))
}

fn copy_path_recursive(src: &Path, dest: &Path) -> Result<(), String> {
    let meta = match std::fs::symlink_metadata(src) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("stat {}: {}", src.display(), e)),
    };
    if meta.file_type().is_symlink() {
        // Restore backups should be plain files/dirs. Symlinks are skipped so
        // a crafted backup cannot write through an unexpected link target.
        return Ok(());
    }
    if meta.is_dir() {
        std::fs::create_dir_all(dest)
            .map_err(|e| format!("create {}: {}", dest.display(), e))?;
        for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {}", src.display(), e))? {
            let entry = entry.map_err(|e| e.to_string())?;
            let name = entry.file_name();
            copy_path_recursive(&entry.path(), &dest.join(PathBuf::from(name)))?;
        }
        return Ok(());
    }
    if meta.is_file() {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {}", parent.display(), e))?;
        }
        std::fs::copy(src, dest)
            .map(|_| ())
            .map_err(|e| format!("copy {} -> {}: {}", src.display(), dest.display(), e))?;
    }
    Ok(())
}

fn restore_path_recursive(src: &Path, dest: &Path) -> Result<Vec<String>, String> {
    if !src.exists() {
        return Ok(Vec::new());
    }
    let meta = std::fs::symlink_metadata(src)
        .map_err(|e| format!("stat restored path {}: {}", src.display(), e))?;
    if meta.file_type().is_symlink() {
        return Err(format!("refusing to restore symlink {}", src.display()));
    }
    if meta.is_dir() {
        let _ = std::fs::remove_dir_all(dest);
    } else {
        let _ = std::fs::remove_file(dest);
    }
    copy_path_recursive(src, dest)?;
    Ok(vec![dest.display().to_string()])
}

fn export_drive_db_to(
    store: std::sync::Arc<sentryusb_drives::DriveStore>,
    dest: PathBuf,
) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create {}: {}", parent.display(), e))?;
    }
    let _ = std::fs::remove_file(&dest);
    let dest_s = dest.to_string_lossy().to_string();
    store.with_locked_conn(|conn| {
        conn.execute("VACUUM INTO ?1", rusqlite::params![dest_s])
            .map(|_| ())
    })
    .map_err(|e| format!("VACUUM INTO failed: {}", e))
}

async fn export_app_data_bundle(
    store: std::sync::Arc<sentryusb_drives::DriveStore>,
    data: &BackupData,
) -> Result<String, String> {
    let _ = std::fs::remove_dir_all(APP_DATA_EXPORT_DIR);
    let _ = std::fs::remove_file(APP_DATA_EXPORT_GZ);
    std::fs::create_dir_all(APP_DATA_EXPORT_DIR)
        .map_err(|e| format!("create app-data staging dir: {}", e))?;

    let mut includes = Vec::new();
    let mut missing = Vec::new();

    let db_dest = Path::new(APP_DATA_EXPORT_DIR).join("backingfiles/drive-data.db");
    if Path::new(DRIVE_DB_PATH).exists() {
        let store_for_db = store.clone();
        let db_dest_for_task = db_dest.clone();
        tokio::task::spawn_blocking(move || export_drive_db_to(store_for_db, db_dest_for_task))
            .await
            .map_err(|e| format!("drive DB export task failed: {}", e))??;
        includes.push("backingfiles/drive-data.db".to_string());
    } else {
        missing.push(DRIVE_DB_PATH.to_string());
    }

    for src in APP_DATA_FILES {
        let src_path = Path::new(src);
        if src_path.exists() {
            let rel = bundle_rel_path(src);
            copy_path_recursive(src_path, &Path::new(APP_DATA_EXPORT_DIR).join(&rel))?;
            includes.push(rel.display().to_string());
        } else {
            missing.push((*src).to_string());
        }
    }

    for src in APP_DATA_DIRS {
        let src_path = Path::new(src);
        if src_path.exists() {
            let rel = bundle_rel_path(src);
            copy_path_recursive(src_path, &Path::new(APP_DATA_EXPORT_DIR).join(&rel))?;
            includes.push(format!("{}/", rel.display()));
        } else {
            missing.push((*src).to_string());
        }
    }

    let manifest = AppDataManifest {
        version: BACKUP_VERSION,
        date: data.date.clone(),
        timestamp: data.timestamp.clone(),
        includes,
        missing,
    };
    let manifest_json = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| format!("marshal app-data manifest: {}", e))?;
    std::fs::write(
        Path::new(APP_DATA_EXPORT_DIR).join("manifest.json"),
        manifest_json,
    )
    .map_err(|e| format!("write app-data manifest: {}", e))?;

    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "tar",
        &["-C", APP_DATA_EXPORT_DIR, "-czf", APP_DATA_EXPORT_GZ, "."],
    )
    .await
    .map_err(|e| format!("tar app-data bundle failed: {}", e))?;
    Ok(APP_DATA_EXPORT_GZ.to_string())
}

async fn ship_app_data(location: &str, gz_path: &str, date: &str) -> Result<(), String> {
    let filename = app_data_filename(date);
    let config_path = sentryusb_config::find_config_path();
    let archive_system = sentryusb_config::parse_file(config_path)
        .ok()
        .and_then(|(active, _)| active.get("ARCHIVE_SYSTEM").cloned())
        .unwrap_or_default();

    match (location, archive_system.as_str()) {
        ("ssd", _) => {
            let dest = format!("{}/{}", LOCAL_APP_DATA_BACKUP_DIR, filename);
            copy_file_atomic(gz_path, &dest)
        }
        (_, "cifs" | "nfs") => {
            if !Path::new("/mnt/archive").exists() {
                return Err("archive not mounted at /mnt/archive".to_string());
            }
            let dest = format!("{}/{}", ARCHIVE_BACKUP_DIR, filename);
            copy_file_atomic(gz_path, &dest)
        }
        (_, "rsync") => {
            let (active, _) = sentryusb_config::parse_file(config_path).map_err(|e| e.to_string())?;
            let server = active.get("RSYNC_SERVER").cloned().unwrap_or_default();
            let user = active.get("RSYNC_USER").cloned().unwrap_or_default();
            let rsync_path = active.get("RSYNC_PATH").cloned().unwrap_or_default();
            if server.is_empty() || user.is_empty() {
                return Err("rsync not configured".to_string());
            }
            let user_at_server = format!("{}@{}", user, server);
            let remote_dir = format!("{}/backups", rsync_path);
            let _ = sentryusb_shell::run_with_timeout(
                Duration::from_secs(10),
                "ssh",
                &[
                    "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=no",
                    "-o", "BatchMode=yes", &user_at_server, "mkdir", "-p", &remote_dir,
                ],
            ).await;
            let dest = format!("{}@{}:{}/backups/{}", user, server, rsync_path, filename);
            sentryusb_shell::run_with_timeout(
                Duration::from_secs(600),
                "rsync",
                &["-h", "--no-perms", "--omit-dir-times", "--timeout=300", gz_path, &dest],
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
        }
        (_, "rclone") => {
            let (active, _) = sentryusb_config::parse_file(config_path).map_err(|e| e.to_string())?;
            let drive = active.get("RCLONE_DRIVE").cloned().unwrap_or_default();
            let rclone_path = active.get("RCLONE_PATH").cloned().unwrap_or_default();
            if drive.is_empty() {
                return Err("rclone not configured".to_string());
            }
            let staged = format!("/backingfiles/{}", filename);
            std::fs::rename(gz_path, &staged).map_err(|e| format!("stage: {}", e))?;
            let dest = format!("{}:{}/backups/", drive, rclone_path);
            let res = sentryusb_shell::run_with_timeout(
                Duration::from_secs(600),
                "rclone",
                &["--config", "/root/.config/rclone/rclone.conf", "copy", &staged, &dest],
            )
            .await;
            let _ = std::fs::rename(&staged, gz_path);
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        _ => {
            let dest = format!("{}/{}", LOCAL_APP_DATA_BACKUP_DIR, filename);
            info!("[backup] No archive system configured, writing app-data bundle locally");
            copy_file_atomic(gz_path, &dest)
        }
    }
}

fn app_data_path_for(dir: &str, date: &str) -> String {
    format!("{}/{}", dir.trim_end_matches('/'), app_data_filename(date))
}

fn find_app_data_backup(date: &str) -> Option<String> {
    for dir in [ARCHIVE_BACKUP_DIR, LOCAL_APP_DATA_BACKUP_DIR] {
        let path = app_data_path_for(dir, date);
        if Path::new(&path).exists() {
            return Some(path);
        }
    }
    None
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
        let app_data_path = if location == "ssd" {
            app_data_path_for(LOCAL_APP_DATA_BACKUP_DIR, &bd.date)
        } else {
            app_data_path_for(dir, &bd.date)
        };
        let app_data_size = std::fs::metadata(&app_data_path).map(|m| m.len()).unwrap_or(0);
        let app_data_filename = if app_data_size > 0 {
            app_data_filename(&bd.date)
        } else {
            String::new()
        };
        out.push(BackupEntry {
            date: bd.date,
            timestamp: bd.timestamp,
            location: location.to_string(),
            size,
            filename: name,
            app_data_included: app_data_size > 0,
            app_data_size,
            app_data_filename,
            total_size: size.saturating_add(app_data_size),
        });
    }
    out
}

/// Main app database path.
const DRIVE_DB_PATH: &str = "/backingfiles/drive-data.db";

#[derive(Deserialize, Default)]
pub struct BackupQuery {
    /// `force=1` skips hash-based change detection.
    #[serde(default)]
    pub force: Option<String>,
}

/// POST /api/system/backup
///
/// Query: `force=1` to bypass change detection. Always writes a local copy
/// as a safety net even when the primary destination is an archive server,
/// so a flaky network can't leave you with no backup at all.
pub async fn create_backup(
    State(s): State<AppState>,
    Query(q): Query<BackupQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut data = match build_backup_data_async().await {
        Ok(d) => d,
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to create backup: {}", e),
            );
        }
    };

    let prefs = crate::preferences::load_prefs();
    let location = prefs
        .get("backup_location")
        .and_then(|v| v.as_str())
        .unwrap_or("archive")
        .to_string();

    // Snapshot + ship the full app-data bundle next to the JSON backup.
    // This includes the drive/charge/telemetry SQLite DB plus small mutable
    // app state (notification history, lock chimes, wraps, BLE/cloud creds,
    // rclone/SSH config). Large disk images and video remain in the normal
    // archive flow.
    let app_data_gz = match export_app_data_bundle(s.drives.store.clone(), &data).await {
        Ok(gz) => gz,
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("App-data backup failed: {}", e),
            );
        }
    };
    if let Err(e) = ship_app_data(&location, &app_data_gz, &data.date).await {
        let _ = std::fs::remove_file(&app_data_gz);
        return crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("App-data backup failed: {}", e),
        );
    }
    if location != "ssd" {
        let local_app_data = format!(
            "{}/{}",
            LOCAL_APP_DATA_BACKUP_DIR,
            app_data_filename(&data.date)
        );
        if let Err(e) = copy_file_atomic(&app_data_gz, &local_app_data) {
            warn!("[backup] Warning: failed to write local app-data backup copy: {}", e);
        }
    }
    let _ = std::fs::remove_file(&app_data_gz);
    let _ = std::fs::remove_dir_all(APP_DATA_EXPORT_DIR);
    data.drive_data_included = Path::new(DRIVE_DB_PATH).exists();
    data.app_data_included = true;
    data.app_data_filename = app_data_filename(&data.date);

    let force = q.force.as_deref() == Some("1");
    let current_hash = compute_backup_hash(&data);
    if !force && current_hash == read_last_hash() && !current_hash.is_empty() {
        let short = &current_hash[..12.min(current_hash.len())];
        info!("[backup] Skipped config backup — no changes detected (hash {})", short);
        return (StatusCode::OK, Json(serde_json::json!({
            "success": true,
            "skipped": true,
            "reason": "no changes detected",
            "app_data_refreshed": true,
            "drive_data_refreshed": data.drive_data_included,
            "date": data.date,
        })));
    }

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
        "app_data_included": data.app_data_included,
        "app_data_filename": data.app_data_filename,
    })))
}

/// GET /api/system/backups
///
/// Merges local and archive listings, deduping by date (archive wins over
/// local when both exist).
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
                    app_data_included: all[i].app_data_included,
                    app_data_size: all[i].app_data_size,
                    app_data_filename: all[i].app_data_filename.clone(),
                    total_size: all[i].total_size,
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
    if !valid_backup_date(&date) {
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

/// GET /api/system/backup/{date}/app-data
///
/// Returns the app-data companion bundle when present. Archive copy wins over
/// the local `/backingfiles/backups` copy so restore/download prefers offsite
/// data if both exist.
pub async fn get_app_data_backup(
    State(_s): State<AppState>,
    AxPath(date): AxPath<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !valid_backup_date(&date) {
        return crate::json_error(StatusCode::BAD_REQUEST, "invalid date").into_response();
    }
    let filename = app_data_filename(&date);
    if let Some(path) = find_app_data_backup(&date) {
        if let Ok(data) = std::fs::read(&path) {
            let mut r = axum::response::Response::new(axum::body::Body::from(data));
            r.headers_mut()
                .insert("content-type", "application/gzip".parse().unwrap());
            r.headers_mut().insert(
                "content-disposition",
                format!("attachment; filename={}", filename).parse().unwrap(),
            );
            return r;
        }
    }
    crate::json_error(
        StatusCode::NOT_FOUND,
        &format!("app-data backup not found for date: {}", date),
    ).into_response()
}

fn valid_backup_date(date: &str) -> bool {
    !date.is_empty()
        && !date.contains("..")
        && !date.contains('/')
        && !date.contains('\\')
        && date
            .chars()
            .all(|c| c.is_ascii_digit() || c == '-')
}

fn is_safe_tar_entry(name: &str) -> bool {
    let clean = name.trim_start_matches("./");
    if clean.is_empty() {
        return true;
    }
    if clean.starts_with('/') || clean.contains("..") {
        return false;
    }
    clean == "manifest.json"
        || clean.starts_with("backingfiles/")
        || clean.starts_with("mutable/")
        || clean.starts_with("root/")
}

async fn validate_app_data_tar(path: &str) -> Result<(), String> {
    let listing = sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "tar",
        &["-tzf", path],
    )
    .await
    .map_err(|e| format!("list app-data bundle: {}", e))?;
    for line in listing.lines() {
        if !is_safe_tar_entry(line) {
            return Err(format!("unsafe path in app-data bundle: {}", line));
        }
    }
    Ok(())
}

fn check_sqlite_integrity(path: &str) -> Result<(), String> {
    let conn = rusqlite::Connection::open(path)
        .map_err(|e| format!("open restored drive DB: {}", e))?;
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| format!("drive DB integrity_check failed: {}", e))?;
    if result == "ok" {
        Ok(())
    } else {
        Err(format!("drive DB integrity_check returned {}", result))
    }
}

async fn restore_app_data_bundle(path: &str) -> Result<AppDataRestoreResult, String> {
    validate_app_data_tar(path).await?;

    let _ = std::fs::remove_dir_all(APP_DATA_RESTORE_DIR);
    std::fs::create_dir_all(APP_DATA_RESTORE_DIR)
        .map_err(|e| format!("create restore staging dir: {}", e))?;
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "tar",
        &["-xzf", path, "-C", APP_DATA_RESTORE_DIR],
    )
    .await
    .map_err(|e| format!("extract app-data bundle: {}", e))?;

    let mut result = AppDataRestoreResult::default();
    let staging = Path::new(APP_DATA_RESTORE_DIR);

    // Restore small files/directories immediately. The live SQLite DB is
    // handled separately below because the daemon already has it open.
    for src in APP_DATA_FILES {
        if *src == "/backingfiles/drive-data.json" {
            let staged = staging.join(bundle_rel_path(src));
            result.restored_paths.extend(restore_path_recursive(&staged, Path::new(src))?);
            continue;
        }
        let staged = staging.join(bundle_rel_path(src));
        result.restored_paths.extend(restore_path_recursive(&staged, Path::new(src))?);
    }
    for src in APP_DATA_DIRS {
        let staged = staging.join(bundle_rel_path(src));
        result.restored_paths.extend(restore_path_recursive(&staged, Path::new(src))?);
    }

    let staged_db = staging.join("backingfiles/drive-data.db");
    if staged_db.exists() {
        let staged_db_s = staged_db.to_string_lossy().to_string();
        tokio::task::spawn_blocking(move || check_sqlite_integrity(&staged_db_s))
            .await
            .map_err(|e| format!("drive DB integrity task failed: {}", e))??;
        let staged_db_s = staged_db.to_string_lossy().to_string();
        copy_file_atomic(&staged_db_s, PENDING_DB_RESTORE)?;
        std::fs::write(
            PENDING_DB_RESTORE_MARKER,
            format!("{}\n", chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        )
        .map_err(|e| format!("write pending DB restore marker: {}", e))?;
        result.db_restore_pending = true;
    }

    let _ = std::fs::remove_dir_all(APP_DATA_RESTORE_DIR);
    Ok(result)
}

/// Apply a pending app-data DB restore before the daemon opens SQLite.
/// Called at startup from `sentryusb` main.
pub fn apply_pending_app_data_restore_at_startup() {
    if !Path::new(PENDING_DB_RESTORE).exists() {
        return;
    }
    let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S").to_string();
    for suffix in ["", "-wal", "-shm"] {
        let current = format!("{}{}", DRIVE_DB_PATH, suffix);
        if Path::new(&current).exists() {
            let backup = format!("{}.pre-restore-{}{}", DRIVE_DB_PATH, stamp, suffix);
            if let Err(e) = std::fs::rename(&current, &backup) {
                warn!("[backup] pending DB restore: failed to move {} to {}: {}", current, backup, e);
                return;
            }
        }
    }
    if let Err(e) = std::fs::rename(PENDING_DB_RESTORE, DRIVE_DB_PATH) {
        warn!("[backup] pending DB restore: failed to install restored DB: {}", e);
        return;
    }
    let _ = std::fs::remove_file(PENDING_DB_RESTORE_MARKER);
    info!("[backup] Applied pending drive-data DB restore at startup");
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
/// Restore a backup envelope into config + DB.
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
    let app_data_path = if backup.app_data_included {
        match find_app_data_backup(&backup.date) {
            Some(p) => {
                if let Err(e) = validate_app_data_tar(&p).await {
                    return crate::json_error(
                        StatusCode::BAD_REQUEST,
                        &format!("Invalid app-data backup: {}", e),
                    );
                }
                Some(p)
            }
            None => {
                return crate::json_error(
                    StatusCode::NOT_FOUND,
                    &format!("App-data backup not found for date: {}", backup.date),
                );
            }
        }
    } else {
        None
    };

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
        // Pick the on-disk filename to match the embedded key type so the
        // restored pubkey lines up with the privkey and `ssh-keygen -y`
        // works as expected. Backups from the modern Rust wizard contain
        // ed25519 keys (OPENSSH PRIVATE KEY); Go-era backups contain RSA
        // (RSA PRIVATE KEY). Fall back to ed25519 for anything else
        // because that's what new installs default to.
        let priv_pem = backup.ssh_private_key.trim_start();
        let is_rsa = priv_pem.starts_with("-----BEGIN RSA PRIVATE KEY-----");
        let (priv_path, pub_path) = if is_rsa {
            (SSH_RSA_PRIVATE_KEY, SSH_RSA_PUBLIC_KEY)
        } else {
            (SSH_ED25519_PRIVATE_KEY, SSH_ED25519_PUBLIC_KEY)
        };
        match write_with_mode(priv_path, &backup.ssh_private_key, 0o600) {
            Ok(()) => info!("[backup] Restored SSH private key to {}", priv_path),
            Err(e) => warn!("[backup] Failed to restore SSH private key: {}", e),
        }
        if !backup.ssh_public_key.is_empty() {
            if let Err(e) = write_with_mode(pub_path, &backup.ssh_public_key, 0o644) {
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

    let app_data_restore = if let Some(path) = app_data_path {
        match restore_app_data_bundle(&path).await {
            Ok(r) => Some(r),
            Err(e) => {
                return crate::json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Failed to restore app data: {}", e),
                );
            }
        }
    } else {
        None
    };

    // Reparse the restored config so the wizard can re-populate fields.
    let active: HashMap<String, String> = sentryusb_config::parse_file(config_path)
        .map(|(a, _)| a.into_iter().collect())
        .unwrap_or_default();

    let app_data_restored = app_data_restore.is_some();
    let db_restore_pending = app_data_restore
        .as_ref()
        .map(|r| r.db_restore_pending)
        .unwrap_or(false);
    let restored_paths = app_data_restore
        .map(|r| r.restored_paths)
        .unwrap_or_default();

    (StatusCode::OK, Json(serde_json::json!({
        "success": true,
        "date": backup.date,
        "hostname": backup.hostname,
        "config": active,
        "app_data_restored": app_data_restored,
        "db_restore_pending": db_restore_pending,
        "restart_required": db_restore_pending,
        "restored_paths": restored_paths,
    })))
}
