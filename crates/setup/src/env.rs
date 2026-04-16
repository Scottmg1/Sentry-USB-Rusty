//! Pi environment detection — replaces `envsetup.sh`.

use std::fs;
use std::path::Path;

use anyhow::Result;

/// Detected Pi model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PiModel {
    Pi5,
    Pi4,
    Pi3,
    PiZero2,
    PiZeroW,
    Pi2,
    Other,
}

impl PiModel {
    pub fn detect() -> Self {
        let model = fs::read_to_string("/sys/firmware/devicetree/base/model")
            .unwrap_or_default()
            .replace('\0', "");
        let lower = model.to_lowercase();

        if lower.contains("pi 5") {
            PiModel::Pi5
        } else if lower.contains("pi 4") {
            PiModel::Pi4
        } else if lower.contains("pi 3") {
            PiModel::Pi3
        } else if lower.contains("zero 2") {
            PiModel::PiZero2
        } else if lower.contains("zero") {
            PiModel::PiZeroW
        } else if lower.contains("pi 2") {
            PiModel::Pi2
        } else {
            PiModel::Other
        }
    }

    /// The config.txt section name for this Pi model's dtoverlay.
    pub fn config_section(&self) -> &'static str {
        match self {
            PiModel::Pi5 => "pi5",
            PiModel::Pi4 => "pi4",
            PiModel::Pi3 => "all", // Pi3 uses global section
            PiModel::PiZero2 => "pi02",
            _ => "all",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            PiModel::Pi5 => "Raspberry Pi 5",
            PiModel::Pi4 => "Raspberry Pi 4",
            PiModel::Pi3 => "Raspberry Pi 3",
            PiModel::PiZero2 => "Raspberry Pi Zero 2 W",
            PiModel::PiZeroW => "Raspberry Pi Zero W",
            PiModel::Pi2 => "Raspberry Pi 2",
            PiModel::Other => "Unknown board",
        }
    }
}

/// Detected environment paths and configuration.
#[derive(Debug, Clone)]
pub struct SetupEnv {
    pub pi_model: PiModel,
    /// Boot partition (/sentryusb -> /boot/firmware or /boot).
    pub boot_path: String,
    /// Path to cmdline.txt if it exists.
    pub cmdline_path: Option<String>,
    /// Path to config.txt if it exists.
    pub piconfig_path: Option<String>,
    /// The boot disk device (e.g. /dev/mmcblk0).
    pub boot_disk: Option<String>,
    /// Root partition device (e.g. /dev/mmcblk0p2).
    pub root_partition: Option<String>,
    /// External data drive set in config, if any.
    pub data_drive: Option<String>,
    /// Parsed configuration values.
    pub config: std::collections::HashMap<String, String>,
}

impl SetupEnv {
    pub async fn detect() -> Result<Self> {
        let pi_model = PiModel::detect();

        // Ensure /sentryusb symlink exists
        ensure_sentryusb_symlink()?;

        let boot_path = fs::read_link("/sentryusb")
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/boot".to_string());

        let cmdline_path = ["/sentryusb/cmdline.txt"]
            .iter()
            .find(|p| Path::new(p).exists())
            .map(|s| s.to_string());

        let piconfig_path = ["/sentryusb/config.txt"]
            .iter()
            .find(|p| Path::new(p).exists())
            .map(|s| s.to_string());

        // Detect boot disk
        let boot_disk = detect_boot_disk().await.ok();
        let root_partition = detect_root_partition().await.ok();

        // Load config
        let config_path = sentryusb_config::find_config_path();
        let config = match sentryusb_config::parse_file(config_path) {
            Ok((active, commented)) => {
                let mut merged = commented;
                merged.extend(active);
                merged
            }
            Err(_) => std::collections::HashMap::new(),
        };

        let data_drive = config.get("DATA_DRIVE")
            .filter(|v| !v.is_empty())
            .cloned();

        Ok(SetupEnv {
            pi_model,
            boot_path,
            cmdline_path,
            piconfig_path,
            boot_disk,
            root_partition,
            data_drive,
            config,
        })
    }

    /// Get a config value with a default.
    pub fn get(&self, key: &str, default: &str) -> String {
        self.config.get(key).cloned().unwrap_or_else(|| default.to_string())
    }

    /// Get a config value as bool (matches bash `true`/`false`).
    pub fn get_bool(&self, key: &str, default: bool) -> bool {
        match self.config.get(key).map(|s| s.as_str()) {
            Some("true") => true,
            Some("false") => false,
            _ => default,
        }
    }
}

/// Creates /sentryusb -> /boot/firmware (or /boot) if it doesn't exist.
fn ensure_sentryusb_symlink() -> Result<()> {
    let link = Path::new("/sentryusb");
    if link.is_symlink() || link.exists() {
        return Ok(());
    }

    // Determine target
    let target = if Path::new("/boot/firmware").exists() {
        "/boot/firmware"
    } else {
        "/boot"
    };

    #[cfg(unix)]
    std::os::unix::fs::symlink(target, "/sentryusb")?;

    Ok(())
}

async fn detect_boot_disk() -> Result<String> {
    let output = sentryusb_shell::run(
        "lsblk", &["-dpno", "pkname", &detect_mount_source("/sentryusb").await?],
    ).await?;
    let dev = output.trim();
    Ok(format!("/dev/{}", dev))
}

async fn detect_root_partition() -> Result<String> {
    let output = sentryusb_shell::run("findmnt", &["-n", "-o", "SOURCE", "/"]).await?;
    Ok(output.trim().to_string())
}

async fn detect_mount_source(mountpoint: &str) -> Result<String> {
    let output = sentryusb_shell::run(
        "findmnt", &["-D", "-no", "SOURCE", "--target", mountpoint],
    ).await?;
    Ok(output.trim().to_string())
}
