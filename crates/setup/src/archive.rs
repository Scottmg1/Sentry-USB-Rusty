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

    let binaries_present = std::path::Path::new("/usr/local/bin/tesla-control").exists()
        && std::path::Path::new("/usr/local/bin/tesla-keygen").exists();
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

        for binary in &["tesla-control", "tesla-keygen"] {
            let src = format!("/tmp/blebin/{}", binary);
            let dst = format!("/usr/local/bin/{}", binary);
            if std::path::Path::new(&src).exists() {
                std::fs::copy(&src, &dst)?;
                sentryusb_shell::run("chmod", &["+x", &dst]).await?;
                emitter.progress(&format!("Installed {}", dst));
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

    crate::system::install_archive_service()?;
    let _ = sentryusb_shell::run("systemctl", &["daemon-reload"]).await;
    let _ = sentryusb_shell::run("systemctl", &["enable", "sentryusb-archive.service"]).await;

    emitter.progress("Archive configuration complete.");
    Ok(true)
}
