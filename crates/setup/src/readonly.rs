//! Read-only root filesystem — replaces `make-root-fs-readonly.sh`.
//!
//! Disables unnecessary services, removes packages that write frequently,
//! and configures tmpfs overlays for directories that need to be writable.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::env::SetupEnv;

/// Make the root filesystem read-only.
pub async fn make_readonly(env: &SetupEnv, progress: &dyn Fn(&str)) -> Result<()> {
    if env.get_bool("SKIP_READONLY", false) {
        progress("SKIP_READONLY is set, skipping read-only filesystem setup");
        return Ok(());
    }

    progress("Making root filesystem read-only...");

    // Ensure boot partition is writable for cmdline.txt edits
    for mp in &["/sentryusb", "/boot/firmware", "/boot"] {
        if Path::new(mp).exists() {
            let _ = sentryusb_shell::run("mount", &[mp, "-o", "remount,rw"]).await;
            break;
        }
    }

    // Disable services that write frequently
    progress("Disabling unnecessary services...");
    for svc in &["apt-daily.timer", "apt-daily-upgrade.timer"] {
        let _ = sentryusb_shell::run("systemctl", &["disable", svc]).await;
    }
    // Disable services that conflict with USB gadget
    for svc in &["amlogic-adbd", "radxa-adbd", "radxa-usbnet", "armbian-led-state"] {
        let _ = sentryusb_shell::run("systemctl", &["disable", svc]).await;
    }

    // Protect essential networking packages from autoremove
    for pkg in &[
        "network-manager", "wpasupplicant", "wpa-supplicant", "ifupdown",
        "dhcpcd", "dhcpcd5", "isc-dhcp-client", "firmware-brcm80211",
        "firmware-realtek", "firmware-atheros", "firmware-iwlwifi",
        "firmware-misc-nonfree",
    ] {
        if sentryusb_shell::run("dpkg", &["-s", pkg]).await.is_ok() {
            let _ = sentryusb_shell::run("apt-mark", &["manual", pkg]).await;
        }
    }

    // Remove packages that write constantly
    progress("Removing packages incompatible with read-only root...");
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(120),
        "apt-get", &["remove", "-y", "--purge", "triggerhappy", "logrotate", "dphys-swapfile"],
    ).await;
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(120),
        "apt-get", &["-y", "autoremove", "--purge"],
    ).await;

    // Install busybox-syslogd and ntp
    progress("Installing ntp and busybox-syslogd...");
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(120),
        "bash", &["-c", "apt-get -y install ntp busybox-syslogd; dpkg --purge rsyslog"],
    ).await;

    // Configure tmpfs mounts for writable directories
    progress("Configuring tmpfs mounts...");
    configure_tmpfs_mounts(env).await?;

    // Add fastboot and read-only to cmdline.txt
    if let Some(cmdline_path) = &env.cmdline_path {
        append_cmdline_param(cmdline_path, "fastboot")?;
        append_cmdline_param(cmdline_path, "noswap")?;
        append_cmdline_param(cmdline_path, "ro")?;
    }

    // Make root mount read-only in fstab
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let mut new_lines = Vec::new();
    for line in fstab.lines() {
        if line.contains("/ ") && line.contains("ext4") && !line.starts_with('#') {
            // Replace rw with ro
            let new_line = if line.contains(",rw") {
                line.replace(",rw", ",ro")
            } else if line.contains("defaults") {
                line.replace("defaults", "defaults,ro")
            } else {
                line.to_string()
            };
            new_lines.push(new_line);
        } else {
            new_lines.push(line.to_string());
        }
    }
    std::fs::write("/etc/fstab", new_lines.join("\n") + "\n")?;

    // Install remountfs_rw helper — now a subcommand of the sentryusb binary
    // but also create a bash wrapper for compatibility
    let wrapper = r#"#!/bin/bash
mount / -o remount,rw
"#;
    let _ = std::fs::create_dir_all("/root/bin");
    std::fs::write("/root/bin/remountfs_rw", wrapper)?;
    let _ = sentryusb_shell::run("chmod", &["+x", "/root/bin/remountfs_rw"]).await;

    progress("Read-only filesystem setup complete.");
    Ok(())
}

/// Append a parameter to cmdline.txt if it's not already present.
fn append_cmdline_param(path: &str, param: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let trimmed = content.trim();

    // Check if already present (at end or followed by space)
    if trimmed.split_whitespace().any(|w| w == param) {
        return Ok(());
    }

    std::fs::write(path, format!("{} {}\n", trimmed, param))?;
    info!("Added '{}' to {}", param, path);
    Ok(())
}

/// Set up tmpfs mounts for directories that need to be writable.
async fn configure_tmpfs_mounts(_env: &SetupEnv) -> Result<()> {
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();

    let tmpfs_mounts = [
        ("tmpfs /tmp tmpfs nosuid,nodev 0 0", "/tmp"),
        ("tmpfs /var/log tmpfs nosuid,nodev 0 0", "/var/log"),
        ("tmpfs /var/tmp tmpfs nosuid,nodev 0 0", "/var/tmp"),
        ("tmpfs /var/lib/dhcp tmpfs nosuid,nodev 0 0", "/var/lib/dhcp"),
        ("tmpfs /var/lib/dhcpcd5 tmpfs nosuid,nodev 0 0", "/var/lib/dhcpcd5"),
        ("tmpfs /var/spool tmpfs nosuid,nodev 0 0", "/var/spool"),
    ];

    let mut additions = String::new();
    for (entry, check_path) in &tmpfs_mounts {
        if !fstab.contains(check_path) {
            additions.push_str(entry);
            additions.push('\n');
        }
    }

    if !additions.is_empty() {
        let mut new_fstab = fstab;
        if !new_fstab.ends_with('\n') {
            new_fstab.push('\n');
        }
        new_fstab.push_str(&additions);
        std::fs::write("/etc/fstab", new_fstab)?;
    }

    Ok(())
}
