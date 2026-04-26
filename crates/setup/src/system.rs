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
            Duration::from_secs(600),
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

/// Configure Samba shares if enabled. Port of `configure-samba.sh`.
///
/// Critical bits the first port missed:
///   * tmpfs entries for /var/run/samba + /var/cache/samba (without them
///     smbd can't write PID/cache on a read-only root).
///   * /var/lib/samba → /mutable/varlib/samba symlink (so bond databases
///     survive reboots).
///   * Default password for the `pi` user (so shares are actually usable).
pub async fn configure_samba(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    if !env.get_bool("SAMBA_ENABLED", false) {
        info!("Samba not enabled, skipping");
        return Ok(false);
    }

    emitter.begin_phase("samba", "Samba share");
    emitter.progress("Configuring Samba...");

    let guest = env.get_bool("SAMBA_GUEST", false);
    let guest_ok = if guest { "yes" } else { "no" };

    let smbd_installed = sentryusb_shell::run("which", &["smbd"]).await.is_ok();

    if !smbd_installed {
        emitter.progress("Installing samba and dependencies...");

        // Move writable dirs off root BEFORE the package install — apt may
        // run smbd briefly and we don't want those writes to land on the
        // soon-to-be-readonly root.
        let _ = std::fs::create_dir_all("/var/cache/samba");
        let _ = std::fs::create_dir_all("/var/run/samba");

        let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
        if !fstab.contains("samba") {
            let mut new_fstab = fstab;
            if !new_fstab.ends_with('\n') {
                new_fstab.push('\n');
            }
            new_fstab.push_str("tmpfs /var/run/samba tmpfs nodev,nosuid 0 0\n");
            new_fstab.push_str("tmpfs /var/cache/samba tmpfs nodev,nosuid 0 0\n");
            std::fs::write("/etc/fstab", new_fstab)?;
        }
        let _ = sentryusb_shell::run("mount", &["/var/cache/samba"]).await;
        let _ = sentryusb_shell::run("mount", &["/var/run/samba"]).await;

        // Migrate /var/lib/samba to /mutable so bond databases persist.
        if !Path::new("/var/lib/samba").is_symlink() {
            if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"])
                .await
                .is_err()
            {
                let _ = sentryusb_shell::run("mount", &["/mutable"]).await;
            }
            let _ = std::fs::create_dir_all("/mutable/varlib");
            if Path::new("/var/lib/samba").is_dir() {
                let _ = sentryusb_shell::run(
                    "mv", &["/var/lib/samba", "/mutable/varlib/"],
                ).await;
            } else {
                let _ = std::fs::create_dir_all("/mutable/varlib/samba");
            }
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink("/mutable/varlib/samba", "/var/lib/samba");
        }

        // Install samba non-interactively.
        let mut install = tokio::process::Command::new("apt-get");
        install.env("DEBIAN_FRONTEND", "noninteractive")
            .args(["-y", "install", "samba"]);
        let status = tokio::time::timeout(
            Duration::from_secs(300),
            install.status(),
        ).await.context("apt-get install samba timed out")??;
        if !status.success() {
            anyhow::bail!("apt-get install samba failed");
        }

        // Start smbd so smbpasswd can register the `pi` user, then stop.
        let _ = sentryusb_shell::run("service", &["smbd", "start"]).await;
        set_default_samba_password().await;
        let _ = sentryusb_shell::run("service", &["smbd", "stop"]).await;

        emitter.progress("Samba installed.");
    }

    // Remove obsolete fstab entry.
    sed_delete_line_matching("/etc/fstab", |l| {
        l == "tmpfs /mnt/smbexport tmpfs nodev,nosuid 0 0"
    })?;

    // Move link folder from backingfiles to mutable if needed.
    if !Path::new("/mutable/TeslaCam").is_dir() && Path::new("/backingfiles/TeslaCam").is_dir() {
        emitter.progress("Moving TeslaCam symlink folder from backingfiles to mutable");
        let _ = sentryusb_shell::run(
            "mv", &["/backingfiles/TeslaCam", "/mutable/TeslaCam"],
        ).await;
    }

    // Always update smb.conf — matches bash behavior so upgrade installs
    // pick up config improvements. Exact contents mirror configure-samba.sh
    // so Samba clients behave identically across Go and Rust builds.
    let smb_conf = format!(
        r#"[global]
   deadtime = 2
   workgroup = WORKGROUP
   dns proxy = no
   log file = /var/log/samba.log.%m
   max log size = 1000
   syslog = 0
   panic action = /usr/share/samba/panic-action %d
   server role = standalone server
   passdb backend = tdbsam
   obey pam restrictions = yes
   unix password sync = yes
   passwd program = /usr/bin/passwd %u
   passwd chat = *Enter\snew\s*\spassword:* %n\n *Retype\snew\s*\spassword:* %n\n *password\supdated\ssuccessfully* .
   pam password change = yes
   map to guest = bad user
   min protocol = SMB2
   usershare allow guests = yes
   unix extensions = no
   wide links = yes

[TeslaCam]
   read only = yes
   locking = no
   path = /mutable/TeslaCam
   guest ok = {guest_ok}
   create mask = 0775
   veto files = /._*/.DS_Store/
   delete veto files = yes
   root preexec = /root/bin/make_snapshot.sh
"#
    );
    let _ = std::fs::create_dir_all("/etc/samba");
    std::fs::write("/etc/samba/smb.conf", smb_conf)?;
    let _ = sentryusb_shell::run("systemctl", &["enable", "smbd"]).await;
    let _ = sentryusb_shell::run("systemctl", &["restart", "smbd"]).await;

    Ok(true)
}

