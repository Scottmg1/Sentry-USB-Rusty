//! Archive system configuration — replaces `configure.sh`.
//!
//! Sets up the archive backend (cifs, nfs, rsync, rclone, or none) by
//! verifying credentials, installing dependencies, and writing the
//! archive loop service.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::env::SetupEnv;
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
            other => bail!("Unrecognized archive system: {}", other),
        }
    }
}

/// Validate that required config variables are present for the chosen archive system.
fn validate_archive_config(env: &SetupEnv, system: ArchiveSystem) -> Result<()> {
    let require = |key: &str| -> Result<()> {
        if env.config.get(key).map_or(true, |v| v.is_empty()) {
            bail!("Required config variable {} is not set", key);
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

/// Ensure rsync is installed. Silent when already present.
async fn ensure_rsync(emitter: &SetupEmitter) -> Result<()> {
    if sentryusb_shell::run("which", &["rsync"]).await.is_ok() {
        return Ok(());
    }
    emitter.progress("Installing rsync...");
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(600),
        "apt-get", &["-y", "install", "rsync"],
    ).await.context("failed to install rsync")?;
    Ok(())
}

/// Check that at most one wake API is configured.
fn validate_wake_apis(env: &SetupEnv) -> Result<()> {
    let apis = [
        env.config.contains_key("TESSIE_API_TOKEN"),
        env.config.contains_key("TESLAFI_API_TOKEN"),
        env.config.contains_key("TESLA_BLE_VIN"),
        env.config.contains_key("KEEP_AWAKE_WEBHOOK_URL"),
    ];
    let count = apis.iter().filter(|&&v| v).count();
    if count > 1 {
        bail!("Multiple control providers configured — only 1 can be enabled at a time");
    }
    Ok(())
}

/// Validate SENTRY_CASE value if any wake API is enabled.
fn validate_sentry_case(env: &SetupEnv) -> Result<()> {
    let has_api = env.config.contains_key("TESSIE_API_TOKEN")
        || env.config.contains_key("TESLAFI_API_TOKEN")
        || env.config.contains_key("TESLA_BLE_VIN")
        || env.config.contains_key("KEEP_AWAKE_WEBHOOK_URL");

    if has_api {
        let case = env.get("SENTRY_CASE", "");
        if !["1", "2", "3"].contains(&case.as_str()) {
            bail!("SENTRY_CASE must be 1, 2, or 3 when a wake API is configured");
        }
    }
    Ok(())
}

/// Configure Tesla BLE if VIN is set. Returns true if the phase did work.
///
/// Idempotent: if the binaries are already installed and keys exist, we do
/// nothing and return false so the caller can skip announcing a phase.
pub async fn configure_tesla_ble(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let vin = match env.config.get("TESLA_BLE_VIN") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            info!("Tesla BLE not enabled");
            return Ok(false);
        }
    };

    // Match the Go-era install layout: the canonical path is /root/bin/
    // (which the vendored awake_start script hardcodes at line 153 and
    // 363), and we additionally symlink into /usr/local/bin/ so PATH
    // lookups also succeed. Without /root/bin/tesla-control, every
    // keep-awake call in archiveloop fails with
    //   /root/bin/awake_start: line 153: /root/bin/tesla-control:
    //   No such file or directory
    // even though the binary exists at /usr/local/bin/.
    let binaries_present = std::path::Path::new("/root/bin/tesla-control").exists()
        && std::path::Path::new("/root/bin/tesla-keygen").exists();
    let keys_present = std::path::Path::new("/root/.ble/key_private.pem").exists();

    if binaries_present && keys_present {
        return Ok(false);
    }

    emitter.begin_phase("tesla_ble", "Tesla BLE peripheral");
    emitter.progress("Configuring Tesla BLE...");

    // Install bluez
    if sentryusb_shell::run("dpkg", &["-s", "bluez"]).await.is_err() {
        emitter.progress("Installing bluez...");
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(600),
            "apt-get", &["-y", "install", "bluez"],
        ).await?;
    }

    // Install pi-bluetooth if available
    if sentryusb_shell::run("bash", &["-c", "apt-cache search pi-bluetooth | grep -q pi-bluetooth"]).await.is_ok() {
        if sentryusb_shell::run("dpkg", &["-s", "pi-bluetooth"]).await.is_err() {
            let _ = sentryusb_shell::run_with_timeout(
                Duration::from_secs(600),
                "apt-get", &["-y", "install", "pi-bluetooth"],
            ).await;
        }
    }

    if !binaries_present {
        emitter.progress("Downloading Tesla BLE control binaries...");
        let arch = sentryusb_shell::run("uname", &["-m"]).await?.trim().to_string();
        let tarball = if arch == "aarch64" || arch.starts_with("arm") {
            "vehicle-command-binaries-linux-armv6.tar.gz"
        } else {
            emitter.progress("Unsupported architecture for Tesla BLE binaries");
            return Ok(true);
        };

        let url = format!(
            "https://github.com/MikeBishop/tesla-vehicle-command-arm-binaries/releases/latest/download/{}",
            tarball
        );
        let _ = std::fs::create_dir_all("/tmp/blebin");
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(60),
            "bash", &["-c", &format!(
                "curl -sL '{}' | tar xzf - -C /tmp/blebin --strip-components=1", url
            )],
        ).await.context("failed to download Tesla BLE binaries")?;

        let _ = std::fs::create_dir_all("/root/bin");
        for binary in &["tesla-control", "tesla-keygen"] {
            let src = format!("/tmp/blebin/{}", binary);
            let dst = format!("/root/bin/{}", binary);
            let path_link = format!("/usr/local/bin/{}", binary);
            if std::path::Path::new(&src).exists() {
                std::fs::copy(&src, &dst)?;
                sentryusb_shell::run("chmod", &["+x", &dst]).await?;
                // Mirror to /usr/local/bin/ via symlink so PATH-based
                // callers (the wizard's pairing test, manual ssh sessions)
                // still work. Remove a stale file or stale symlink first
                // so re-runs leave us with a clean link.
                let _ = std::fs::remove_file(&path_link);
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(&dst, &path_link);
                emitter.progress(&format!("Installed {} (symlinked to {})", dst, path_link));
            }
        }
        let _ = std::fs::remove_dir_all("/tmp/blebin");
    }

    // Generate BLE keys if they don't exist
    let _ = std::fs::create_dir_all("/root/.ble");
    if !std::path::Path::new("/root/.ble/key_private.pem").exists() {
        sentryusb_shell::run(
            "tesla-keygen", &["-key-file", "/root/.ble/key_private.pem", "-output", "/root/.ble/key_public.pem", "create"],
        ).await?;
        sentryusb_shell::run("chmod", &["600", "/root/.ble/key_private.pem"]).await?;
        sentryusb_shell::run("chmod", &["644", "/root/.ble/key_public.pem"]).await?;
        std::fs::write("/root/.ble/key_pending_pairing", "")?;
        emitter.progress("Generated Tesla BLE keys. Pairing required via web UI.");
    } else {
        let vin_upper = vin.to_uppercase();
        let paired = sentryusb_shell::run_with_timeout(
            Duration::from_secs(35),
            "tesla-control", &["-ble", "-vin", &vin_upper, "body-controller-state"],
        ).await;

        match paired {
            Ok(_) => emitter.progress("Tesla BLE keys exist and car is reachable."),
            Err(_) => emitter.progress("Tesla BLE keys exist, but car not reachable. Pairing can be done later."),
        }
    }

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
    Ok(())
}
