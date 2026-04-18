//! USB gadget control via Linux configfs.
//!
//! Replaces `enable_gadget.sh` and `disable_gadget.sh` with native Rust
//! operations on `/sys/kernel/config/usb_gadget/sentryusb`.

pub mod snapshot;
pub mod space;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tracing::info;

const GADGET_NAME: &str = "sentryusb";
const LANG: &str = "0x0409"; // US English
const CFG: &str = "c";

/// Disk images that can be exposed as USB mass storage LUNs.
const DISK_IMAGES: &[(&str, &str)] = &[
    ("/backingfiles/cam_disk.bin", "CAM"),
    ("/backingfiles/music_disk.bin", "MUSIC"),
    ("/backingfiles/lightshow_disk.bin", "LIGHTSHOW"),
    ("/backingfiles/boombox_disk.bin", "BOOMBOX"),
    ("/backingfiles/wraps_disk.bin", "WRAPS"),
];

/// Find the configfs root mount point.
fn find_configfs_root() -> Result<PathBuf> {
    let mounts = fs::read_to_string("/proc/mounts")
        .context("failed to read /proc/mounts")?;
    for line in mounts.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 3 && fields[2] == "configfs" {
            return Ok(PathBuf::from(fields[1]));
        }
    }
    bail!("configfs not mounted")
}

/// Write a string to a sysfs/configfs file.
fn write_file(path: &Path, content: &str) -> Result<()> {
    fs::write(path, content)
        .with_context(|| format!("failed to write {}", path.display()))
}

/// Get the SBC model and return the appropriate MaxPower value (mA).
fn get_max_power() -> u32 {
    let model = fs::read_to_string("/proc/device-tree/model").unwrap_or_default();
    let model = model.to_lowercase();
    if model.contains("pi 5") {
        600
    } else if model.contains("pi 4") {
        500
    } else if model.contains("pi 3") {
        300
    } else if model.contains("pi 2") || model.contains("zero 2") {
        200
    } else {
        100
    }
}

/// Get the machine ID for the serial number.
fn get_machine_serial() -> String {
    let mid = fs::read_to_string("/etc/machine-id").unwrap_or_default();
    let mid = mid.trim();
    // SHA256 hash of machine-id (matching the bash script)
    use std::process::Command;
    if let Ok(_output) = Command::new("sha256sum").stdin(std::process::Stdio::piped()).output() {
        // Fallback: just use the machine-id directly
        return format!("SentryUSB-{}", mid);
    }
    format!("SentryUSB-{}", mid)
}

/// Enable the USB gadget by setting up configfs.
/// This is equivalent to `enable_gadget.sh`.
pub fn enable() -> Result<()> {
    let configfs = find_configfs_root()?;
    let gadget = configfs.join("usb_gadget").join(GADGET_NAME);

    // Unload legacy g_mass_storage so it doesn't hold the UDC.
    let _ = std::process::Command::new("modprobe")
        .args(["-q", "-r", "g_mass_storage"])
        .status();

    // If the gadget dir already exists, only a UDC (re)bind is required — a
    // prior enable may have failed to bind (UDC busy) leaving partial state.
    if gadget.exists() {
        return bind_udc(&gadget);
    }

    // Load the composite module
    let _ = std::process::Command::new("modprobe")
        .arg("libcomposite")
        .status();

    // Create gadget directory structure
    let cfg_dir = gadget.join(format!("configs/{}.1", CFG));
    fs::create_dir_all(&cfg_dir)?;

    // Common USB descriptor setup
    write_file(&gadget.join("idVendor"), "0x1d6b")?;  // Linux Foundation
    write_file(&gadget.join("idProduct"), "0x0104")?;  // Composite Gadget
    write_file(&gadget.join("bcdDevice"), "0x0100")?;  // v1.0.0
    write_file(&gadget.join("bcdUSB"), "0x0200")?;     // USB 2.0

    // String descriptors
    let strings_dir = gadget.join(format!("strings/{}", LANG));
    fs::create_dir_all(&strings_dir)?;
    let cfg_strings = gadget.join(format!("configs/{}.1/strings/{}", CFG, LANG));
    fs::create_dir_all(&cfg_strings)?;

    write_file(&strings_dir.join("serialnumber"), &get_machine_serial())?;
    write_file(&strings_dir.join("manufacturer"), "SentryUSB")?;
    write_file(&strings_dir.join("product"), "SentryUSB Composite Gadget")?;
    write_file(&cfg_strings.join("configuration"), "SentryUSB Config")?;

    // MaxPower based on Pi model
    write_file(
        &cfg_dir.join("MaxPower"),
        &get_max_power().to_string(),
    )?;

    // Mass storage function with LUNs for each disk image
    let func_dir = gadget.join("functions/mass_storage.0");
    fs::create_dir_all(&func_dir)?;

    let mut lun = 0;
    for (image_path, label) in DISK_IMAGES {
        if Path::new(image_path).exists() {
            let lun_dir = func_dir.join(format!("lun.{}", lun));
            if lun > 0 {
                fs::create_dir_all(&lun_dir)?;
            }
            write_file(&lun_dir.join("file"), image_path)?;

            // Get file size for inquiry string
            let size = fs::metadata(image_path)
                .map(|m| format_size(m.len()))
                .unwrap_or_else(|_| "?".to_string());
            write_file(
                &lun_dir.join("inquiry_string"),
                &format!("SentryUSB {} {}", label, size),
            )?;

            lun += 1;
        }
    }

    // Link the function to the configuration
    let link_target = cfg_dir.join("mass_storage.0");
    if !link_target.exists() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&func_dir, &link_target)?;
        #[cfg(not(unix))]
        bail!("USB gadget control requires Linux");
    }

    info!("USB gadget configured with {} LUN(s)", lun);
    bind_udc(&gadget)
}

