//! Setup runner — the main orchestrator that replaces `setup-sentryusb`.
//!
//! Ties all setup phases together with progress logging via a callback,
//! typically wired to both a log file and WebSocket broadcast.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};
use tracing::info;

use crate::env::SetupEnv;

const SETUP_LOG: &str = "/sentryusb/sentryusb-setup.log";
const SETUP_FINISHED_MARKER: &str = "/sentryusb/SENTRYUSB_SETUP_FINISHED";
const SETUP_STARTED_MARKER: &str = "/sentryusb/SENTRYUSB_SETUP_STARTED";

/// Progress callback type — receives timestamped messages.
pub type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Create a progress callback that writes to both the log file and an
/// arbitrary closure (e.g. WebSocket broadcast).
pub fn make_progress(extra: impl Fn(&str) + Send + Sync + 'static) -> ProgressFn {
    Arc::new(move |msg: &str| {
        let stamped = format!("{} : {}", chrono_now(), msg);
        // Write to log file
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(SETUP_LOG)
        {
            use std::io::Write;
            let _ = writeln!(f, "{}", stamped);
        }
        info!("[setup] {}", msg);
        extra(msg);
    })
}

fn chrono_now() -> String {
    // Use system date command since we don't want to pull in chrono just for this
    std::process::Command::new("date")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "???".to_string())
}

