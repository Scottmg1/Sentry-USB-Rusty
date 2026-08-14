//! Config backup and restore.
//!
//! A JSON envelope contains configuration, preferences, SSH and BLE keys,
//! rclone state, and notification credentials. A stable SHA-256 skips
//! identical copies.

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

// Support both current Ed25519 keys and RSA keys from earlier installs.
const SSH_ED25519_PRIVATE_KEY: &str = "/root/.ssh/id_ed25519";
const SSH_ED25519_PUBLIC_KEY: &str = "/root/.ssh/id_ed25519.pub";
const SSH_RSA_PRIVATE_KEY: &str = "/root/.ssh/id_rsa";
const SSH_RSA_PUBLIC_KEY: &str = "/root/.ssh/id_rsa.pub";
const RCLONE_CONFIG: &str = "/root/.config/rclone/rclone.conf";
const BLE_PRIVATE_KEY: &str = "/root/.ble/key_private.pem";
const BLE_PUBLIC_KEY: &str = "/root/.ble/key_public.pem";
const NOTIFICATION_CREDS: &str = "/root/.sentryusb/notification-credentials.json";

/// Read the Ed25519 keypair if present, otherwise RSA.
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

#[derive(Serialize, Deserialize, Default, Clone)]
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

/// Flatten preferences to strings, serializing structured JSON values.
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

/// Run `write` against `/mnt/archive` under the shared mount lock.
///
/// If the share wasn't mounted, mounts it from fstab and unmounts it
/// again before releasing the lock. Pre-existing mounts remain owned by their
/// caller. Cleanup runs after write failure without masking the write result.
///
/// A detached task finishes cleanup even if the HTTP request is cancelled.
async fn with_archive_mounted<F, Fut>(write: F) -> Result<(), String>
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<(), String>> + Send + 'static,
{
    tokio::spawn(archive_mount_transaction(write))
        .await
        .map_err(|e| format!("archive write task: {}", e))?
}

