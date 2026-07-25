//! Archive system configuration — replaces `configure.sh`.
//!
//! Sets up the archive backend (cifs, nfs, rsync, rclone, or none) by
//! verifying credentials, installing dependencies, and writing the
//! archive loop service.

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

/// Pre-populate root's known_hosts with the rsync server's SSH host key
/// so the non-interactive archiveloop SSH-via-rsync calls succeed. Without
/// this, the very first sync fails with "Host key verification failed."
/// because OpenSSH refuses to add unknown hosts in batch mode, and the
/// user has no way to accept it interactively (the call runs as root
/// inside a systemd service, not in their shell). Idempotent: ssh-keyscan
/// returns the same line on every run; we deduplicate against the
/// existing known_hosts before appending.
async fn trust_rsync_host_key(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    let server = match env.config.get("RSYNC_SERVER") {
        Some(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => return Ok(()),
    };

    let _ = std::fs::create_dir_all("/root/.ssh");
    let known_hosts_path = "/root/.ssh/known_hosts";
    let existing = std::fs::read_to_string(known_hosts_path).unwrap_or_default();

    emitter.progress(&format!("Trusting SSH host key for {}...", server));
    let scan = match sentryusb_shell::run_with_timeout(
        Duration::from_secs(15),
        "ssh-keyscan", &["-H", "-T", "5", &server],
    ).await {
        Ok(s) => s,
        Err(e) => {
            // Don't fail the whole setup if the server is currently
            // unreachable — the archive cycle will report a clearer
            // error later, and the user can re-run setup once the
            // server is online.
            emitter.progress(&format!(
                "ssh-keyscan {} failed: {}. Music sync may need a manual ssh-keyscan later.",
                server, e
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
        "Added {} host key entry/entries for {} to /root/.ssh/known_hosts",
        new_lines.len(), server
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

/// True only when BLE is being used as the keep-awake mechanism.
/// A VIN alone means BLE telemetry is set up — that doesn't block
/// other keep-awake providers or require a Sentry case.
fn ble_used_for_keep_awake(env: &SetupEnv) -> bool {
    env.is_set("TESLA_BLE_VIN")
        && matches!(
            env.config.get("BLE_KEEP_AWAKE_ENABLED").map(|s| s.trim()),
            Some("yes") | Some("true") | Some("1")
        )
}

/// Check that at most one keep-awake provider is configured.
///
/// "Configured" means present AND non-empty (see [`SetupEnv::is_set`]).
/// An empty `export KEY=''` — written when the wizard switches away from
/// a provider — does NOT count, matching the runtime in `run/awake_start`
/// (`${VAR:+x}`). Counting empties here is what bricked devices into a
/// setup boot loop after a keep-awake method change.
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

/// Ensure bluez is installed and generate the BLE keypair. Caller-
/// agnostic: invoked by `configure_tesla_ble` during setup and by the
/// settings-page lazy-install endpoint on a live system. Idempotent —
/// returns Ok(()) immediately if the keypair already exists.
///
/// No longer downloads tesla-control / tesla-keygen: keygen, pairing and
/// every BLE command are native now (the `tesla_ble` crate +
/// `sentryusb-ble-action`, which ships in the image). The function name
/// is kept so the existing `ble-install` endpoint call site is stable.
///
/// Progress messages route through `progress` so callers can dispatch
/// them to whatever surface they have (setup `SetupEmitter`,
/// WebSocket broadcast, logs).
pub async fn install_tesla_ble_binaries<F>(progress: F) -> Result<()>
where
    F: Fn(&str),
{
    if std::path::Path::new("/root/.ble/key_private.pem").exists() {
        return Ok(());
    }

    // Root partition is mounted read-only in steady state on the Pi.
    // Best-effort flip to rw so the writes below can land. No-op /
    // missing-script case (mid-pi-gen, dev machines) is harmless.
    let _ = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status();

    // Install bluez
    if sentryusb_shell::run("dpkg", &["-s", "bluez"]).await.is_err() {
        progress("Installing bluez...");
        crate::apt::apt_install(
            &progress,
            &["bluez"],
            Duration::from_secs(600),
        ).await?;
    }

    // Install pi-bluetooth if available
    if sentryusb_shell::run("bash", &["-c", "apt-cache search pi-bluetooth | grep -q pi-bluetooth"]).await.is_ok() {
        if sentryusb_shell::run("dpkg", &["-s", "pi-bluetooth"]).await.is_err() {
            let _ = crate::apt::apt_install(
                &progress,
                &["pi-bluetooth"],
                Duration::from_secs(600),
            ).await;
        }
    }

    // Generate BLE keys if they don't exist. Uses our Rust-side
    // P-256 generator (sentryusb_tesla_ble::keys::generate_keypair)
    // — no longer shells out to tesla-keygen. Writes PKCS#8 PEM for
    // the private key (vs tesla-keygen's SEC1 format); our loader
    // accepts both, so existing installs that already have a SEC1
    // key file from tesla-keygen keep working untouched.
    if !std::path::Path::new("/root/.ble/key_private.pem").exists() {
        let dir = std::path::Path::new("/root/.ble");
        sentryusb_tesla_ble::keys::generate_keypair(dir)
            .context("generating Tesla BLE keypair")?;
        std::fs::write("/root/.ble/key_pending_pairing", "")?;
        progress("Generated Tesla BLE keys. Pairing required via web UI.");
    }

    Ok(())
}

/// Configure Tesla BLE if VIN is set. Returns true if the phase did work.
///
/// Idempotent: if the binaries are already installed and keys exist, we do
/// nothing and return false so the caller can skip announcing a phase.
pub async fn configure_tesla_ble(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    // BLE is opt-in: skip entirely if no VIN is configured.
    if env
        .config
        .get("TESLA_BLE_VIN")
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        info!("Tesla BLE not enabled");
        return Ok(false);
    }

    // The only durable artifact is the keypair — keygen, pairing and
    // every command are native now (no tesla-control/tesla-keygen to
    // install). If it already exists there's nothing to do.
    if std::path::Path::new("/root/.ble/key_private.pem").exists() {
        return Ok(false);
    }

    emitter.begin_phase("tesla_ble", "Tesla BLE peripheral");
    emitter.progress("Configuring Tesla BLE...");

    install_tesla_ble_binaries(|msg| emitter.progress(msg)).await?;

    Ok(true)
}

/// Full archive configuration flow. Returns true if the phase did work.
pub async fn configure_archive(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let archive_system = ArchiveSystem::from_config(&env.get("ARCHIVE_SYSTEM", "none"))?;

    validate_wake_apis(env)?;
    validate_sentry_case(env)?;
    validate_archive_config(env, archive_system)?;

    // Idempotency: rsync installed, archive service already installed, already enabled.
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

    // Port of run/nfs_archive/verify-and-configure-archive.sh::configure_archive
    // and its cifs_archive counterpart. The bash flow always wrote an
    // `/etc/fstab` entry for mount-based archive backends; without it
    // `connect-archive.sh` (which calls `mount /mnt/archive` from fstab)
    // fails all 10 retries every archive cycle, and clips never leave
    // the Pi. `noauto` keeps the mount on-demand so boot doesn't hang
    // waiting for a NAS that's usually offline except when parked at
    // home. rsync/rclone paths don't need this — they talk directly.
    match archive_system {
        ArchiveSystem::Nfs => configure_nfs_mount(env, emitter).await?,
        ArchiveSystem::Cifs => configure_cifs_mount(env, emitter).await?,
        ArchiveSystem::Rsync => trust_rsync_host_key(env, emitter).await?,
        ArchiveSystem::Rclone => {
            // rclone rewrites rclone.conf whenever it refreshes an OAuth
            // token (e.g. Google Drive, roughly hourly); on the read-only
            // root that write fails and archiving dies once the token
            // expires. Relocate the config to /mutable behind a symlink.
            // Non-fatal: the OTA runtime patch retries on fielded devices.
            emitter.progress("Ensuring rclone config lives on /mutable...");
            if let Err(e) = ensure_rclone_config_on_mutable().await {
                emitter.progress(&format!(
                    "WARNING: could not move rclone config to /mutable: {e}"
                ));
            }
        }
        _ => {}
    }

    // Drop the per-archive-system bash helpers (archive-clips.sh,
    // archive-is-reachable.sh, etc.) into /root/bin/. archiveloop reads
    // these by fixed name regardless of which system is active, so we
    // pick the right variant based on ARCHIVE_SYSTEM. Without this,
    // archiveloop hits "command not found" on every cycle and clips
    // never leave the Pi — the Go-era pi-gen image used to bake these
    // in at build time, but `curl | bash install-pi.sh` doesn't run
    // pi-gen, so the responsibility moved to the Rust setup runner.
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

/// Ensure `/root/.config/rclone` is a symlink to `/mutable/configs/rclone`.
///
/// rclone persists refreshed OAuth tokens by rewriting `rclone.conf` in
/// place, which fails on the read-only root ("Failed to save config after
/// 10 tries: ... read-only file system"). The legacy bash installer did
/// this migration in `verify-and-configure-archive.sh`; the Rust flow
/// installs that script but never runs it, so it happens here instead.
///
/// Skips (without error) when `/mutable` can't be mounted, so a wizard run
/// on an unusual system never hard-fails on this.
pub async fn ensure_rclone_config_on_mutable() -> Result<()> {
    let link = std::path::Path::new(RCLONE_CONFIG_LINK);
    let target = std::path::Path::new(RCLONE_MUTABLE_DIR);

    // Fast path: correct symlink with an existing target.
    if std::fs::read_link(link).ok().as_deref() == Some(target) && target.is_dir() {
        return Ok(());
    }

    // /mutable must actually be the mounted partition before we move the
    // config into it, or the "migrated" config lands on the RO root.
    if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"]).await.is_err() {
        let _ = sentryusb_shell::run("mount", &["/mutable"]).await;
        if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"]).await.is_err() {
            info!("/mutable not mounted; skipping rclone config migration");
            return Ok(());
        }
    }

    // Root may be read-only on a standalone re-run. Flip rw for the
    // rename/symlink and restore ro only if it was ro before — mid-setup
    // the runner keeps root rw and later phases still need it that way.
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

/// Filesystem state machine for the migration. Pure fs operations on the
/// given paths (testable off-device). Handles: nothing yet, real directory,
/// half-migrated (both real dir and mutable copy exist), and a wrong or
/// dangling symlink.
fn migrate_rclone_config(link: &std::path::Path, target: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::{symlink, PermissionsExt};

    match std::fs::symlink_metadata(link) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Wrong or dangling symlink → point it at the mutable dir.
            std::fs::create_dir_all(target)
                .with_context(|| format!("create {}", target.display()))?;
            if std::fs::read_link(link).ok().as_deref() != Some(target) {
                std::fs::remove_file(link).with_context(|| format!("remove {}", link.display()))?;
                symlink(target, link).with_context(|| format!("symlink {}", link.display()))?;
            }
        }
        Ok(meta) if meta.is_dir() => {
            // Real directory on the RO root. `rename` can't cross the
            // filesystem boundary to /mutable, so copy then delete. If a
            // mutable copy already exists (half-migrated, or an old install
            // that lost its symlink), the mutable copy wins conflicts — it
            // holds the freshest OAuth tokens.
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
            // Nothing configured yet: pre-provision the symlink so a later
            // `rclone config` over SSH writes through to /mutable without
            // needing a read-write root.
            std::fs::create_dir_all(target)
                .with_context(|| format!("create {}", target.display()))?;
            if let Some(parent) = link.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create {}", parent.display()))?;
            }
            symlink(target, link).with_context(|| format!("symlink {}", link.display()))?;
        }
    }

    // Token material lives here — keep the dir private like ~/.ble et al.
    let _ = std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o700));
    Ok(())
}