/// Run the full setup process from scratch.
///
/// This replaces the entire `setup-sentryusb` script. It detects the
/// environment, partitions the disk, creates disk images, configures the
/// system, and marks setup as complete.
pub async fn run_full_setup(progress: ProgressFn) -> Result<()> {
    progress("=== SentryUSB Setup Starting ===");

    // Root check
    if !am_root() {
        bail!("Setup must run as root");
    }

    // Mark setup as started
    let _ = std::fs::remove_file(SETUP_FINISHED_MARKER);
    let _ = std::fs::create_dir_all("/sentryusb");
    let _ = std::fs::write(SETUP_STARTED_MARKER, "");

    // Remount root filesystem read-write for setup
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    // Phase 1: Detect environment
    progress("Phase 1: Detecting environment...");
    let env = SetupEnv::detect().await?;
    progress(&format!("Detected: {}", env.pi_model.display_name()));

    // Phase 2: WiFi regulatory domain
    progress("Phase 2: WiFi regulatory domain...");
    let needs_reboot = configure_wifi_regulatory(&env, &*progress).await?;
    if needs_reboot {
        progress("Rebooting to apply WiFi regulatory domain change...");
        reboot().await;
        return Ok(());
    }

    // Phase 3: config.txt — dwc2 overlay for USB gadget
    progress("Phase 3: USB gadget overlay...");
    let dwc2_changed = configure_dwc2_overlay(&env, &*progress).await?;
    if dwc2_changed {
        progress("Rebooting to apply dwc2 overlay change...");
        reboot().await;
        return Ok(());
    }

    // Phase 4: Root partition shrink (if auto-expanded by Pi Imager)
    progress("Phase 4: Checking disk layout...");
    let shrink_needed = check_root_shrink(&env, &*progress).await?;
    if shrink_needed {
        // Shrink triggers a reboot via initramfs; this function doesn't return
        return Ok(());
    }

    // Phase 5: Hostname
    progress("Phase 5: System configuration...");
    crate::system::configure_hostname(&env, &*progress).await?;

    // Phase 6: Update package index
    progress("Phase 6: Package management...");
    update_package_index(&*progress).await?;

    // Phase 7: Timezone
    crate::system::configure_timezone(&env, &*progress).await?;

    // Phase 8: cmdline.txt modules-load
    progress("Phase 8: Boot configuration...");
    let modules_changed = fix_cmdline_modules(&env, &*progress).await?;
    if modules_changed {
        progress("Rebooting to apply cmdline.txt change...");
        reboot().await;
        return Ok(());
    }

    // Phase 9: Install required packages
    progress("Phase 9: Required packages...");
    crate::system::install_required_packages(&*progress).await?;

    // Phase 9b: Runtime helper scripts
    crate::scripts::install_runtime_scripts(&*progress).await?;

    // Phase 10: UAS quirks for external USB drives
    fix_uas_quirks(&env, &*progress).await?;

    // Phase 11: Partitions
    progress("Phase 10: Disk partitioning...");
    if env.data_drive.is_some() {
        crate::partition::setup_data_drive(&env, &*progress).await?;
    } else {
        crate::partition::setup_sd_card(&env, &*progress).await?;
    }

    // Mount partitions
    mount_partitions(&*progress).await?;

    // Phase 12: Disk images
    progress("Phase 11: Disk images...");
    crate::disk_images::create_disk_images(&env, &*progress).await?;

    // Update fstab for disk images
    update_image_fstab_entries(&*progress).await?;

    // Initialize drive directories
    initialize_drive_directories(&*progress).await?;

    // Phase 13: Archive configuration
    if env.get_bool("CONFIGURE_ARCHIVING", true) {
        progress("Phase 12: Archive configuration...");
        crate::archive::configure_archive(&env, &*progress).await?;
    }

    // Phase 14: Samba
    crate::system::configure_samba(&env, &*progress).await?;

    // Phase 15: WiFi AP
    if env.config.contains_key("AP_SSID") {
        progress("Phase 13: WiFi AP...");
        crate::network::configure_ap(&env, &*progress).await?;
    }

    // Phase 16: SSH hardening
    progress("Phase 14: SSH hardening...");
    crate::system::configure_ssh(&*progress).await?;

    // Phase 17: Avahi mDNS
    crate::system::configure_avahi(&env, &*progress).await?;

    // Phase 18: RTC
    crate::system::configure_rtc(&env, &*progress).await?;

    // Phase 19: BLE daemon
    progress("Phase 15: BLE peripheral...");
    crate::archive::configure_tesla_ble(&env, &*progress).await?;

    // Phase 20: Read-only filesystem
    progress("Phase 16: Read-only filesystem...");
    crate::readonly::make_readonly(&env, &*progress).await?;

    // Phase 21: Optional package upgrade
    if env.get_bool("UPGRADE_PACKAGES", false) {
        progress("Upgrading installed packages...");
        let _ = sentryusb_shell::run("apt-get", &["clean"]).await;
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(600), "apt-get", &["--assume-yes", "upgrade"],
        ).await;
        let _ = sentryusb_shell::run("apt-get", &["clean"]).await;
    }

    // Mark setup complete
    let _ = std::fs::remove_file(SETUP_STARTED_MARKER);
    let _ = std::fs::write(SETUP_FINISHED_MARKER, "");

    progress("=== SentryUSB Setup Complete ===");
    progress("Reboot now for changes to take effect.");

    Ok(())
}

fn am_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: geteuid is always safe to call
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

async fn reboot() {
    let _ = sentryusb_shell::run("reboot", &[]).await;
}

