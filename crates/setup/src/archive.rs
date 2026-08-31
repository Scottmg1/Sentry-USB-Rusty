//! Archive backend validation, dependencies, mounts, helpers, and service setup.

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::env::SetupEnv;
use crate::error::ConfigError;
use crate::SetupEmitter;

/// Supported archive backends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveSystem {
    Cifs,
    Nfs,
    Rsync,
    Rclone,
    None,
}

impl ArchiveSystem {
    pub fn from_config(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "cifs" => Ok(Self::Cifs),
            "nfs" => Ok(Self::Nfs),
            "rsync" => Ok(Self::Rsync),
            "rclone" => Ok(Self::Rclone),
            "none" | "" => Ok(Self::None),
            other => Err(ConfigError(format!("Unrecognized archive system: {other}")).into()),
        }
    }
}

/// Validate that required config variables are present for the chosen archive system.
fn validate_archive_config(env: &SetupEnv, system: ArchiveSystem) -> Result<()> {
    let require = |key: &str| -> Result<()> {
        if env.config.get(key).map_or(true, |v| v.is_empty()) {
            return Err(
                ConfigError(format!("Required config variable {key} is not set")).into(),
            );
        }
        Ok(())
    };

    match system {
        ArchiveSystem::Rsync => {
            require("RSYNC_USER")?;
            require("RSYNC_SERVER")?;
            require("RSYNC_PATH")?;
            // Reject invalid explicit ports instead of silently using 22.
            sentryusb_config::validate_rsync_ssh_port(&env.config).map_err(ConfigError)?;
        }
        ArchiveSystem::Rclone => {
            require("RCLONE_DRIVE")?;
            require("RCLONE_PATH")?;
        }
        ArchiveSystem::Cifs => {
            require("SHARE_NAME")?;
            require("SHARE_USER")?;
            require("SHARE_PASSWORD")?;
            require("ARCHIVE_SERVER")?;
        }
        ArchiveSystem::Nfs => {
            require("SHARE_NAME")?;
            require("ARCHIVE_SERVER")?;
        }
        ArchiveSystem::None => {}
    }

    Ok(())
}

/// Adds the rsync server's host key for noninteractive archive jobs, using
/// OpenSSH's configured-port notation and deduplicating existing entries.
async fn trust_rsync_host_key(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    let server = match env.config.get("RSYNC_SERVER") {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Ok(()),
    };
    let port = sentryusb_config::rsync_ssh_port(&env.config);
    let port_flags = sentryusb_shell::SshPort::new(port);
    // Match OpenSSH's nondefault-port host notation.
    let endpoint = match port {
        Some(p) => format!("[{}]:{}", server, p),
        None => server.clone(),
    };

    let _ = std::fs::create_dir_all("/root/.ssh");
    let known_hosts_path = "/root/.ssh/known_hosts";
    let existing = std::fs::read_to_string(known_hosts_path).unwrap_or_default();

    emitter.progress(&format!("Trusting SSH host key for {}...", endpoint));
    let mut args: Vec<&str> = vec!["-H", "-T", "5"];
    args.extend(port_flags.ssh_args());
    args.push(&server);
    let scan = match sentryusb_shell::run_with_timeout(
        Duration::from_secs(15),
        "ssh-keyscan", &args,
    ).await {
        Ok(s) => s,
        Err(e) => {
            // Unreachable archive servers remain a warning so setup can finish.
            let manual = match port {
                Some(p) => format!("ssh-keyscan -p {} -H {}", p, server),
                None => format!("ssh-keyscan -H {}", server),
            };
            emitter.progress(&format!(
                "ssh-keyscan {} failed: {}. Archiving will fail with \"Host key \
                 verification failed\" until the key is trusted. Re-run setup once \
                 the server is up, or run: {} >> {}",
                endpoint, e, manual, known_hosts_path
            ));
            return Ok(());
        }
    };

    let mut new_lines: Vec<&str> = Vec::new();
    for line in scan.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if !existing.lines().any(|e| e.trim() == line) {
            new_lines.push(line);
        }
    }

    if new_lines.is_empty() {
        return Ok(());
    }

    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    for l in &new_lines {
        updated.push_str(l);
        updated.push('\n');
    }
    std::fs::write(known_hosts_path, updated)?;
    let _ = sentryusb_shell::run("chmod", &["600", known_hosts_path]).await;
    emitter.progress(&format!(
        "Added {} host key entry/entries for {} to {}",
        new_lines.len(), endpoint, known_hosts_path
    ));
    Ok(())
}