/// Set the default Samba password for the `pi` user by piping
/// `raspberry\nraspberry\n` through `smbpasswd -s -a pi`.
async fn set_default_samba_password() {
    use tokio::io::AsyncWriteExt;

    let mut child = match tokio::process::Command::new("smbpasswd")
        .args(["-s", "-a", "pi"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            info!("smbpasswd spawn failed: {}", e);
            return;
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"raspberry\nraspberry\n").await;
        drop(stdin);
    }
    let _ = child.wait().await;
}

/// Remove lines from a file where `pred(line) == true`.
fn sed_delete_line_matching<F: Fn(&str) -> bool>(path: &str, pred: F) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let had_trailing = content.ends_with('\n');
    let kept: Vec<&str> = content.lines().filter(|l| !pred(l)).collect();
    let mut out = kept.join("\n");
    if had_trailing {
        out.push('\n');
    }
    std::fs::write(path, out)?;
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

/// Ensure required system packages are installed. Only announces a phase if
/// one or more packages actually need installing.
///
/// We test for the *binary* via `which` rather than the *package* via
/// `dpkg -s` because Debian splits binaries across packages differently
/// across releases — e.g. `fdisk` is its own package on bookworm but ships
/// inside `util-linux` on bullseye, so `dpkg -s fdisk` would falsely report
/// missing on bullseye and `apt-get install fdisk` would then fail. The
/// binary check works regardless of which package owns the file.
pub async fn install_required_packages(emitter: &SetupEmitter) -> Result<bool> {
    // (binary_to_check, package_to_install_when_missing)
    //
    // `ntpsec-ntpdig` provides the `ntpdig` binary that
    // `run/archiveloop`'s `set_time()` calls via
    //   `ntpdig -S time.google.com || sntp -S 129.6.15.28`
    // On a fresh Pi OS bookworm image neither tool is present; without
    // this, archiveloop logs "sntp failed, retrying..." five times per
    // cycle and falls through with "Failed to set time" — harmless for
    // the clock (systemd-timesyncd keeps sync quietly in the background)
    // but it floods the archive log and causes a cold-boot window where
    // clip folder timestamps are wrong until timesyncd catches up.
    let packages: &[(&str, &str)] = &[
        ("dos2unix", "dos2unix"),
        ("parted", "parted"),
        ("fdisk", "fdisk"),
        ("curl", "curl"),
        ("rsync", "rsync"),
        ("jq", "jq"),
        ("ntpdig", "ntpsec-ntpdig"),
    ];
    let mut to_install: Vec<&str> = Vec::new();

    for (binary, package) in packages {
        if sentryusb_shell::run("which", &[binary]).await.is_err() {
            to_install.push(*package);
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
///
/// Previously this only read `/etc/timezone`, but on Raspberry Pi OS
/// (bookworm/bullseye) `timedatectl set-timezone` primarily rewrites the
/// `/etc/localtime` symlink and `/etc/timezone` is often absent. That
/// meant every mid-setup resume (dwc2 reboot, root-shrink reboot,
/// cmdline reboot…) would re-decide the timezone wasn't set and re-emit
/// the progress line, flooding the setup log with duplicate "Setting
/// timezone to X" messages on a single run. Read both sources before
/// acting, and keep `/etc/timezone` in sync ourselves so legacy tools
/// that consult it (apt, logrotate, some cron jobs) agree with systemd.
pub async fn configure_timezone(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let raw = match env.config.get("TIME_ZONE") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return Ok(false),
    };

    // Newer Pi OS / Debian images (bookworm and later) ship only the
    // canonical IANA tzdata zones and drop the legacy `US/*` and
    // single-name aliases that older images still carried. Configs
    // saved with one of those shortcuts then fail timedatectl with
    // "Invalid or not installed time zone". Map them up front so we
    // hand timedatectl a name every shipped tzdata version recognizes.
    let tz = normalize_timezone(&raw);

    if current_timezone().as_deref() == Some(tz.as_str()) {
        return Ok(false);
    }

    emitter.progress(&format!("Setting timezone to {}", tz));
    sentryusb_shell::run("timedatectl", &["set-timezone", &tz]).await?;

    // Keep /etc/timezone in sync with the symlink. On images where the
    // file is missing this also creates it, which makes our own
    // idempotency check cheap on the next resume.
    let _ = std::fs::write("/etc/timezone", format!("{}\n", tz));

    Ok(true)
}

/// Translate legacy tzdata aliases to their canonical IANA names.
/// Returns the input unchanged if it isn't a known alias.
fn normalize_timezone(tz: &str) -> String {
    match tz {
        // US/* aliases — all dropped from minimal tzdata installs
        "US/Alaska" => "America/Anchorage",
        "US/Aleutian" => "America/Adak",
        "US/Arizona" => "America/Phoenix",
        "US/Central" => "America/Chicago",
        "US/East-Indiana" => "America/Indiana/Indianapolis",
        "US/Eastern" => "America/New_York",
        "US/Hawaii" => "Pacific/Honolulu",
        "US/Indiana-Starke" => "America/Indiana/Knox",
        "US/Michigan" => "America/Detroit",
        "US/Mountain" => "America/Denver",
        "US/Pacific" => "America/Los_Angeles",
        "US/Samoa" => "Pacific/Pago_Pago",
        // Common single-name legacy zones
        "GMT" | "UTC" | "Universal" | "Zulu" => "UTC",
        "Navajo" => "America/Denver",
        "Cuba" => "America/Havana",
        "Egypt" => "Africa/Cairo",
        "Eire" => "Europe/Dublin",
        "GB" | "GB-Eire" => "Europe/London",
        "Hongkong" => "Asia/Hong_Kong",
        "Iceland" => "Atlantic/Reykjavik",
        "Iran" => "Asia/Tehran",
        "Israel" => "Asia/Jerusalem",
        "Jamaica" => "America/Jamaica",
        "Japan" => "Asia/Tokyo",
        "Kwajalein" => "Pacific/Kwajalein",
        "Libya" => "Africa/Tripoli",
        "Poland" => "Europe/Warsaw",
        "Portugal" => "Europe/Lisbon",
        "Singapore" => "Asia/Singapore",
        "Turkey" => "Europe/Istanbul",
        other => return other.to_string(),
    }
    .to_string()
}

#[cfg(test)]
mod timezone_normalize_tests {
    use super::normalize_timezone;

    #[test]
    fn maps_us_aliases() {
        assert_eq!(normalize_timezone("US/Eastern"), "America/New_York");
        assert_eq!(normalize_timezone("US/Pacific"), "America/Los_Angeles");
        assert_eq!(normalize_timezone("US/Hawaii"), "Pacific/Honolulu");
    }

    #[test]
    fn passes_canonical_through() {
        assert_eq!(normalize_timezone("America/New_York"), "America/New_York");
        assert_eq!(normalize_timezone("Europe/Berlin"), "Europe/Berlin");
    }

    #[test]
    fn maps_single_name_legacy() {
        assert_eq!(normalize_timezone("Japan"), "Asia/Tokyo");
        assert_eq!(normalize_timezone("Eire"), "Europe/Dublin");
    }
}

/// Best-effort detection of the system's current timezone. Tries
/// `/etc/timezone` first (cheap, textual), falls back to the target of
/// the `/etc/localtime` symlink (systemd's source of truth on Pi OS).
/// Returns `None` only when neither source is usable.
fn current_timezone() -> Option<String> {
    if let Ok(raw) = std::fs::read_to_string("/etc/timezone") {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    let link = std::fs::read_link("/etc/localtime").ok()?;
    let s = link.to_string_lossy();
    s.find("/zoneinfo/").map(|idx| s[idx + "/zoneinfo/".len()..].to_string())
}

/// Configure the RTC if enabled.
///
/// Dispatches on Pi model:
///   * Pi 5: uses the built-in RTC via `/dev/rtc0`; installs
///     `sentryusb-hwclock.service` for boot-time hctosys sync and optionally
///     enables trickle charging via `dtparam=rtc_bbat_vchg`.
///   * Other models: adds a DS3231 I²C overlay to config.txt (for users
///     wiring in an external RTC module).
pub async fn configure_rtc(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    if env.pi_model == crate::env::PiModel::Pi5 {
        return configure_rtc_pi5(env, emitter).await;
    }
    configure_rtc_ds3231(env, emitter).await
}

/// Pi 5 built-in RTC — port of `configure-rtc.sh`.
async fn configure_rtc_pi5(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let config_path = match &env.piconfig_path {
        Some(p) => p.clone(),
        None => return Ok(false),
    };

    let enabled = env.get_bool("RTC_BATTERY_ENABLED", false);
    let trickle = env.get_bool("RTC_TRICKLE_CHARGE", false);

    // Quick idempotency check. If already in the desired state, silent skip.
    let service_path = "/lib/systemd/system/sentryusb-hwclock.service";
    let config = std::fs::read_to_string(&config_path).unwrap_or_default();
    let service_installed = Path::new(service_path).exists();
    let trickle_present = config.lines().any(|l| l.starts_with("dtparam=rtc_bbat_vchg"));

    if enabled && service_installed && (trickle == trickle_present) {
        return Ok(false);
    }
    if !enabled && !service_installed && !trickle_present {
        return Ok(false);
    }

    emitter.begin_phase("rtc", "Real-time clock");

    if enabled {
        emitter.progress("Enabling RTC battery support");

        // Disable fake-hwclock so it doesn't fight the real RTC.
        if sentryusb_shell::run("systemctl", &["is-enabled", "fake-hwclock.service"])
            .await
            .map(|o| o.trim() == "enabled")
            .unwrap_or(false)
        {
            emitter.progress("Disabling fake-hwclock");
            let _ = sentryusb_shell::run("systemctl", &["stop", "fake-hwclock.service"]).await;
            let _ = sentryusb_shell::run("systemctl", &["disable", "fake-hwclock.service"]).await;
        }

        emitter.progress("Creating sentryusb-hwclock.service");
        std::fs::write(service_path, SENTRYUSB_HWCLOCK_SERVICE)?;
        let _ = sentryusb_shell::run("systemctl", &["daemon-reload"]).await;
        let _ = sentryusb_shell::run("systemctl", &["enable", "sentryusb-hwclock.service"]).await;

        // Sync current system time to the RTC right now so reboots during
        // the rest of setup have a good time source.
        rtc_sync_systohc(emitter).await;

        // Trickle charging (only relevant for rechargeable cells).
        update_trickle_charge(&config_path, trickle, emitter)?;

        emitter.progress("RTC battery support enabled");
    } else {
        emitter.progress("RTC battery support disabled, ensuring fake-hwclock is active");

        if Path::new(service_path).exists() {
            let _ = sentryusb_shell::run("systemctl", &["stop", "sentryusb-hwclock.service"]).await;
            let _ = sentryusb_shell::run("systemctl", &["disable", "sentryusb-hwclock.service"]).await;
            let _ = std::fs::remove_file(service_path);
            let _ = sentryusb_shell::run("systemctl", &["daemon-reload"]).await;
        }

        update_trickle_charge(&config_path, false, emitter)?;

        // Re-enable fake-hwclock if it was disabled.
        if sentryusb_shell::run("systemctl", &["is-enabled", "fake-hwclock.service"])
            .await
            .map(|o| o.trim() == "disabled")
            .unwrap_or(false)
        {
            emitter.progress("Re-enabling fake-hwclock");
            let _ = sentryusb_shell::run("systemctl", &["enable", "fake-hwclock.service"]).await;
        }

        emitter.progress("fake-hwclock restored");
    }

    Ok(true)
}

/// External DS3231 I²C RTC — kept as a feature addition for non-Pi5 users
/// who wire their own RTC module (the Go/bash project never had this path).
async fn configure_rtc_ds3231(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
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

/// Add or remove `dtparam=rtc_bbat_vchg=3000000` in config.txt.
fn update_trickle_charge(config_path: &str, enable: bool, emitter: &SetupEmitter) -> Result<()> {
    let content = std::fs::read_to_string(config_path).unwrap_or_default();
    let has_active = content.lines().any(|l| l.starts_with("dtparam=rtc_bbat_vchg"));

    if enable {
        if has_active {
            // Normalize any existing value to 3000000.
            let new: String = content
                .lines()
                .map(|l| {
                    if l.trim_start_matches('#').starts_with("dtparam=rtc_bbat_vchg") {
                        "dtparam=rtc_bbat_vchg=3000000".to_string()
                    } else {
                        l.to_string()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            std::fs::write(config_path, new + "\n")?;
        } else {
            emitter.progress("Enabling RTC trickle charging (3.0V)");
            let mut new = content;
            if !new.ends_with('\n') {
                new.push('\n');
            }
            new.push_str("dtparam=rtc_bbat_vchg=3000000\n");
            std::fs::write(config_path, new)?;
        }
    } else if has_active {
        emitter.progress("Removing RTC trickle charging");
        let kept: Vec<&str> = content
            .lines()
            .filter(|l| !l.starts_with("dtparam=rtc_bbat_vchg"))
            .collect();
        let mut new = kept.join("\n");
        if content.ends_with('\n') {
            new.push('\n');
        }
        std::fs::write(config_path, new)?;
    }
    Ok(())
}

/// Write current system time to the RTC via `/dev/rtc0` ioctl. Uses an
/// embedded Python one-liner because hwclock is not on minimal images and
/// `/sys/class/rtc/rtc0/since_epoch` is read-only on rpi-rtc.
async fn rtc_sync_systohc(emitter: &SetupEmitter) {
    if !Path::new("/dev/rtc0").exists() {
        info!("RTC: /dev/rtc0 not found, skipping systohc sync");
        return;
    }
    let py = r#"
import fcntl, struct, time
t = time.gmtime()
# struct rtc_time: sec, min, hour, mday, mon(0-based), year-1900, wday, yday, isdst
data = struct.pack('9i', t.tm_sec, t.tm_min, t.tm_hour, t.tm_mday, t.tm_mon-1, t.tm_year-1900, t.tm_wday, t.tm_yday, -1)
with open('/dev/rtc0', 'wb') as f:
    fcntl.ioctl(f.fileno(), 0x4024700a, data)  # RTC_SET_TIME
"#;
    match sentryusb_shell::run("python3", &["-c", py]).await {
        Ok(_) => emitter.progress("Synced system time to RTC"),
        Err(_) => emitter.progress("Warning: failed to sync time to RTC"),
    }
}

const SENTRYUSB_HWCLOCK_SERVICE: &str = r#"[Unit]
Description=SentryUSB hardware clock sync
DefaultDependencies=no
After=dev-rtc0.device
Before=time-sync.target sysinit.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/bash -c '\
  epoch=$(python3 -c "\
import fcntl, struct, calendar, time;\
f=open(\"/dev/rtc0\",\"rb\");\
d=fcntl.ioctl(f.fileno(),0x80247009,b\"\\x00\"*36);\
f.close();\
v=struct.unpack(\"9i\",d);\
t=time.struct_time((v[5]+1900,v[4]+1,v[3],v[2],v[1],v[0],v[6],v[7],v[8]));\
print(int(calendar.timegm(t)))\
" 2>/dev/null);\
  if [ -n "$epoch" ] && [ "$epoch" -gt 1704067200 ]; then\
    date -u -s "@$epoch" > /dev/null;\
  fi'

[Install]
WantedBy=sysinit.target
"#;