/// Set WiFi regulatory domain to US if not set. Persists via /etc/default/crda
/// and cfg80211 module param so it survives reboots without needing one.
async fn configure_wifi_regulatory(_env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<bool> {
    if sentryusb_shell::run("systemctl", &["-q", "is-enabled", "NetworkManager.service"]).await.is_ok() {
        let output = sentryusb_shell::run(
            "bash", &["-c", "iw reg get 2>/dev/null | grep -oP '(?<=country )\\w+' | head -1"],
        ).await.unwrap_or_default();
        let reg = output.trim();
        if reg.is_empty() || reg == "00" {
            progress("Setting WiFi regulatory domain to US");
            let _ = sentryusb_shell::run("iw", &["reg", "set", "US"]).await;
            // Persist so it survives reboots (no reboot needed)
            let _ = std::fs::write("/etc/default/crda", "REGDOMAIN=US\n");
            let _ = sentryusb_shell::run(
                "bash", &["-c", "mkdir -p /etc/modprobe.d && echo 'options cfg80211 ieee80211_regdom=US' > /etc/modprobe.d/cfg80211.conf"],
            ).await;
        }
    }
    // Never requires a reboot
    Ok(false)
}

/// Configure the dwc2 USB gadget overlay in config.txt with proper per-model sections.
async fn configure_dwc2_overlay(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<bool> {
    let config_path = match &env.piconfig_path {
        Some(p) => p.clone(),
        None => return Ok(false),
    };

    let config = std::fs::read_to_string(&config_path).unwrap_or_default();
    let section = env.pi_model.config_section();

    // Pi 3 uses dr_mode=peripheral
    let overlay_line = if env.pi_model == crate::env::PiModel::Pi3 {
        "dtoverlay=dwc2,dr_mode=peripheral"
    } else {
        "dtoverlay=dwc2"
    };

    if section == "all" {
        // Global: check before any [section] or in [all]
        let in_global = config.lines()
            .take_while(|l| !l.starts_with('['))
            .any(|l| l.contains("dtoverlay=dwc2"));
        let in_all = if let Some(idx) = config.find("[all]") {
            config[idx..].lines().skip(1)
                .take_while(|l| !l.starts_with('['))
                .any(|l| l.contains("dtoverlay=dwc2"))
        } else {
            false
        };

        if in_global || in_all {
            return Ok(false);
        }

        if config.contains("[all]") {
            let new = config.replacen("[all]", &format!("[all]\n{}", overlay_line), 1);
            std::fs::write(&config_path, new)?;
        } else {
            let mut f = std::fs::OpenOptions::new().append(true).open(&config_path)?;
            use std::io::Write;
            writeln!(f, "\n{}", overlay_line)?;
        }
    } else {
        // Model-specific section [pi4], [pi5], [pi02]
        let section_header = format!("[{}]", section);
        let in_section = if let Some(idx) = config.find(&section_header) {
            config[idx..].lines().skip(1)
                .take_while(|l| !l.starts_with('['))
                .any(|l| l.contains("dtoverlay=dwc2"))
        } else {
            false
        };

        if in_section {
            return Ok(false);
        }

        if config.contains(&section_header) {
            let new = config.replacen(
                &section_header,
                &format!("{}\n{}", section_header, overlay_line),
                1,
            );
            std::fs::write(&config_path, new)?;
        } else {
            let mut f = std::fs::OpenOptions::new().append(true).open(&config_path)?;
            use std::io::Write;
            writeln!(f, "\n{}\n{}", section_header, overlay_line)?;
        }

        // Remove stale global dtoverlay=dwc2
        let content = std::fs::read_to_string(&config_path)?;
        let mut lines: Vec<String> = Vec::new();
        let mut in_section_any = false;
        for line in content.lines() {
            if line.starts_with('[') {
                in_section_any = true;
            }
            if !in_section_any && line.trim() == "dtoverlay=dwc2" {
                continue; // Remove global stale entry
            }
            lines.push(line.to_string());
        }
        std::fs::write(&config_path, lines.join("\n") + "\n")?;
    }

    progress(&format!("Added {} to config.txt under [{}]", overlay_line, section));
    Ok(true)
}

/// Check if root needs shrinking (when Pi Imager auto-expanded it).
async fn check_root_shrink(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<bool> {
    // Only needed on first setup when no partitions exist yet
    if crate::partition::partitions_exist().await {
        return Ok(false);
    }

    let boot_disk = match &env.boot_disk {
        Some(d) => d.clone(),
        None => return Ok(false),
    };

    let output = sentryusb_shell::run(
        "bash", &["-c", &format!(
            "sfdisk -F '{}' 2>/dev/null | grep -o '[0-9]* bytes' | head -1 | awk '{{print $1}}'",
            boot_disk
        )],
    ).await.unwrap_or_default();
    let unpart_bytes: u64 = output.trim().parse().unwrap_or(0);
    let min_space: u64 = 8 * 1024 * 1024 * 1024; // 8 GiB

    if unpart_bytes >= min_space {
        progress(&format!("Sufficient unpartitioned space: {} GB", unpart_bytes / 1024 / 1024 / 1024));
        return Ok(false);
    }

    progress(&format!(
        "Insufficient unpartitioned space ({} MB). Root partition may need shrinking.",
        unpart_bytes / 1024 / 1024
    ));
    progress("This usually happens when Raspberry Pi Imager is used to flash the image.");
    progress("Initiating root filesystem shrink via initramfs...");

    // Calculate shrink target
    let used_output = sentryusb_shell::run(
        "bash", &["-c", "df --output=used -k / | tail -1 | tr -d ' '"],
    ).await?;
    let used_kb: u64 = used_output.trim().parse().unwrap_or(0);
    let target_gb = ((used_kb / 1024 / 1024) + 2).max(6);

    let _ = std::fs::write("/root/RESIZE_ATTEMPTED", "");

    // This will reboot the system
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "bash", &["-c", &format!(
            "resize2fs /dev/$(findmnt -n -o SOURCE /) {}G && reboot", target_gb
        )],
    ).await?;

    Ok(true)
}

/// Fix cmdline.txt modules-load to include dwc2 and g_ether.
async fn fix_cmdline_modules(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<bool> {
    let cmdline_path = match &env.cmdline_path {
        Some(p) => p.clone(),
        None => return Ok(false),
    };

    let content = std::fs::read_to_string(&cmdline_path)?;
    let has_dwc2 = content.contains("dwc2");
    let has_gether = content.contains("g_ether");

    if has_dwc2 && has_gether {
        return Ok(false);
    }

    // Remove old modules-load param if present
    let new_content = content.trim().to_string();
    let new_content = if let Some(start) = new_content.find("modules-load=") {
        let end = new_content[start..].find(' ').unwrap_or(new_content.len() - start);
        format!("{}{}", &new_content[..start], &new_content[start + end..])
    } else {
        new_content
    };

    let final_content = format!("{} modules-load=dwc2,g_ether\n", new_content.trim());
    std::fs::write(&cmdline_path, final_content)?;
    progress("Updated cmdline.txt with modules-load=dwc2,g_ether");
    Ok(true)
}

/// Add UAS quirks for known problematic USB drives.
async fn fix_uas_quirks(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    let cmdline_path = match &env.cmdline_path {
        Some(p) => p.clone(),
        None => return Ok(()),
    };

    let known_quirks = [
        "04e8:4001", // Samsung T7
        "04e8:4011", // Samsung T5 EVO
        "04e8:61f5", // Samsung T5/T3
        "174c:55aa", // ASMedia ASM1051E
        "152d:0578", // JMicron JMS578
    ];

    let content = std::fs::read_to_string(&cmdline_path)?;
    let mut new_entries = Vec::new();

    for quirk in &known_quirks {
        if !content.contains(quirk) {
            new_entries.push(format!("{}:u", quirk));
        }
    }

    if new_entries.is_empty() {
        return Ok(());
    }

    let joined = new_entries.join(",");
    let new_content = if content.contains("usb-storage.quirks=") {
        // Append to existing
        content.replace(
            "usb-storage.quirks=",
            &format!("usb-storage.quirks={},", joined),
        )
    } else {
        format!("{} usb-storage.quirks={}\n", content.trim(), joined)
    };

    std::fs::write(&cmdline_path, new_content)?;
    progress(&format!("Added UAS quirks: {}", joined));
    Ok(())
}

/// Update the package index.
async fn update_package_index(progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    progress("Updating package index...");
    let _ = sentryusb_shell::run("dpkg", &["--configure", "-a"]).await;

    for attempt in 0..3 {
        if sentryusb_shell::run_with_timeout(
            Duration::from_secs(300),
            "apt-get", &["update"],
        ).await.is_ok() {
            return Ok(());
        }
        progress(&format!("apt-get update failed (attempt {}), retrying...", attempt + 1));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Last try with --allow-releaseinfo-change
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "apt-get", &["update", "--allow-releaseinfo-change"],
    ).await?;
    Ok(())
}

/// Mount backingfiles and mutable partitions.
async fn mount_partitions(progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    let _ = std::fs::create_dir_all("/backingfiles");
    let _ = std::fs::create_dir_all("/mutable");

    if sentryusb_shell::run("findmnt", &["--mountpoint", "/backingfiles"]).await.is_err() {
        progress("Mounting backingfiles partition...");
        // Clear XFS log first
        if let Ok(dev) = sentryusb_shell::run("findfs", &["LABEL=backingfiles"]).await {
            let _ = sentryusb_shell::run_with_timeout(
                Duration::from_secs(60), "xfs_repair", &["-L", dev.trim()],
            ).await;
        }
        sentryusb_shell::run("mount", &["/backingfiles"]).await?;
    }

    if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"]).await.is_err() {
        progress("Mounting mutable partition...");
        sentryusb_shell::run("mount", &["/mutable"]).await?;
    }

    Ok(())
}

/// Update fstab with sentryusb mount entries for disk image files.
async fn update_image_fstab_entries(_progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    let images = [
        ("/backingfiles/cam_disk.bin", "/mnt/cam"),
        ("/backingfiles/music_disk.bin", "/mnt/music"),
        ("/backingfiles/lightshow_disk.bin", "/mnt/lightshow"),
        ("/backingfiles/boombox_disk.bin", "/mnt/boombox"),
        ("/backingfiles/wraps_disk.bin", "/mnt/wraps"),
    ];

    let mut fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();

    // Remove old entries
    let fstab_lines: Vec<&str> = fstab.lines()
        .filter(|l| !images.iter().any(|(img, _)| l.starts_with(img)))
        .collect();
    fstab = fstab_lines.join("\n");

    // Add entries for existing images
    for (img, mnt) in &images {
        if Path::new(img).exists() {
            let _ = std::fs::create_dir_all(mnt);
            fstab.push_str(&format!("\n{} {} sentryusb noauto 0 0", img, mnt));
        }
    }

    if !fstab.ends_with('\n') {
        fstab.push('\n');
    }
    std::fs::write("/etc/fstab", fstab)?;

    Ok(())
}

/// Mount each drive image, create required directories, then unmount.
async fn initialize_drive_directories(_progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    let _ = sentryusb_gadget::disable();

    let drives: &[(&str, &[&str])] = &[
        ("/mnt/cam", &["TeslaCam", "TeslaTrackMode"]),
        ("/mnt/music", &[]),
        ("/mnt/lightshow", &["LightShow"]),
        ("/mnt/boombox", &["Boombox"]),
        ("/mnt/wraps", &["Wraps"]),
    ];

    for (mnt, dirs) in drives {
        let image = format!(
            "/backingfiles/{}_disk.bin",
            mnt.rsplit('/').next().unwrap_or("cam")
        );
        if !Path::new(&image).exists() {
            continue;
        }

        // Try mounting with retry
        let mut mounted = false;
        for _ in 0..5 {
            if sentryusb_shell::run("mount", &[mnt]).await.is_ok() {
                mounted = true;
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        if mounted {
            for dir in *dirs {
                let _ = std::fs::create_dir_all(format!("{}/{}", mnt, dir));
            }
            let _ = std::fs::write(format!("{}/.metadata_never_index", mnt), "");
            let _ = sentryusb_shell::run("umount", &["-l", mnt]).await;
        }
    }

    Ok(())
}