/// Ensure rsync is installed. Silent when already present.
async fn ensure_rsync(emitter: &SetupEmitter) -> Result<()> {
    if sentryusb_shell::run("which", &["rsync"]).await.is_ok() {
        return Ok(());
    }
    emitter.progress("Installing rsync...");
    crate::apt::apt_install(
        |m| emitter.progress(m),
        &["rsync"],
        Duration::from_secs(600),
    ).await.context("failed to install rsync")?;
    Ok(())
}

/// A VIN alone enables telemetry; the explicit flag selects BLE keep-awake.
fn ble_used_for_keep_awake(env: &SetupEnv) -> bool {
    env.is_set("TESLA_BLE_VIN")
        && matches!(
            env.config.get("BLE_KEEP_AWAKE_ENABLED").map(|s| s.trim()),
            Some("yes") | Some("true") | Some("1")
        )
}

/// Requires at most one nonempty keep-awake provider, matching runtime checks.
fn validate_wake_apis(env: &SetupEnv) -> Result<()> {
    let mut providers = Vec::new();
    if env.is_set("TESSIE_API_TOKEN") {
        providers.push("Tessie");
    }
    if env.is_set("TESLAFI_API_TOKEN") {
        providers.push("TeslaFi");
    }
    if ble_used_for_keep_awake(env) {
        providers.push("BLE");
    }
    if env.is_set("KEEP_AWAKE_WEBHOOK_URL") {
        providers.push("Webhook");
    }
    if providers.len() > 1 {
        return Err(ConfigError(format!(
            "Multiple keep-awake providers configured ({}) — only 1 can be enabled \
             at a time. Edit /root/sentryusb.conf and keep just one.",
            providers.join(", ")
        ))
        .into());
    }
    Ok(())
}

/// Validate SENTRY_CASE value if any wake API is enabled.
fn validate_sentry_case(env: &SetupEnv) -> Result<()> {
    let has_api = env.is_set("TESSIE_API_TOKEN")
        || env.is_set("TESLAFI_API_TOKEN")
        || ble_used_for_keep_awake(env)
        || env.is_set("KEEP_AWAKE_WEBHOOK_URL");

    if has_api {
        let case = env.get("SENTRY_CASE", "");
        if !["1", "2", "3"].contains(&case.as_str()) {
            return Err(ConfigError(
                "SENTRY_CASE must be 1, 2, or 3 when a wake API is configured".into(),
            )
            .into());
        }
    }
    Ok(())
}

/// Whether both Tesla BLE key files are valid and belong to the same pair.
pub fn tesla_ble_keypair_is_valid() -> bool {
    sentryusb_tesla_ble::keys::load_keypair(std::path::Path::new("/root/.ble")).is_ok()
}

static TESLA_BLE_INSTALL_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Installs BlueZ and generates or repairs a native BLE keypair when needed.
pub async fn install_tesla_ble_binaries<F>(progress: F) -> Result<()>
where
    F: Fn(&str),
{
    // Serialise API/setup callers before package installation as well as key
    // publication. The on-disk flock in tesla_ble also covers other processes.
    let _guard = TESLA_BLE_INSTALL_LOCK.lock().await;
    let key_dir = std::path::Path::new("/root/.ble");
    let private_key = key_dir.join("key_private.pem");
    if tesla_ble_keypair_is_valid() {
        return Ok(());
    }
    let had_usable_private_key = sentryusb_tesla_ble::keys::key_is_usable(&private_key);

    // Persist keys on read-only-root installations.
    let _ = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status();

    if sentryusb_shell::run("dpkg", &["-s", "bluez"]).await.is_err() {
        progress("Installing bluez...");
        crate::apt::apt_install(
            &progress,
            &["bluez"],
            Duration::from_secs(600),
        ).await?;
    }

    // `pi-bluetooth` is optional on non-Raspberry Pi distributions.
    if sentryusb_shell::run("bash", &["-c", "apt-cache search pi-bluetooth | grep -q pi-bluetooth"]).await.is_ok() {
        if sentryusb_shell::run("dpkg", &["-s", "pi-bluetooth"]).await.is_err() {
            let _ = crate::apt::apt_install(
                &progress,
                &["pi-bluetooth"],
                Duration::from_secs(600),
            ).await;
        }
    }

    sentryusb_tesla_ble::keys::regenerate_keypair(key_dir)
        .context("generating or repairing Tesla BLE keypair")?;
    if had_usable_private_key {
        progress("Repaired Tesla BLE public key from the existing private key.");
    } else {
        progress("Generated Tesla BLE keys. Pairing required via web UI.");
    }

    Ok(())
}

/// Configures opted-in Tesla BLE and reports whether work was needed.
pub async fn configure_tesla_ble(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    // A configured VIN is the setup-time opt-in signal.
    if env
        .config
        .get("TESLA_BLE_VIN")
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        info!("Tesla BLE not enabled");
        return Ok(false);
    }

    // Native action binaries ship with the image; only the keypair is durable.
    if tesla_ble_keypair_is_valid() {
        return Ok(false);
    }

    emitter.begin_phase("tesla_ble", "Tesla BLE peripheral");
    emitter.progress("Configuring Tesla BLE...");

    install_tesla_ble_binaries(|msg| emitter.progress(msg)).await?;

    Ok(true)
}

