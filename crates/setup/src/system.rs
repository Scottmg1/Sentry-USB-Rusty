//! System configuration — replaces various configure-*.sh scripts.
//!
//! Handles hostname, dwc2 overlay, Avahi mDNS, SSH hardening, Samba, etc.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::env::SetupEnv;

/// Set the Pi hostname.
pub async fn configure_hostname(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    let hostname = env.get("SENTRYUSB_HOSTNAME", "sentryusb");
    progress(&format!("Setting hostname to '{}'", hostname));

    let current = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let current = current.trim();

    if current != hostname {
        std::fs::write("/etc/hostname", format!("{}\n", hostname))?;
        if sentryusb_shell::run("hostnamectl", &["set-hostname", &hostname]).await.is_err() {
            sentryusb_shell::run("hostname", &[&hostname]).await?;
        }

        // Update /etc/hosts
        let hosts = std::fs::read_to_string("/etc/hosts").unwrap_or_default();
        let new_hosts = if hosts.contains(current) && !current.is_empty() {
            hosts.replace(current, &hostname)
        } else if !hosts.contains(&hostname) {
            format!("{}\n127.0.1.1\t{}\n", hosts.trim_end(), hostname)
        } else {
            hosts
        };
        std::fs::write("/etc/hosts", new_hosts)?;
    }

    Ok(())
}

/// Configure dwc2 USB gadget overlay in config.txt and cmdline.txt.
pub async fn configure_dwc2(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    progress("Configuring USB gadget (dwc2) overlay...");

    // Add dwc2 overlay to config.txt
    if let Some(config_path) = &env.piconfig_path {
        let config = std::fs::read_to_string(config_path).unwrap_or_default();
        if !config.contains("dtoverlay=dwc2") {
            let addition = format!(
                "\n[{}]\ndtoverlay=dwc2\n",
                env.pi_model.config_section()
            );
            std::fs::write(config_path, format!("{}{}", config, addition))?;
            info!("Added dwc2 overlay to {}", config_path);
        }
    }

    // Add modules-load=dwc2 to cmdline.txt
    if let Some(cmdline_path) = &env.cmdline_path {
        let cmdline = std::fs::read_to_string(cmdline_path).unwrap_or_default();
        if !cmdline.contains("modules-load=dwc2") {
            let new = cmdline.trim().to_string() + " modules-load=dwc2";
            std::fs::write(cmdline_path, format!("{}\n", new))?;
            info!("Added modules-load=dwc2 to cmdline.txt");
        }
    }

    Ok(())
}

/// Set up Avahi mDNS service for local network discovery.
pub async fn configure_avahi(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    progress("Configuring Avahi mDNS service...");

    let hostname = env.get("SENTRYUSB_HOSTNAME", "sentryusb");

    // Ensure avahi-daemon is installed
    if sentryusb_shell::run("which", &["avahi-daemon"]).await.is_err() {
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(120),
            "apt-get", &["-y", "install", "avahi-daemon"],
        ).await.context("failed to install avahi-daemon")?;
    }

    // Write the service file
    let service_file = "/etc/avahi/services/sentryusb.service";
    let _ = std::fs::create_dir_all("/etc/avahi/services");
    std::fs::write(service_file, format!(
        r#"<?xml version="1.0" standalone='no'?>
<!DOCTYPE service-group SYSTEM "avahi-service.dtd">
<service-group>
  <name replace-wildcards="yes">{hostname}</name>
  <service>
    <type>_http._tcp</type>
    <port>80</port>
  </service>
</service-group>
"#
    ))?;

    let _ = sentryusb_shell::run("systemctl", &["enable", "avahi-daemon"]).await;
    let _ = sentryusb_shell::run("systemctl", &["restart", "avahi-daemon"]).await;

    progress(&format!("mDNS configured: {}.local", hostname));
    Ok(())
}

/// Harden SSH configuration.
pub async fn configure_ssh(progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    progress("Hardening SSH...");

    let sshd_config = Path::new("/etc/ssh/sshd_config");
    if !sshd_config.exists() {
        info!("sshd_config not found, skipping SSH hardening");
        return Ok(());
    }

    let content = std::fs::read_to_string(sshd_config)?;
    let mut lines: Vec<String> = content.lines().map(String::from).collect();

    let settings = [
        ("PermitRootLogin", "prohibit-password"),
        ("PasswordAuthentication", "no"),
        ("ChallengeResponseAuthentication", "no"),
        ("UsePAM", "yes"),
    ];

    for (key, value) in &settings {
        let directive = format!("{} {}", key, value);
        let found = lines.iter_mut().any(|line| {
            if line.trim_start().starts_with(key) || line.trim_start().starts_with(&format!("#{}", key)) {
                *line = directive.clone();
                true
            } else {
                false
            }
        });
        if !found {
            lines.push(directive);
        }
    }

    std::fs::write(sshd_config, lines.join("\n") + "\n")?;
    let _ = sentryusb_shell::run("systemctl", &["reload", "sshd"]).await;

    Ok(())
}

