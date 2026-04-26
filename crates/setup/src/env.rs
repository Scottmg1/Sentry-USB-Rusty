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

        // Look through /sentryusb first (preserves the user's chosen
        // boot dir), then fall through to the canonical locations so a
        // broken symlink left over from a prior install doesn't make
        // every cmdline/config edit silently no-op. Bookworm puts the
        // boot files under /boot/firmware; older images use /boot.
        let cmdline_path = [
            "/sentryusb/cmdline.txt",
            "/boot/firmware/cmdline.txt",
            "/boot/cmdline.txt",
        ]
        .iter()
        .find(|p| Path::new(p).exists())
        .map(|s| s.to_string());

        let piconfig_path = [
            "/sentryusb/config.txt",
            "/boot/firmware/config.txt",
            "/boot/config.txt",
        ]
        .iter()
        .find(|p| Path::new(p).exists())
        .map(|s| s.to_string());

        // Detect boot disk
        let boot_disk = detect_boot_disk().await.ok();
        let root_partition = detect_root_partition().await.ok();

        // Load config. Only use *active* (uncommented) exports — commented
        // sample lines in sentryusb.conf are documentation, not user choices.
        // Merging them in would make every optional phase (AP setup, extra
        // drives, etc.) run against sample defaults the user never picked.
        let config_path = sentryusb_config::find_config_path();
        let mut config = sentryusb_config::parse_file(config_path)
            .map(|(active, _commented)| active)
            .unwrap_or_default();

        // Migrate legacy key names (teslausb-era lowercase, renamed
        // settings). Copies the old value to the new key only if the new
        // key isn't already set — so user edits to the new name always win.
        migrate_legacy_config_keys(&mut config);

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

/// Mobile push credentials loaded from the JSON file the API server
/// manages. Returned as `(device_id, device_secret)`.
///
/// The JSON file is the single source of truth for these values to avoid
/// conf-file write races (the bash version was `envsetup.sh:142-150`).
/// Returns `None` if the file doesn't exist, can't be parsed, or either
/// field is missing.
pub fn mobile_push_credentials() -> Option<(String, String)> {
    let json = std::fs::read_to_string("/root/.sentryusb/notification-credentials.json").ok()?;
    let device_id = extract_json_string(&json, "device_id")?;
    let device_secret = extract_json_string(&json, "device_secret")?;
    Some((device_id, device_secret))
}

/// Minimal JSON string-value extractor. Matches the bash `sed` pattern
/// used by envsetup.sh so behavior is identical across ports — we don't
/// need full serde here because the credentials file is a flat object
/// written by our own API.
fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let start = json.find(&needle)?;
    let after = &json[start + needle.len()..];
    let colon = after.find(':')?;
    let rest = &after[colon + 1..];
    let quote = rest.find('"')?;
    let value_start = quote + 1;
    let value_end = rest[value_start..].find('"')?;
    Some(rest[value_start..value_start + value_end].to_string())
}

/// Copy legacy config keys to their current names, matching the table in
/// bash `envsetup.sh:62-94`. New-name wins: if the user has already set
/// the new key, we don't overwrite it from the old one.
fn migrate_legacy_config_keys(config: &mut std::collections::HashMap<String, String>) {
    const LEGACY_MAP: &[(&str, &str)] = &[
        ("archiveserver", "ARCHIVE_SERVER"),
        ("camsize", "CAM_SIZE"),
        ("musicsize", "MUSIC_SIZE"),
        ("sharename", "SHARE_NAME"),
        ("musicsharename", "MUSIC_SHARE_NAME"),
        ("shareuser", "SHARE_USER"),
        ("sharepassword", "SHARE_PASSWORD"),
        ("tesla_email", "TESLA_EMAIL"),
        ("tesla_password", "TESLA_PASSWORD"),
        ("tesla_vin", "TESLA_VIN"),
        ("timezone", "TIME_ZONE"),
        ("usb_drive", "DATA_DRIVE"),
        ("USB_DRIVE", "DATA_DRIVE"),
        ("archivedelay", "ARCHIVE_DELAY"),
        ("trigger_file_saved", "TRIGGER_FILE_SAVED"),
        ("trigger_file_sentry", "TRIGGER_FILE_SENTRY"),
        ("trigger_file_any", "TRIGGER_FILE_ANY"),
        ("pushover_enabled", "PUSHOVER_ENABLED"),
        ("pushover_user_key", "PUSHOVER_USER_KEY"),
        ("pushover_app_key", "PUSHOVER_APP_KEY"),
        ("gotify_enabled", "GOTIFY_ENABLED"),
        ("gotify_domain", "GOTIFY_DOMAIN"),
        ("gotify_app_token", "GOTIFY_APP_TOKEN"),
        ("gotify_priority", "GOTIFY_PRIORITY"),
        ("ifttt_enabled", "IFTTT_ENABLED"),
        ("ifttt_event_name", "IFTTT_EVENT_NAME"),
        ("ifttt_key", "IFTTT_KEY"),
        ("sns_enabled", "SNS_ENABLED"),
        ("aws_region", "AWS_REGION"),
        ("aws_access_key_id", "AWS_ACCESS_KEY_ID"),
        ("aws_secret_key", "AWS_SECRET_ACCESS_KEY"),
        ("aws_sns_topic_arn", "AWS_SNS_TOPIC_ARN"),
    ];

    for (old, new) in LEGACY_MAP {
        if config.contains_key(*new) {
            continue;
        }
        if let Some(val) = config.get(*old).cloned() {
            config.insert((*new).to_string(), val);
            config.remove(*old);
        }
    }
}

/// Creates /sentryusb -> /boot/firmware (or /boot) if it doesn't exist.
fn ensure_sentryusb_symlink() -> Result<()> {
    let link = Path::new("/sentryusb");
    if link.is_symlink() || link.exists() {
        return Ok(());
    }

    #[cfg(unix)]
    {
        let target = if Path::new("/boot/firmware").exists() {
            "/boot/firmware"
        } else {
            "/boot"
        };
        std::os::unix::fs::symlink(target, "/sentryusb")?;
    }

    Ok(())
}

async fn detect_boot_disk() -> Result<String> {
    // `-p` makes lsblk emit full paths already (e.g. "/dev/sda"), so don't
    // prepend "/dev/" again — that yields "/dev//dev/sda" and every sfdisk
    // call on it fails silently, cascading into bogus "not last partition"
    // errors during the shrink phase.
    let output = sentryusb_shell::run(
        "lsblk", &["-dpno", "pkname", &detect_mount_source("/sentryusb").await?],
    ).await?;
    let dev = output.trim().to_string();
    if dev.is_empty() {
        anyhow::bail!("could not determine boot disk for /sentryusb");
    }
    Ok(dev)
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