/// Bind (or rebind) the UDC for an already-configured gadget dir. If the UDC
/// is busy, blank the UDC slot, wait briefly, and retry so stale bindings
/// clear. Returns the underlying error if the final attempt fails.
fn bind_udc(gadget: &Path) -> Result<()> {
    let udc = find_udc()?;
    let udc_path = gadget.join("UDC");

    // Clear any stale binding before writing the new one.
    let _ = fs::write(&udc_path, "");

    for attempt in 1..=5 {
        match fs::write(&udc_path, &udc) {
            Ok(()) => {
                info!("USB gadget bound to UDC: {}", udc);
                return Ok(());
            }
            Err(e) if attempt < 5 => {
                info!("UDC bind attempt {} failed ({}), retrying", attempt, e);
                let _ = fs::write(&udc_path, "");
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to bind UDC {}: {}", udc, e));
            }
        }
    }
    Ok(())
}

/// Disable the USB gadget by tearing down configfs.
/// This is equivalent to `disable_gadget.sh`.
pub fn disable() -> Result<()> {
    // Remove legacy g_mass_storage if loaded
    let _ = std::process::Command::new("modprobe")
        .args(["-q", "-r", "g_mass_storage"])
        .status();

    let configfs = find_configfs_root()?;
    let gadget = configfs.join("usb_gadget").join(GADGET_NAME);

    if !gadget.exists() {
        info!("USB gadget already disabled");
        return Ok(());
    }

    // Deactivate UDC
    let _ = fs::write(gadget.join("UDC"), "");

    // Remove config symlinks and string dirs
    let cfg_dir = gadget.join(format!("configs/{}.1", CFG));
    let _ = fs::remove_file(cfg_dir.join("mass_storage.0"));
    let cfg_strings = cfg_dir.join(format!("strings/{}", LANG));
    let _ = fs::remove_dir(&cfg_strings);

    // Remove extra LUNs (lun.1 through lun.4)
    let func_dir = gadget.join("functions/mass_storage.0");
    for i in 1..=4 {
        let _ = fs::remove_dir(func_dir.join(format!("lun.{}", i)));
    }
    let _ = fs::remove_dir(&func_dir);

    // Remove config and string dirs
    let _ = fs::remove_dir(&cfg_dir);
    let _ = fs::remove_dir(gadget.join(format!("strings/{}", LANG)));
    let _ = fs::remove_dir(&gadget);

    // Unload modules
    let _ = std::process::Command::new("modprobe")
        .args(["-r", "usb_f_mass_storage", "g_ether", "usb_f_ecm", "usb_f_rndis", "libcomposite"])
        .status();

    info!("USB gadget disabled");
    Ok(())
}

/// Check if the gadget is currently active — i.e. actually bound to a UDC.
/// A stale config dir with an empty UDC file is considered inactive so the
/// user can recover via a fresh toggle.
pub fn is_active() -> bool {
    let udc_file = Path::new("/sys/kernel/config/usb_gadget/sentryusb/UDC");
    fs::read_to_string(udc_file)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Find the first available UDC (USB Device Controller).
fn find_udc() -> Result<String> {
    let udc_dir = Path::new("/sys/class/udc");
    if let Ok(entries) = fs::read_dir(udc_dir) {
        for entry in entries.flatten() {
            return Ok(entry.file_name().to_string_lossy().to_string());
        }
    }
    bail!("no UDC found in /sys/class/udc")
}

/// Format a byte count as human-readable (e.g., "32G", "512M").
fn format_size(bytes: u64) -> String {
    if bytes >= 1_073_741_824 {
        format!("{}G", bytes / 1_073_741_824)
    } else if bytes >= 1_048_576 {
        format!("{}M", bytes / 1_048_576)
    } else if bytes >= 1024 {
        format!("{}K", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}