/// Configures the selected archive backend and reports whether work was needed.
pub async fn configure_archive(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let archive_system = ArchiveSystem::from_config(&env.get("ARCHIVE_SYSTEM", "none"))?;

    validate_wake_apis(env)?;
    validate_sentry_case(env)?;
    validate_archive_config(env, archive_system)?;

    // Skip a complete, enabled installation.
    let rsync_ok = sentryusb_shell::run("which", &["rsync"]).await.is_ok();
    let service_path = std::path::Path::new("/lib/systemd/system/sentryusb-archive.service");
    let service_enabled = sentryusb_shell::run(
        "systemctl", &["is-enabled", "sentryusb-archive.service"],
    ).await.is_ok();

    if rsync_ok && service_path.exists() && service_enabled && archive_system == ArchiveSystem::None {
        return Ok(false);
    }

    emitter.begin_phase("archive", "Archive configuration");
    emitter.progress(&format!("Configuring archive system: {:?}", archive_system));

    ensure_rsync(emitter).await?;

    // Mount backends require on-demand fstab entries; remote tools connect directly.
    match archive_system {
        ArchiveSystem::Nfs => configure_nfs_mount(env, emitter).await?,
        ArchiveSystem::Cifs => configure_cifs_mount(env, emitter).await?,
        ArchiveSystem::Rsync => trust_rsync_host_key(env, emitter).await?,
        ArchiveSystem::Rclone => {
            // OAuth refreshes require rclone config to live on writable storage.
            emitter.progress("Ensuring rclone config lives on /mutable...");
            if let Err(e) = ensure_rclone_config_on_mutable().await {
                emitter.progress(&format!(
                    "WARNING: could not move rclone config to /mutable: {e}"
                ));
            }
        }
        _ => {}
    }

    // Install the selected backend under archiveloop's fixed helper names.
    install_archive_scripts(archive_system, emitter)?;

    crate::system::install_archive_service()?;
    let _ = sentryusb_shell::run("systemctl", &["daemon-reload"]).await;
    let _ = sentryusb_shell::run("systemctl", &["enable", "sentryusb-archive.service"]).await;

    emitter.progress("Archive configuration complete.");
    Ok(true)
}

// ── rclone config on /mutable ─────────────────────────────────────────────

const RCLONE_CONFIG_LINK: &str = "/root/.config/rclone";
const RCLONE_MUTABLE_DIR: &str = "/mutable/configs/rclone";

/// Ensures rclone's writable OAuth state is symlinked into `/mutable`.
pub async fn ensure_rclone_config_on_mutable() -> Result<()> {
    let link = std::path::Path::new(RCLONE_CONFIG_LINK);
    let target = std::path::Path::new(RCLONE_MUTABLE_DIR);

    // A correct, populated link needs no migration.
    if std::fs::read_link(link).ok().as_deref() == Some(target) && target.is_dir() {
        return Ok(());
    }

    // Never migrate until `/mutable` is actually mounted.
    if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"]).await.is_err() {
        let _ = sentryusb_shell::run("mount", &["/mutable"]).await;
        if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"]).await.is_err() {
            info!("/mutable not mounted; skipping rclone config migration");
            return Ok(());
        }
    }

    // Preserve the caller's root mount mode across a standalone migration.
    let was_ro = root_mounted_readonly();
    let _ = std::process::Command::new("bash")
        .args([
            "-c",
            "/root/bin/remountfs_rw 2>/dev/null || mount -o remount,rw / 2>/dev/null || true",
        ])
        .status();

    let result = migrate_rclone_config(link, target);

    if was_ro {
        let _ = std::process::Command::new("sh")
            .args(["-c", "mount -o remount,ro / 2>/dev/null || true"])
            .status();
    }
    result
}