/// Configure Samba shares if enabled.
pub async fn configure_samba(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    if !env.get_bool("SAMBA_ENABLED", false) {
        info!("Samba not enabled, skipping");
        return Ok(());
    }

    progress("Configuring Samba...");

    // Install samba if needed
    if sentryusb_shell::run("which", &["smbd"]).await.is_err() {
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(300),
            "apt-get", &["-y", "install", "samba"],
        ).await.context("failed to install samba")?;
    }

    let guest = env.get_bool("SAMBA_GUEST", false);
    let guest_ok = if guest { "yes" } else { "no" };

    let smb_conf = format!(
        r#"[global]
workgroup = WORKGROUP
server string = SentryUSB
security = user
map to guest = Bad User
log level = 1

[cam]
path = /mnt/cam
read only = yes
guest ok = {guest_ok}
browseable = yes

[music]
path = /mnt/music
read only = no
guest ok = {guest_ok}
browseable = yes

[lightshow]
path = /mnt/lightshow
read only = no
guest ok = {guest_ok}
browseable = yes
"#
    );

    std::fs::write("/etc/samba/smb.conf", smb_conf)?;
    let _ = sentryusb_shell::run("systemctl", &["enable", "smbd"]).await;
    let _ = sentryusb_shell::run("systemctl", &["restart", "smbd"]).await;

    Ok(())
}

/// Install the sentryusb systemd service.
pub fn install_systemd_service(binary_path: &str) -> Result<()> {
    let service = format!(
        r#"[Unit]
Description=SentryUSB web server
After=network.target

[Service]
Type=simple
Conflicts=nginx.service
ExecStartPre=-/usr/bin/systemctl stop nginx
ExecStart={binary_path} --port 80
Restart=always
RestartSec=3
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
"#
    );

    std::fs::write("/etc/systemd/system/sentryusb.service", service)?;
    Ok(())
}

/// Install the archive loop systemd service.
///
/// Uses the bash archiveloop script for now. This will be ported to a Rust
/// subcommand in a future release.
pub fn install_archive_service() -> Result<()> {
    let service = r#"[Unit]
Description=SentryUSB archiveloop service
DefaultDependencies=no
After=mutable.mount backingfiles.mount

[Service]
Type=simple
ExecStart=/bin/bash /root/bin/archiveloop
Restart=always

[Install]
WantedBy=backingfiles.mount
"#;

    std::fs::write("/lib/systemd/system/sentryusb-archive.service", service)?;
    Ok(())
}

/// Ensure required system packages are installed.
pub async fn install_required_packages(progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    progress("Checking required packages...");

    let packages = ["dos2unix", "parted", "fdisk", "curl", "rsync", "jq"];
    let mut to_install = Vec::new();

    for pkg in &packages {
        if sentryusb_shell::run("dpkg", &["-s", pkg]).await.is_err() {
            to_install.push(*pkg);
        }
    }

    if !to_install.is_empty() {
        progress(&format!("Installing: {}", to_install.join(", ")));
        let mut args = vec!["-y", "install"];
        args.extend(&to_install);
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(300),
            "apt-get", &args,
        ).await.context("failed to install required packages")?;
    }

    Ok(())
}

/// Set the system timezone.
pub async fn configure_timezone(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    if let Some(tz) = env.config.get("TIME_ZONE") {
        if !tz.is_empty() {
            progress(&format!("Setting timezone to {}", tz));
            sentryusb_shell::run("timedatectl", &["set-timezone", tz]).await?;
        }
    }
    Ok(())
}

/// Configure the RTC (DS3231) if enabled.
pub async fn configure_rtc(env: &SetupEnv, progress: &(dyn Fn(&str) + Send + Sync)) -> Result<()> {
    if !env.get_bool("RTC_BATTERY_ENABLED", false) {
        return Ok(());
    }

    progress("Configuring RTC (DS3231)...");

    // Enable I2C
    if let Some(config_path) = &env.piconfig_path {
        let config = std::fs::read_to_string(config_path).unwrap_or_default();
        if !config.contains("dtoverlay=i2c-rtc,ds3231") {
            let addition = if env.get_bool("RTC_TRICKLE_CHARGE", false) {
                "\ndtoverlay=i2c-rtc,ds3231,trickle-resistor-ohms=11800\n"
            } else {
                "\ndtoverlay=i2c-rtc,ds3231\n"
            };
            std::fs::write(config_path, format!("{}{}", config, addition))?;
        }
    }

    Ok(())
}