async fn archive_mount_transaction<F, Fut>(write: F) -> Result<(), String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    async fn is_mounted() -> bool {
        sentryusb_shell::run("findmnt", &["--mountpoint", "/mnt/archive"])
            .await
            .is_ok()
    }

    if !Path::new("/mnt/archive").exists() {
        return Err("archive mount point /mnt/archive does not exist".to_string());
    }

    // Archiveloop holds this lock only during mount transitions.
    let guard = tokio::task::spawn_blocking(|| {
        crate::archive_mount_lock::acquire(Duration::from_secs(60))
    })
    .await
    .map_err(|e| format!("archive mount lock task: {}", e))?
    .map_err(|e| format!("archive mount lock: {}", e))?;

    let mut mounted_by_us = false;
    if !is_mounted().await {
        let mount_res = sentryusb_shell::run_with_timeout(
            Duration::from_secs(30),
            "mount",
            &["/mnt/archive"],
        )
        .await;
        // A timed-out mount process may still complete the kernel transition.
        if is_mounted().await {
            mounted_by_us = true;
        } else {
            return match mount_res {
                Err(e) => Err(format!("mount /mnt/archive: {}", e)),
                Ok(_) => Err("archive not mounted at /mnt/archive after mount".to_string()),
            };
        }
    }

    let result = write().await;

    if mounted_by_us {
        // Bound force/lazy unmount so an unreachable share cannot hang the API.
        if let Err(e) = sentryusb_shell::run_with_timeout(
            Duration::from_secs(15),
            "umount",
            &["-f", "-l", "/mnt/archive"],
        )
        .await
        {
            warn!("[backup] failed to unmount /mnt/archive after backup: {}", e);
        }
    }

    drop(guard);
    result
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
    let port = sentryusb_shell::SshPort::new(sentryusb_config::rsync_ssh_port(&active));

    let tmp_dir = "/tmp/sentryusb-backup-sync";
    let _ = std::fs::create_dir_all(tmp_dir);
    let filename = backup_filename(&data.date);
    let tmp_path = format!("{}/{}", tmp_dir, filename);
    let json_bytes = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
    std::fs::write(&tmp_path, &json_bytes).map_err(|e| e.to_string())?;

    // Remote directory creation is best-effort.
    let user_at_server = format!("{}@{}", user, server);
    let remote_dir = format!("{}/backups", rsync_path);
    let mut ssh_args: Vec<&str> = vec![
        "-o", "ConnectTimeout=10", "-o", "StrictHostKeyChecking=no", "-o", "BatchMode=yes",
    ];
    ssh_args.extend(port.ssh_args());
    ssh_args.extend_from_slice(&[&user_at_server, "mkdir", "-p", &remote_dir]);
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(10), "ssh", &ssh_args,
    ).await;

    let dest = format!("{}@{}:{}/backups/{}", user, server, rsync_path, filename);
    let mut args: Vec<&str> = vec!["-avh", "--no-perms", "--omit-dir-times", "--timeout=60"];
    args.extend(port.rsync_args());
    args.extend_from_slice(&[&tmp_path, &dest]);
    let res = sentryusb_shell::run_with_timeout(
        Duration::from_secs(60), "rsync", &args,
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

/// Drive-DB scratch space on the large backing partition.
const DB_EXPORT_TMP: &str = "/backingfiles/.backup-drive-data.db";
const DRIVE_DB_PATH: &str = "/backingfiles/drive-data.db";

fn drive_data_filename(date: &str) -> String {
    format!("sentryusb-backup-{}.drive-data.db.gz", date)
}

/// Marker for the last calendar day the drive-DB snapshot shipped
/// successfully, containing `<date> <location>`.
const DB_BACKUP_STAMP: &str = "/mutable/.last-drive-db-backup";

/// Serialize jobs that share [`DB_EXPORT_TMP`].
static DB_BACKUP_JOB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// True while a VACUUM/gzip/ship is in flight — the slow-request
/// middleware logs it so an episode during a backup names its cause.
pub static DB_BACKUP_IN_FLIGHT: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Daily-gated drive-DB snapshot. Returns whether a dated DB companion
/// exists for `date` (shipped now, or earlier today on a skip).
async fn ship_drive_data_gated(
    s: &AppState,
    location: &str,
    date: &str,
    force: bool,
) -> bool {
    use std::sync::atomic::Ordering;

    let _job = DB_BACKUP_JOB.lock().await;

    if location == "ssd" {
        // Local storage is too small for dated database exports.
        info!("[backup] drive_db_snapshot_skipped reason=ssd_location");
        return false;
    }
    let stamp = std::fs::read_to_string(DB_BACKUP_STAMP).unwrap_or_default();
    if !force && stamp.split_whitespace().next() == Some(date) {
        info!(
            "[backup] drive_db_snapshot_skipped reason=already_today date={}",
            date
        );
        return true;
    }

    DB_BACKUP_IN_FLIGHT.store(true, Ordering::Release);
    let shipped = match export_drive_data_gz(s.drives.store.clone()).await {
        Ok(gz) => {
            let ok = match ship_drive_data(location, &gz, date).await {
                Ok(()) => {
                    info!(
                        "[backup] Drive DB snapshot shipped as {}",
                        drive_data_filename(date)
                    );
                    true
                }
                Err(e) => {
                    warn!("[backup] Drive DB snapshot not shipped: {}", e);
                    false
                }
            };
            let _ = std::fs::remove_file(&gz);
            ok
        }
        Err(e) => {
            warn!("[backup] Drive DB export failed: {}", e);
            false
        }
    };
    DB_BACKUP_IN_FLIGHT.store(false, Ordering::Release);

    if shipped {
        let _ = std::fs::write(DB_BACKUP_STAMP, format!("{} {}", date, location));
    }
    shipped
}

async fn export_drive_data_gz(
    store: std::sync::Arc<sentryusb_drives::DriveStore>,
) -> Result<String, String> {
    if !Path::new(DRIVE_DB_PATH).exists() {
        return Err("drive DB does not exist yet".to_string());
    }
    let gz = format!("{}.gz", DB_EXPORT_TMP);
    // VACUUM INTO refuses to overwrite stale scratch files.
    let _ = std::fs::remove_file(DB_EXPORT_TMP);
    let _ = std::fs::remove_file(&gz);

    // A dedicated read connection avoids the application's writer mutex.
    let vacuum =
        tokio::task::spawn_blocking(move || store.vacuum_into(DB_EXPORT_TMP)).await;
    match vacuum {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("VACUUM INTO failed: {}", e)),
        Err(e) => return Err(format!("export task failed: {}", e)),
    }

    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "gzip",
        &["-f", DB_EXPORT_TMP],
    )
    .await
    .map_err(|e| format!("gzip failed: {}", e))?;
    Ok(gz)
}