fn root_mounted_readonly() -> bool {
    std::process::Command::new("findmnt")
        .args(["-no", "OPTIONS", "/"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|opts| opts.trim().split(',').any(|o| o == "ro"))
        .unwrap_or(false)
}

/// Migrates absent, real-directory, partial, and incorrect-symlink states.
fn migrate_rclone_config(link: &std::path::Path, target: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{symlink, PermissionsExt};

    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Repair wrong or dangling links.
            std::fs::create_dir_all(target)
                .with_context(|| format!("create {}", target.display()))?;
            if std::fs::read_link(link).ok().as_deref() != Some(target) {
                std::fs::remove_file(link).with_context(|| format!("remove {}", link.display()))?;
                symlink(target, link).with_context(|| format!("symlink {}", link.display()))?;
            }
        }
        Ok(meta) if meta.is_dir() => {
            // Cross-filesystem migration copies; mutable files win conflicts.
            copy_tree_no_clobber(link, target)?;
            std::fs::remove_dir_all(link).with_context(|| format!("remove {}", link.display()))?;
            symlink(target, link).with_context(|| format!("symlink {}", link.display()))?;
        }
        Ok(_) => {
            anyhow::bail!(
                "{} exists but is neither a directory nor a symlink",
                link.display()
            );
        }
        Err(_) => {
            // Pre-provision the link before the first interactive configuration.
            std::fs::create_dir_all(target)
                .with_context(|| format!("create {}", target.display()))?;
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            symlink(target, link).with_context(|| format!("symlink {}", link.display()))?;
        }
    }

    // Protect token material.
    let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Recursively copies missing files while preserving destination conflicts.
fn copy_tree_no_clobber(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("create {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("read {}", src.display()))? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        let ty = entry.file_type()?;
        if ty.is_dir() {
            copy_tree_no_clobber(&entry.path(), &to)?;
        } else if !to.exists() {
            std::fs::copy(entry.path(), &to)
                .with_context(|| format!("copy {} -> {}", entry.path().display(), to.display()))?;
        }
    }
    Ok(())
}

// ── Per-archive-system bash helper scripts ────────────────────────────────
//
// Install each backend's scripts under archiveloop's fixed helper names.

// Shared by CIFS and NFS clip archival.
const MOUNTED_ARCHIVE_MONITOR: &str = include_str!("../../../run/mounted-archive-monitor.sh");

const CIFS_ARCHIVE_CLIPS: &str = include_str!("../../../run/cifs_archive/archive-clips.sh");
const CIFS_ARCHIVE_IS_REACHABLE: &str = include_str!("../../../run/cifs_archive/archive-is-reachable.sh");
const CIFS_CONNECT_ARCHIVE: &str = include_str!("../../../run/cifs_archive/connect-archive.sh");
const CIFS_COPY_MUSIC: &str = include_str!("../../../run/cifs_archive/copy-music.sh");
const CIFS_DISCONNECT_ARCHIVE: &str = include_str!("../../../run/cifs_archive/disconnect-archive.sh");
const CIFS_VERIFY_CONFIGURE: &str = include_str!("../../../run/cifs_archive/verify-and-configure-archive.sh");

const NFS_ARCHIVE_CLIPS: &str = include_str!("../../../run/nfs_archive/archive-clips.sh");
const NFS_ARCHIVE_IS_REACHABLE: &str = include_str!("../../../run/nfs_archive/archive-is-reachable.sh");
const NFS_CONNECT_ARCHIVE: &str = include_str!("../../../run/nfs_archive/connect-archive.sh");
const NFS_COPY_MUSIC: &str = include_str!("../../../run/nfs_archive/copy-music.sh");
const NFS_DISCONNECT_ARCHIVE: &str = include_str!("../../../run/nfs_archive/disconnect-archive.sh");
const NFS_VERIFY_CONFIGURE: &str = include_str!("../../../run/nfs_archive/verify-and-configure-archive.sh");

const RSYNC_ARCHIVE_CLIPS: &str = include_str!("../../../run/rsync_archive/archive-clips.sh");
const RSYNC_ARCHIVE_IS_REACHABLE: &str = include_str!("../../../run/rsync_archive/archive-is-reachable.sh");
const RSYNC_CONNECT_ARCHIVE: &str = include_str!("../../../run/rsync_archive/connect-archive.sh");
const RSYNC_COPY_MUSIC: &str = include_str!("../../../run/rsync_archive/copy-music.sh");
const RSYNC_DISCONNECT_ARCHIVE: &str = include_str!("../../../run/rsync_archive/disconnect-archive.sh");
const RSYNC_VERIFY_CONFIGURE: &str = include_str!("../../../run/rsync_archive/verify-and-configure-archive.sh");

const RCLONE_ARCHIVE_CLIPS: &str = include_str!("../../../run/rclone_archive/archive-clips.sh");
const RCLONE_ARCHIVE_IS_REACHABLE: &str = include_str!("../../../run/rclone_archive/archive-is-reachable.sh");
const RCLONE_CONNECT_ARCHIVE: &str = include_str!("../../../run/rclone_archive/connect-archive.sh");
const RCLONE_DISCONNECT_ARCHIVE: &str = include_str!("../../../run/rclone_archive/disconnect-archive.sh");
const RCLONE_VERIFY_CONFIGURE: &str = include_str!("../../../run/rclone_archive/verify-and-configure-archive.sh");