/// Recursively copy `src` into `dst`, never overwriting an existing file in
/// `dst`. `std::fs::copy` preserves permissions (rclone.conf is 0600).
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
// Each archive backend has its own copies of these helpers under
// `run/<system>_archive/`. They share filenames; archiveloop calls them by
// fixed name (e.g. `/root/bin/archive-is-reachable.sh`). At setup time we
// drop the matching variant into `/root/bin/` based on ARCHIVE_SYSTEM. A
// follow-up wizard run with a different system swaps the files cleanly
// because we always write the full set.

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

/// Drop the per-archive-system bash helpers into /root/bin/ with mode 0755.
/// Idempotent — overwriting existing files is fine, and a stale entry from
/// a prior run with a different ARCHIVE_SYSTEM gets cleanly replaced.
fn install_archive_scripts(system: ArchiveSystem, emitter: &SetupEmitter) -> Result<()> {
    let _ = std::fs::create_dir_all("/root/bin");

    let scripts: &[(&str, &str)] = match system {
        ArchiveSystem::Cifs => &[
            ("archive-clips.sh", CIFS_ARCHIVE_CLIPS),
            ("archive-is-reachable.sh", CIFS_ARCHIVE_IS_REACHABLE),
            ("connect-archive.sh", CIFS_CONNECT_ARCHIVE),
            ("copy-music.sh", CIFS_COPY_MUSIC),
            ("disconnect-archive.sh", CIFS_DISCONNECT_ARCHIVE),
            ("verify-and-configure-archive.sh", CIFS_VERIFY_CONFIGURE),
        ],
        ArchiveSystem::Nfs => &[
            ("archive-clips.sh", NFS_ARCHIVE_CLIPS),
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

/// Ensure the named package is installed (idempotent, skips if already
/// there). Used by the on-demand archive-helper installs.
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

/// Strip any prior entry for `mount_point` with filesystem type `fstype`
/// from `/etc/fstab` and append `new_line`. Keeps the file's other
/// entries (root, /boot, /mutable, cam_disk, tmpfs, etc.) intact.
fn replace_fstab_entry(fstype: &str, mount_point: &str, new_line: &str) -> Result<()> {
    // Root was remounted read-write at the start of the setup runner,
    // but belt-and-suspenders re-remount here so a user who invokes the
    // archive phase standalone doesn't hit an EROFS.
    let _ = std::process::Command::new("mount")
        .args(["/", "-o", "remount,rw"])
        .output();

    let existing = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let mut lines: Vec<String> = existing
        .lines()
        .filter(|l| {
            // Match " nfs " / " cifs " as a whole field and the exact
            // mount point. Avoids clobbering an unrelated entry that
            // happens to mention the same substring.
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

/// Strip any prior entry for `mount_point` with filesystem type `fstype`
/// from `/etc/fstab` without writing a replacement. Used when the wizard
/// clears an optional share (e.g. MUSIC_SHARE_NAME) so the old line
/// doesn't linger and confuse archiveloop on the next mount cycle.
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

    // vers=3 + proto=tcp matches the bash flow. Broader NAS compat
    // (UniFi Drive, Synology DSM 7, TrueNAS) than defaulting to v4.2,
    // and `nolock` avoids NLM lock-server dependencies we don't need.
    let line = format!(
        "{}:{} /mnt/archive nfs rw,noauto,nolock,proto=tcp,vers=3 0 0",
        server, share
    );
    replace_fstab_entry("nfs", "/mnt/archive", &line)?;
    emitter.progress("Added NFS mount to /etc/fstab");

    // Optional read-only music share. archiveloop mounts /mnt/musicarchive
    // from this entry and copy-music.sh rsyncs it into music_disk.bin;
    // without the fstab line the mount retries and bails, so a configured
    // MUSIC_SHARE_NAME would silently never sync.
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

    // Credentials live in a 0600 file referenced by fstab so the
    // password doesn't leak into the world-readable fstab itself.
    // Matches `/root/.teslaCamArchiveCredentials` from the bash flow.
    let creds_path = "/root/.teslaCamArchiveCredentials";
    let mut creds = format!("username={}\npassword={}\n", user, pass);
    if !domain.is_empty() {
        creds.push_str(&format!("domain={}\n", domain));
    }
    std::fs::write(creds_path, creds).context("write credentials file")?;
    // `chmod 600` via shell — std::os::unix::fs::PermissionsExt isn't on
    // the Windows dev host where we cargo-check, so we keep this off the
    // std::os::unix path entirely. The setup phase only ever runs on
    // Linux at execution time, so the shell call is the real code path.
    let _ = sentryusb_shell::run("chmod", &["600", creds_path]).await;

    std::fs::create_dir_all("/mnt/archive").context("mkdir /mnt/archive")?;

    // Fstab mangles spaces in paths as \040. Preserves share names like
    // "Tesla Cam" without breaking the field split.
    let share_escaped = share.replace(' ', "\\040");
    let line = format!(
        "//{}/{} /mnt/archive cifs rw,noauto,credentials={},iocharset=utf8,file_mode=0777,dir_mode=0777,vers={} 0 0",
        server, share_escaped, creds_path, vers
    );
    replace_fstab_entry("cifs", "/mnt/archive", &line)?;
    emitter.progress("Added CIFS mount to /etc/fstab");

    // Optional music share — CIFS counterpart of the NFS music block
    // above. archiveloop's `connect-archive.sh` mounts /mnt/musicarchive
    // from this fstab entry and `copy-music.sh` rsyncs from there into
    // music_disk.bin. `ro` because we only ever read the share; reuses
    // the same credentials file as the cam share (matches the bash
    // `cifs_archive/verify-and-configure-archive.sh` flow). Without
    // this block, CIFS installs that set MUSIC_SHARE_NAME never get
    // a fstab entry, /mnt/musicarchive is never created, and music
    // sync silently never runs — only NFS users hit the working path.
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

/// Drop the /mnt/musicarchive fstab line of `fstype` (if any) and remove
/// the mount-point directory. Called when MUSIC_SHARE_NAME is cleared so
/// archiveloop stops trying to mount a share the user no longer wants.
/// `rmdir` is intentional — refuses to remove a dir that's still mounted
/// or has content, which is the safe behavior.
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
        // Reproduces the boot-loop bug ("Multiple control providers
        // configured"). The wizard "clears" a deselected keep-awake
        // provider by writing `export TESSIE_API_TOKEN=''`. An empty value
        // must NOT count as a configured provider — matching the runtime's
        // `${VAR:+x}` check in run/awake_start and the frontend's JS-falsy
        // check. Only the validator miscounted it, bricking the device.
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
        // The actual "only 1" rule: two genuinely-set providers must fail.
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
        // Config-validation failures must be a downcastable ConfigError so
        // the web server can tell "user must fix settings" apart from a
        // transient failure and stop the silent setup boot-loop.
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
        // A VIN with no BLE_KEEP_AWAKE_ENABLED is telemetry-only, so it can
        // coexist with another keep-awake provider.
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
        // Idempotent on re-run.
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
        // The conf is now reachable *through the link*.
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
        // Mutable copy wins the conflict (it has the refreshed tokens)...
        assert_eq!(
            std::fs::read_to_string(sb.target().join("rclone.conf")).unwrap(),
            "fresh-mutable-copy"
        );
        // ...but files only present on the root side are preserved.
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