/// Ship the drive-DB snapshot beside the JSON backup.
/// Local mutable storage is intentionally excluded because it is too small.
async fn ship_drive_data(location: &str, gz_path: &str, date: &str) -> Result<(), String> {
    if location == "ssd" {
        return Err(
            "backup_location is local SSD — drive DB snapshot is only shipped to archive \
             destinations (/mutable is too small for dated DB copies)"
                .to_string(),
        );
    }
    let filename = drive_data_filename(date);
    let config_path = sentryusb_config::find_config_path();
    let (active, _) = sentryusb_config::parse_file(config_path).map_err(|e| e.to_string())?;
    let archive_system = active.get("ARCHIVE_SYSTEM").cloned().unwrap_or_default();
    match archive_system.as_str() {
        "cifs" | "nfs" => {
            let gz = gz_path.to_string();
            let dest = format!("{}/{}", ARCHIVE_BACKUP_DIR, filename);
            with_archive_mounted(move || async move {
                // Keep network file I/O off the async runtime.
                tokio::task::spawn_blocking(move || {
                    std::fs::create_dir_all(ARCHIVE_BACKUP_DIR)
                        .map_err(|e| format!("create {}: {}", ARCHIVE_BACKUP_DIR, e))?;
                    let tmp = format!("{}.tmp", dest);
                    std::fs::copy(&gz, &tmp).map_err(|e| format!("copy to archive: {}", e))?;
                    std::fs::rename(&tmp, &dest).map_err(|e| format!("finalize: {}", e))
                })
                .await
                .map_err(|e| format!("copy task failed: {}", e))?
            })
            .await
        }
        "rsync" => {
            let server = active.get("RSYNC_SERVER").cloned().unwrap_or_default();
            let user = active.get("RSYNC_USER").cloned().unwrap_or_default();
            let rsync_path = active.get("RSYNC_PATH").cloned().unwrap_or_default();
            if server.is_empty() || user.is_empty() {
                return Err("rsync not configured".to_string());
            }
            let port = sentryusb_shell::SshPort::new(sentryusb_config::rsync_ssh_port(&active));
            let dest = format!("{}@{}:{}/backups/{}", user, server, rsync_path, filename);
            let mut args: Vec<&str> =
                vec!["-h", "--no-perms", "--omit-dir-times", "--timeout=300"];
            args.extend(port.rsync_args());
            args.extend_from_slice(&[gz_path, &dest]);
            sentryusb_shell::run_with_timeout(Duration::from_secs(600), "rsync", &args)
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        "rclone" => {
            let drive = active.get("RCLONE_DRIVE").cloned().unwrap_or_default();
            let rclone_path = active.get("RCLONE_PATH").cloned().unwrap_or_default();
            if drive.is_empty() {
                return Err("rclone not configured".to_string());
            }
            // Stage with the dated filename preserved by rclone.
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
        _ => Err("no archive system configured".to_string()),
    }
}

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

    let force = q.force.as_deref() == Some("1");

    // Database data changes independently of config, so ship it even when the
    // config hash is unchanged. Automatic exports are daily; forced exports
    // bypass the gate. Failure does not block the config backup.
    data.drive_data_included = ship_drive_data_gated(&s, &location, &data.date, force).await;
    let current_hash = compute_backup_hash(&data);
    if !force && current_hash == read_last_hash() && !current_hash.is_empty() {
        let short = &current_hash[..12.min(current_hash.len())];
        info!("[backup] Skipped config backup — no changes detected (hash {})", short);
        return (StatusCode::OK, Json(serde_json::json!({
            "success": true,
            "skipped": true,
            "reason": "no changes detected",
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
                // The detached transaction may outlive its request.
                let data = data.clone();
                with_archive_mounted(move || async move {
                    write_backup_to_dir(ARCHIVE_BACKUP_DIR, &data)
                })
                .await
            }
            "rsync" => sync_backup_to_rsync(&data).await,
            "rclone" => sync_backup_to_rclone(&data).await,
            _ => {
                info!("[backup] No archive system configured, falling back to local SSD");
                write_backup_to_dir(LOCAL_BACKUP_DIR, &data)
            }
        }
    };

    // Retain a local safety copy for remote destinations.
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
    invalidate_backup_list();
    (StatusCode::OK, Json(serde_json::json!({
        "success": true,
        "date": data.date,
        "location": location,
    })))
}

/// Cache potentially slow local and network directory scans briefly.
const BACKUP_LIST_TTL: std::time::Duration = std::time::Duration::from_secs(60);

static BACKUP_LIST_CACHE: std::sync::Mutex<
    Option<(std::time::Instant, serde_json::Value)>,
> = std::sync::Mutex::new(None);

/// Called by the backup-creation paths so a fresh backup shows up
/// immediately instead of after the TTL.
pub(crate) fn invalidate_backup_list() {
    *BACKUP_LIST_CACHE.lock().unwrap() = None;
}

/// GET /api/system/backups
///
/// Merges local and archive listings, deduping by date (archive wins over
/// local when both exist).
pub async fn list_backups(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    {
        let guard = BACKUP_LIST_CACHE.lock().unwrap();
        if let Some((at, v)) = guard.as_ref() {
            if at.elapsed() < BACKUP_LIST_TTL {
                return (StatusCode::OK, Json(v.clone()));
            }
        }
    }
    // Keep local and network directory scans off the async runtime.
    let (code, body) = tokio::task::spawn_blocking(list_backups_blocking)
        .await
        .unwrap_or_else(|_| {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "backup listing task failed")
        });
    if code == StatusCode::OK {
        *BACKUP_LIST_CACHE.lock().unwrap() =
            Some((std::time::Instant::now(), body.0.clone()));
    }
    (code, body)
}

fn list_backups_blocking() -> (StatusCode, Json<serde_json::Value>) {
    let mut all: Vec<BackupEntry> = Vec::new();
    all.extend(list_backups_in_dir(LOCAL_BACKUP_DIR, "ssd"));
    if Path::new(ARCHIVE_BACKUP_DIR).exists() {
        all.extend(list_backups_in_dir(ARCHIVE_BACKUP_DIR, "archive"));
    }

    // Prefer the archive copy when dates collide.
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
/// Restore a backup envelope into configuration and credential files.
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
        // Restore each private key under the filename matching its embedded type.
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
        // Keep mutable OAuth state off the read-only root; fall back in place if
        // mutable storage is unavailable.
        if let Err(e) = sentryusb_setup::archive::ensure_rclone_config_on_mutable().await {
            warn!("[backup] rclone config migration failed, restoring in place: {}", e);
        }
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
                // Preserve pairing state with the restored key.
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