const NONE_ARCHIVE_CLIPS: &str = include_str!("../../../run/none_archive/archive-clips.sh");
const NONE_ARCHIVE_IS_REACHABLE: &str = include_str!("../../../run/none_archive/archive-is-reachable.sh");
const NONE_CONNECT_ARCHIVE: &str = include_str!("../../../run/none_archive/connect-archive.sh");
const NONE_DISCONNECT_ARCHIVE: &str = include_str!("../../../run/none_archive/disconnect-archive.sh");
const NONE_VERIFY_CONFIGURE: &str = include_str!("../../../run/none_archive/verify-and-configure-archive.sh");

/// Installs the selected backend's helpers with mode 0755.
fn install_archive_scripts(system: ArchiveSystem, emitter: &SetupEmitter) -> Result<()> {
    let _ = std::fs::create_dir_all("/root/bin");

    let scripts: &[(&str, &str)] = match system {
        ArchiveSystem::Cifs => &[
            ("archive-clips.sh", CIFS_ARCHIVE_CLIPS),
            ("mounted-archive-monitor.sh", MOUNTED_ARCHIVE_MONITOR),
            ("archive-is-reachable.sh", CIFS_ARCHIVE_IS_REACHABLE),
            ("connect-archive.sh", CIFS_CONNECT_ARCHIVE),
            ("copy-music.sh", CIFS_COPY_MUSIC),
            ("disconnect-archive.sh", CIFS_DISCONNECT_ARCHIVE),
            ("verify-and-configure-archive.sh", CIFS_VERIFY_CONFIGURE),
        ],
        ArchiveSystem::Nfs => &[
            ("archive-clips.sh", NFS_ARCHIVE_CLIPS),
            ("mounted-archive-monitor.sh", MOUNTED_ARCHIVE_MONITOR),
            ("archive-is-reachable.sh", NFS_ARCHIVE_IS_REACHABLE),
            ("connect-archive.sh", NFS_CONNECT_ARCHIVE),
            ("copy-music.sh", NFS_COPY_MUSIC),
            ("disconnect-archive.sh", NFS_DISCONNECT_ARCHIVE),
            ("verify-and-configure-archive.sh", NFS_VERIFY_CONFIGURE),
        ],
        ArchiveSystem::Rsync => &[
            ("archive-clips.sh", RSYNC_ARCHIVE_CLIPS),
            ("archive-is-reachable.sh", RSYNC_ARCHIVE_IS_REACHABLE),
            ("connect-archive.sh", RSYNC_CONNECT_ARCHIVE),
            ("copy-music.sh", RSYNC_COPY_MUSIC),
            ("disconnect-archive.sh", RSYNC_DISCONNECT_ARCHIVE),
            ("verify-and-configure-archive.sh", RSYNC_VERIFY_CONFIGURE),
        ],
        ArchiveSystem::Rclone => &[
            ("archive-clips.sh", RCLONE_ARCHIVE_CLIPS),
            ("archive-is-reachable.sh", RCLONE_ARCHIVE_IS_REACHABLE),
            ("connect-archive.sh", RCLONE_CONNECT_ARCHIVE),
            ("disconnect-archive.sh", RCLONE_DISCONNECT_ARCHIVE),
            ("verify-and-configure-archive.sh", RCLONE_VERIFY_CONFIGURE),
        ],
        ArchiveSystem::None => &[
            ("archive-clips.sh", NONE_ARCHIVE_CLIPS),
            ("archive-is-reachable.sh", NONE_ARCHIVE_IS_REACHABLE),
            ("connect-archive.sh", NONE_CONNECT_ARCHIVE),
            ("disconnect-archive.sh", NONE_DISCONNECT_ARCHIVE),
            ("verify-and-configure-archive.sh", NONE_VERIFY_CONFIGURE),
        ],
    };

    for (name, content) in scripts {
        let path = format!("/root/bin/{}", name);
        std::fs::write(&path, *content)
            .with_context(|| format!("write {}", path))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
        }
    }

    emitter.progress(&format!("Installed {} archive helper scripts", scripts.len()));
    Ok(())
}

/// Installs a missing archive-helper package.
async fn ensure_pkg(pkg: &str, emitter: &SetupEmitter) -> Result<()> {
    if sentryusb_shell::run("dpkg", &["-s", pkg]).await.is_ok() {
        return Ok(());
    }
    emitter.progress(&format!("Installing {}...", pkg));
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(240),
        "apt-get",
        &[
            "-o", "DPkg::Lock::Timeout=180",
            "install", "-y", "--no-install-recommends", pkg,
        ],
    )
    .await
    .with_context(|| format!("failed to install {}", pkg))?;
    Ok(())
}

