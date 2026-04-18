//! System configuration — replaces various configure-*.sh scripts.
//!
//! Handles hostname, dwc2 overlay, Avahi mDNS, SSH hardening, Samba, etc.
//!
//! Each phase-level function only announces itself via `emitter.begin_phase`
//! when it actually has work to do. No-op re-runs are silent so the wizard's
//! phase list doesn't light up for phases that did nothing.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::env::SetupEnv;
use crate::SetupEmitter;

/// Set the Pi hostname (and /etc/hosts). Idempotent — silent if already set.
///
/// This phase is bundled with `configure_timezone` under the "System
/// configuration" UI phase. The caller announces that phase once; we just do
/// the work quietly.
pub async fn configure_hostname(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let hostname = env.get("SENTRYUSB_HOSTNAME", "sentryusb");
    let current = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let current = current.trim();
    if current == hostname {
        return Ok(false);
    }

    emitter.progress(&format!("Setting hostname to '{}'", hostname));
    std::fs::write("/etc/hostname", format!("{}\n", hostname))?;
    if sentryusb_shell::run("hostnamectl", &["set-hostname", &hostname]).await.is_err() {
        sentryusb_shell::run("hostname", &[&hostname]).await?;
    }

    let hosts = std::fs::read_to_string("/etc/hosts").unwrap_or_default();
    let new_hosts = if hosts.contains(current) && !current.is_empty() {
        hosts.replace(current, &hostname)
    } else if !hosts.contains(&hostname) {
        format!("{}\n127.0.1.1\t{}\n", hosts.trim_end(), hostname)
    } else {
        hosts
    };
    std::fs::write("/etc/hosts", new_hosts)?;
    Ok(true)
}

/// Configure dwc2 USB gadget overlay in config.txt and cmdline.txt.
pub async fn configure_dwc2(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    emitter.progress("Configuring USB gadget (dwc2) overlay...");

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
///
/// Idempotent: if the service file is already present and matches, do
/// nothing and return `false` so the caller can skip announcing this phase.
pub async fn configure_avahi(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let hostname = env.get("SENTRYUSB_HOSTNAME", "sentryusb");
    let service_file = "/etc/avahi/services/sentryusb.service";
    let desired = format!(
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
    );

    let needs_install = sentryusb_shell::run("which", &["avahi-daemon"]).await.is_err();
    let existing = std::fs::read_to_string(service_file).unwrap_or_default();
    let content_matches = existing == desired;

    if !needs_install && content_matches {
        return Ok(false);
    }

    emitter.begin_phase("avahi", "mDNS service");
    emitter.progress("Configuring Avahi mDNS service...");

    if needs_install {
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(120),
            "apt-get", &["-y", "install", "avahi-daemon"],
        ).await.context("failed to install avahi-daemon")?;
    }

    if !content_matches {
        let _ = std::fs::create_dir_all("/etc/avahi/services");
        std::fs::write(service_file, desired)?;
    }

    let _ = sentryusb_shell::run("systemctl", &["enable", "avahi-daemon"]).await;
    let _ = sentryusb_shell::run("systemctl", &["restart", "avahi-daemon"]).await;

    emitter.progress(&format!("mDNS configured: {}.local", hostname));
    Ok(true)
}

/// Harden SSH configuration. Idempotent — silent when no changes are needed.
pub async fn configure_ssh(emitter: &SetupEmitter) -> Result<bool> {
    let sshd_config = Path::new("/etc/ssh/sshd_config");
    if !sshd_config.exists() {
        info!("sshd_config not found, skipping SSH hardening");
        return Ok(false);
    }

    let content = std::fs::read_to_string(sshd_config)?;
    let settings = [
        ("PermitRootLogin", "prohibit-password"),
        ("PasswordAuthentication", "no"),
        ("ChallengeResponseAuthentication", "no"),
        ("UsePAM", "yes"),
    ];

    // Quick idempotency check — if every setting already has an active line
    // with the desired value, there's nothing to do.
    let all_set = settings.iter().all(|(k, v)| {
        let expected = format!("{} {}", k, v);
        content.lines().any(|l| l.trim_start() == expected)
    });
    if all_set {
        return Ok(false);
    }

    emitter.begin_phase("ssh", "SSH hardening");
    emitter.progress("Hardening SSH...");

    let mut lines: Vec<String> = content.lines().map(String::from).collect();
    for (key, value) in &settings {
        let directive = format!("{} {}", key, value);
        let found = lines.iter_mut().any(|line| {
            if line.trim_start().starts_with(key)
                || line.trim_start().starts_with(&format!("#{}", key))
            {
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
    Ok(true)
}

/// Configure Samba shares if enabled. Gated at runner level, but this
/// function is still idempotent if called with SAMBA_ENABLED unset.
pub async fn configure_samba(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    if !env.get_bool("SAMBA_ENABLED", false) {
        info!("Samba not enabled, skipping");
        return Ok(false);
    }

    emitter.begin_phase("samba", "Samba share");
    emitter.progress("Configuring Samba...");

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

    Ok(true)
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

/// Ensure required system packages are installed. Only announces a phase if
/// one or more packages actually need installing.
pub async fn install_required_packages(emitter: &SetupEmitter) -> Result<bool> {
    let packages = ["dos2unix", "parted", "fdisk", "curl", "rsync", "jq"];
    let mut to_install = Vec::new();

    for pkg in &packages {
        if sentryusb_shell::run("dpkg", &["-s", pkg]).await.is_err() {
            to_install.push(*pkg);
        }
    }

    if to_install.is_empty() {
        return Ok(false);
    }

    emitter.begin_phase("required_packages", "Installing required packages");
    emitter.progress(&format!("Installing: {}", to_install.join(", ")));
    let mut args = vec!["-y", "install"];
    args.extend(&to_install);
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "apt-get", &args,
    ).await.context("failed to install required packages")?;

    Ok(true)
}

/// Set the system timezone. Idempotent — silent if already matching.
pub async fn configure_timezone(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let tz = match env.config.get("TIME_ZONE") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return Ok(false),
    };

    // Check current timezone; /etc/timezone is simpler than parsing timedatectl
    let current = std::fs::read_to_string("/etc/timezone")
        .unwrap_or_default()
        .trim()
        .to_string();
    if current == tz {
        return Ok(false);
    }

    emitter.progress(&format!("Setting timezone to {}", tz));
    sentryusb_shell::run("timedatectl", &["set-timezone", &tz]).await?;
    Ok(true)
}

/// Configure the RTC (DS3231) if enabled. Idempotent — silent when the
/// overlay is already configured.
pub async fn configure_rtc(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    if !env.get_bool("RTC_BATTERY_ENABLED", false) {
        return Ok(false);
    }

    let config_path = match &env.piconfig_path {
        Some(p) => p.clone(),
        None => return Ok(false),
    };
    let config = std::fs::read_to_string(&config_path).unwrap_or_default();
    if config.contains("dtoverlay=i2c-rtc,ds3231") {
        return Ok(false);
    }

    emitter.begin_phase("rtc", "Real-time clock");
    emitter.progress("Configuring RTC (DS3231)...");

    let addition = if env.get_bool("RTC_TRICKLE_CHARGE", false) {
        "\ndtoverlay=i2c-rtc,ds3231,trickle-resistor-ohms=11800\n"
    } else {
        "\ndtoverlay=i2c-rtc,ds3231\n"
    };
    std::fs::write(&config_path, format!("{}{}", config, addition))?;
    Ok(true)
}
