//! Config backup and restore.
//!
//! A backup is a JSON envelope containing
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

/// Run `write` against a mounted `/mnt/archive`, owning the mount for
/// the duration. Serialized against archiveloop's connect/disconnect
/// scripts via the shared archive-mount flock, so a mount this creates
/// can't be adopted by an archive cycle mid-write, and archiveloop's
/// `umount -f -l` can't land under an in-flight backup.
///
/// If the share wasn't mounted, mounts it from fstab and unmounts it
/// again before releasing the lock — a CIFS mount left up after the
/// car drives away goes stale ("host is down") and wedges the next
/// archive cycle, which is exactly what disconnect-archive.sh exists
/// to prevent. A mount that was already up (an archive cycle's, whose
/// post-archive step is what called us) is left alone; its owner
/// unmounts it. The unmount runs even when `write` fails; an unmount
/// failure is logged but doesn't mask the write result.
///
/// The whole transaction runs in its own spawned task: if the HTTP
/// request future is cancelled (client disconnect), the lock guard must
/// not drop mid-write and skip the unmount — the task detaches and
/// finishes cleanup on its own.
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

    // Bounded wait: archiveloop holds this lock only for the seconds a
    // mount/unmount transition takes, never across a whole cycle.
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
        // findmnt is ground truth, not mount's exit status: a timed-out
        // mount(8) may still have completed the kernel transition, and
        // that mount is ours to clean up.
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
        // Mirror disconnect-archive.sh: bounded, force+lazy so a dead
        // share can't hang the API. On failure the mount lingers (same
        // exposure as a crash mid-archive) — log and move on.
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

/// Scratch path for the drive-DB snapshot. Lives on /backingfiles (the
/// big data partition) — /tmp can be RAM-backed and /mutable is small,
/// and the source DB is on the same filesystem anyway.
const DB_EXPORT_TMP: &str = "/backingfiles/.backup-drive-data.db";
const DRIVE_DB_PATH: &str = "/backingfiles/drive-data.db";

fn drive_data_filename(date: &str) -> String {
    format!("sentryusb-backup-{}.drive-data.db.gz", date)
}

/// Snapshot the drive DB (drives, charges, telemetry) into a gzipped
/// copy ready to ship next to the JSON backup. `VACUUM INTO` takes a
/// read transaction on the live DB so the copy is consistent even while
/// the sampler writes; other DB users queue for the few seconds it
/// takes, which is fine for a manual/nightly backup. Returns the .gz
/// path.
/// Marker for the last calendar day the drive-DB snapshot shipped
/// successfully (content: "<date> <location>"). Lives on /mutable like
/// `.last_hash` so recording it never touches the DB.
const DB_BACKUP_STAMP: &str = "/mutable/.last-drive-db-backup";

/// Serializes VACUUM+gzip+ship across concurrent backup calls — two
/// racers passing the date gate would otherwise fight over the shared
/// DB_EXPORT_TMP scratch file.
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
        // ship_drive_data refuses ssd targets — skip up front instead of
        // paying VACUUM+gzip for an artifact that can never ship.
        info!("[backup] drive_db_snapshot_skipped reason=ssd_location");
        return false;
    }
    let stamp = std::fs::read_to_string(DB_BACKUP_STAMP).unwrap_or_default();
    if !force && stamp.split_whitespace().next() == Some(date) {
        info!(
            "[backup] drive_db_snapshot_skipped reason=already_today date={}",
            date
        );
        // The dated companion exists from the earlier run today.
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
    // VACUUM INTO refuses to overwrite; clear leftovers from a crashed run.
    let _ = std::fs::remove_file(DB_EXPORT_TMP);
    let _ = std::fs::remove_file(&gz);

    // Dedicated connection: VACUUM INTO only reads the source DB, so the
    // copy runs without seizing the app's writer mutex.
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

/// Ship the drive-DB snapshot to the same archive destination the JSON
/// backup goes to. Deliberately NOT written to the /mutable local copy —
/// that partition is a few hundred MB and a dated DB snapshot per backup
/// would fill it. `location == "ssd"` therefore skips with an
/// explanatory error the caller logs.
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
                // Tens of MB onto a network mount — keep it off the runtime.
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
        "rclone" => {
            let drive = active.get("RCLONE_DRIVE").cloned().unwrap_or_default();
            let rclone_path = active.get("RCLONE_PATH").cloned().unwrap_or_default();
            if drive.is_empty() {
                return Err("rclone not configured".to_string());
            }
            // rclone preserves the source filename; stage under the dated
            // name so the remote gets sentryusb-backup-<date>.drive-data.db.gz.
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

    // Snapshot + ship the drive DB (drives, charges, telemetry) next to
    // the JSON backup. Runs even when the config hash is unchanged —
    // drive data changes every day the car is used, and a stale export
    // is exactly how users lose drive history on a reflash. Best-effort:
    // a DB export failure must never block the config backup.
    //
    // Gated to once per calendar day for AUTOMATIC calls: post-archive
    // invokes this endpoint after EVERY successful archive, and on a
    // grown DB (908MB measured) each VACUUM+gzip+ship was a multi-minute
    // disk storm — the dated filename overwrites same-day copies anyway,
    // so extra runs bought no recovery granularity. Manual Backup Now
    // (force=1) still ships. The job mutex serializes concurrent calls
    // over the shared scratch file; the marker advances only after a
    // successful ship.
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
                // Cloned: the write closure must be 'static so the
                // transaction task can outlive a cancelled request.
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
    invalidate_backup_list();
    (StatusCode::OK, Json(serde_json::json!({
        "success": true,
        "date": data.date,
        "location": location,
    })))
}

/// Backups change at most daily; the listing scans /backingfiles plus a
/// possibly-network archive mount (7-10s observed while an archive owned
/// the disk), so a short TTL makes repeat Settings visits instant.
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
    // Directory scans over /backingfiles and the archive mount — slow
    // while an archive run saturates the disk (observed 7+ s), so keep
    // them off the async workers.
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
        // Establish the /mutable/configs/rclone symlink first so the restore
        // never recreates a real directory on the read-only root (rclone's
        // OAuth token refresh must be able to rewrite this file later).
        // Falls back to the plain directory if the migration is unavailable
        // (e.g. /mutable not mounted) — better a restored config than none.
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