/// Replaces one fstab entry identified by exact mount point and filesystem type.
fn replace_fstab_entry(fstype: &str, mount_point: &str, new_line: &str) -> Result<()> {
    // Support standalone invocation on read-only-root systems.
    let _ = std::process::Command::new("mount")
        .args(["/", "-o", "remount,rw"])
        .output();

    let existing = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| {
            // Compare parsed fields to avoid substring collisions.
            let fields: Vec<&str> = l.split_whitespace().collect();
            !(fields.len() >= 3 && fields[1] == mount_point && fields[2] == fstype)
        })
        .map(|s| s.to_string())
        .collect();
    lines.push(new_line.to_string());
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write("/etc/fstab", out).context("write /etc/fstab")?;
    Ok(())
}

/// Removes one exact fstab entry when an optional share is cleared.
fn remove_fstab_entry(fstype: &str, mount_point: &str) -> Result<()> {
    let _ = std::process::Command::new("mount")
        .args(["/", "-o", "remount,rw"])
        .output();

    let existing = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let kept: Vec<String> = existing
        .lines()
        .filter(|l| {
            let fields: Vec<&str> = l.split_whitespace().collect();
            !(fields.len() >= 3 && fields[1] == mount_point && fields[2] == fstype)
        })
        .map(|s| s.to_string())
        .collect();
    if kept.len() == existing.lines().count() {
        return Ok(());
    }
    let mut out = kept.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write("/etc/fstab", out).context("write /etc/fstab")?;
    Ok(())
}

async fn configure_nfs_mount(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    let server = env.get("ARCHIVE_SERVER", "");
    let share = env.get("SHARE_NAME", "");
    if server.is_empty() || share.is_empty() {
        return Ok(());
    }

    ensure_pkg("nfs-common", emitter).await?;
    std::fs::create_dir_all("/mnt/archive").context("mkdir /mnt/archive")?;

    // NFSv3 over TCP broadens NAS compatibility; archive I/O needs no NLM.
    let line = format!(
        "{}:{} /mnt/archive nfs rw,noauto,nolock,proto=tcp,vers=3 0 0",
        server, share
    );
    replace_fstab_entry("nfs", "/mnt/archive", &line)?;
    emitter.progress("Added NFS mount to /etc/fstab");

    // Music sync uses a separate read-only on-demand mount.
    let music_share = env.get("MUSIC_SHARE_NAME", "");
    if music_share.is_empty() {
        clear_music_archive_mount("nfs", emitter)?;
        return Ok(());
    }
    std::fs::create_dir_all("/mnt/musicarchive").context("mkdir /mnt/musicarchive")?;
    let music_line =
        format!("{server}:{music_share} /mnt/musicarchive nfs ro,noauto,nolock,proto=tcp,vers=3 0 0");
    replace_fstab_entry("nfs", "/mnt/musicarchive", &music_line)?;
    emitter.progress("Added NFS music mount to /etc/fstab");
    Ok(())
}

async fn configure_cifs_mount(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    let server = env.get("ARCHIVE_SERVER", "");
    let share = env.get("SHARE_NAME", "");
    let user = env.get("SHARE_USER", "");
    let pass = env.get("SHARE_PASSWORD", "");
    let domain = env.get("SHARE_DOMAIN", "");
    let vers = env.get("CIFS_VERSION", "3.0");
    if server.is_empty() || share.is_empty() || user.is_empty() || pass.is_empty() {
        return Ok(());
    }

    ensure_pkg("cifs-utils", emitter).await?;

    // Keep credentials out of world-readable fstab.
    let creds_path = "/root/.teslaCamArchiveCredentials";
    let mut creds = format!("username={}\npassword={}\n", user, pass);
    if !domain.is_empty() {
        creds.push_str(&format!("domain={}\n", domain));
    }
    std::fs::write(creds_path, creds).context("write credentials file")?;
    // Use the runtime's Linux `chmod` without adding platform-specific Rust APIs.
    let _ = sentryusb_shell::run("chmod", &["600", creds_path]).await;

    std::fs::create_dir_all("/mnt/archive").context("mkdir /mnt/archive")?;

    // Escape spaces for fstab field parsing.
    let share_escaped = share.replace(' ', "\\040");
    let line = format!(
        "//{}/{} /mnt/archive cifs rw,noauto,credentials={},iocharset=utf8,file_mode=0777,dir_mode=0777,vers={} 0 0",
        server, share_escaped, creds_path, vers
    );
    replace_fstab_entry("cifs", "/mnt/archive", &line)?;
    emitter.progress("Added CIFS mount to /etc/fstab");

    // CIFS music sync reuses credentials through a read-only on-demand mount.
    let music_share = env.get("MUSIC_SHARE_NAME", "");
    if !music_share.is_empty() {
        std::fs::create_dir_all("/mnt/musicarchive").context("mkdir /mnt/musicarchive")?;
        let music_escaped = music_share.replace(' ', "\\040");
        let music_line = format!(
            "//{}/{} /mnt/musicarchive cifs ro,noauto,credentials={},iocharset=utf8,file_mode=0777,dir_mode=0777,vers={} 0 0",
            server, music_escaped, creds_path, vers
        );
        replace_fstab_entry("cifs", "/mnt/musicarchive", &music_line)?;
        emitter.progress("Added CIFS music mount to /etc/fstab");
    } else {
        clear_music_archive_mount("cifs", emitter)?;
    }
    Ok(())
}

/// Removes a cleared music share; `rmdir` safely refuses mounted/nonempty paths.
fn clear_music_archive_mount(fstype: &str, emitter: &SetupEmitter) -> Result<()> {
    let path = "/mnt/musicarchive";
    remove_fstab_entry(fstype, path)?;
    if std::path::Path::new(path).is_dir() {
        if std::fs::remove_dir(path).is_ok() {
            emitter.progress("Removed stale /mnt/musicarchive (MUSIC_SHARE_NAME unset)");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::{PiModel, SetupEnv};
    use std::collections::HashMap;

    fn env_with(pairs: &[(&str, &str)]) -> SetupEnv {
        let config: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        SetupEnv {
            pi_model: PiModel::Other,
            boot_path: String::new(),
            cmdline_path: None,
            piconfig_path: None,
            boot_disk: None,
            root_partition: None,
            data_drive: None,
            config,
        }
    }

    #[test]
    fn empty_provider_tokens_do_not_count_as_configured() {
        let env = env_with(&[
            ("TESLA_BLE_VIN", "5YJ3E1EA4JF000001"),
            ("BLE_KEEP_AWAKE_ENABLED", "yes"),
            ("TESSIE_API_TOKEN", ""),
            ("TESLAFI_API_TOKEN", ""),
            ("KEEP_AWAKE_WEBHOOK_URL", ""),
        ]);
        assert!(
            validate_wake_apis(&env).is_ok(),
            "BLE keep-awake plus empty (cleared) provider tokens must validate"
        );
    }

    #[test]
    fn two_real_providers_are_rejected() {
        let env = env_with(&[
            ("TESLA_BLE_VIN", "5YJ3E1EA4JF000001"),
            ("BLE_KEEP_AWAKE_ENABLED", "yes"),
            ("TESSIE_API_TOKEN", "real-token"),
        ]);
        let err = validate_wake_apis(&env).unwrap_err().to_string();
        assert!(err.contains("BLE") && err.contains("Tessie"), "error names both: {err}");
    }

    #[test]
    fn provider_conflict_is_a_config_error() {
        let env = env_with(&[
            ("TESLA_BLE_VIN", "5YJ3E1EA4JF000001"),
            ("BLE_KEEP_AWAKE_ENABLED", "yes"),
            ("TESSIE_API_TOKEN", "real-token"),
        ]);
        let err = validate_wake_apis(&env).unwrap_err();
        assert!(
            err.downcast_ref::<crate::error::ConfigError>().is_some(),
            "provider conflict must be a ConfigError, got: {err:?}"
        );
    }

    #[test]
    fn missing_sentry_case_is_a_config_error() {
        let env = env_with(&[("TESSIE_API_TOKEN", "real-token")]);
        let err = validate_sentry_case(&env).unwrap_err();
        assert!(
            err.downcast_ref::<crate::error::ConfigError>().is_some(),
            "missing SENTRY_CASE must be a ConfigError, got: {err:?}"
        );
    }

    #[test]
    fn single_real_provider_is_ok() {
        let env = env_with(&[("TESSIE_API_TOKEN", "real-token")]);
        assert!(validate_wake_apis(&env).is_ok());
    }

    #[test]
    fn bare_vin_is_telemetry_only_not_a_provider() {
        let env = env_with(&[
            ("TESLA_BLE_VIN", "5YJ3E1EA4JF000001"),
            ("TESSIE_API_TOKEN", "real-token"),
        ]);
        assert!(validate_wake_apis(&env).is_ok());
    }

    #[test]
    fn whitespace_only_token_does_not_count() {
        let env = env_with(&[
            ("TESSIE_API_TOKEN", "   "),
            ("KEEP_AWAKE_WEBHOOK_URL", "http://ha.local/hook"),
        ]);
        assert!(validate_wake_apis(&env).is_ok());
    }

    // ── rsync SSH port ───────────────────────────────────────────────────

    fn rsync_env(port: Option<&str>) -> SetupEnv {
        let mut pairs = vec![
            ("ARCHIVE_SYSTEM", "rsync"),
            ("RSYNC_USER", "sentryusb"),
            ("RSYNC_SERVER", "sentryusb.example.com"),
            ("RSYNC_PATH", "/mnt/user/TeslaCam"),
        ];
        if let Some(p) = port {
            pairs.push(("RSYNC_SSH_PORT", p));
        }
        env_with(&pairs)
    }

    #[test]
    fn rsync_ssh_port_is_optional() {
        assert!(validate_archive_config(&rsync_env(None), ArchiveSystem::Rsync).is_ok());
        assert!(validate_archive_config(&rsync_env(Some("")), ArchiveSystem::Rsync).is_ok());
    }

    #[test]
    fn custom_rsync_ssh_port_validates() {
        assert!(validate_archive_config(&rsync_env(Some("23232")), ArchiveSystem::Rsync).is_ok());
    }

    #[test]
    fn invalid_rsync_ssh_port_is_a_config_error() {
        let err = validate_archive_config(&rsync_env(Some("not-a-port")), ArchiveSystem::Rsync)
            .unwrap_err();
        assert!(
            err.downcast_ref::<crate::error::ConfigError>().is_some(),
            "bad RSYNC_SSH_PORT must be a ConfigError, got: {err:?}"
        );
    }

    // ── migrate_rclone_config state machine ──────────────────────────────

    struct MigrationSandbox {
        root: std::path::PathBuf,
    }

    impl MigrationSandbox {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "sentryusb-rclone-migrate-{}-{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }
        fn link(&self) -> std::path::PathBuf {
            self.root.join("root/.config/rclone")
        }
        fn target(&self) -> std::path::PathBuf {
            self.root.join("mutable/configs/rclone")
        }
    }

    impl Drop for MigrationSandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn assert_migrated(sb: &MigrationSandbox) {
        assert_eq!(
            std::fs::read_link(sb.link()).unwrap(),
            sb.target(),
            "link must point at the mutable dir"
        );
        assert!(sb.target().is_dir());
    }

    #[test]
    fn migrate_provisions_symlink_when_nothing_exists() {
        let sb = MigrationSandbox::new("fresh");
        migrate_rclone_config(&sb.link(), &sb.target()).unwrap();
        assert_migrated(&sb);
        migrate_rclone_config(&sb.link(), &sb.target()).unwrap();
        assert_migrated(&sb);
    }

    #[test]
    fn migrate_moves_real_directory() {
        let sb = MigrationSandbox::new("realdir");
        std::fs::create_dir_all(sb.link()).unwrap();
        std::fs::write(sb.link().join("rclone.conf"), "[gdrive]\ntoken=old\n").unwrap();
        migrate_rclone_config(&sb.link(), &sb.target()).unwrap();
        assert_migrated(&sb);
        assert_eq!(
            std::fs::read_to_string(sb.target().join("rclone.conf")).unwrap(),
            "[gdrive]\ntoken=old\n"
        );
        assert!(sb.link().join("rclone.conf").exists());
    }

    #[test]
    fn migrate_half_migrated_keeps_mutable_copy() {
        let sb = MigrationSandbox::new("half");
        std::fs::create_dir_all(sb.link()).unwrap();
        std::fs::write(sb.link().join("rclone.conf"), "stale-root-copy").unwrap();
        std::fs::write(sb.link().join("only-on-root.txt"), "keep me").unwrap();
        std::fs::create_dir_all(sb.target()).unwrap();
        std::fs::write(sb.target().join("rclone.conf"), "fresh-mutable-copy").unwrap();
        migrate_rclone_config(&sb.link(), &sb.target()).unwrap();
        assert_migrated(&sb);
        assert_eq!(
            std::fs::read_to_string(sb.target().join("rclone.conf")).unwrap(),
            "fresh-mutable-copy"
        );
        assert_eq!(
            std::fs::read_to_string(sb.target().join("only-on-root.txt")).unwrap(),
            "keep me"
        );
    }

    #[test]
    fn migrate_fixes_wrong_or_dangling_symlink() {
        let sb = MigrationSandbox::new("dangling");
        std::fs::create_dir_all(sb.link().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(sb.root.join("nowhere"), sb.link()).unwrap();
        migrate_rclone_config(&sb.link(), &sb.target()).unwrap();
        assert_migrated(&sb);
    }

    #[test]
    fn migrate_correct_symlink_is_noop() {
        let sb = MigrationSandbox::new("noop");
        std::fs::create_dir_all(sb.target()).unwrap();
        std::fs::write(sb.target().join("rclone.conf"), "token").unwrap();
        std::fs::create_dir_all(sb.link().parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(sb.target(), sb.link()).unwrap();
        migrate_rclone_config(&sb.link(), &sb.target()).unwrap();
        assert_migrated(&sb);
        assert_eq!(
            std::fs::read_to_string(sb.target().join("rclone.conf")).unwrap(),
            "token"
        );
    }
}
