//! Hostname, mDNS, SSH, Samba, timezone, package, and RTC configuration.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tracing::info;

use crate::env::SetupEnv;
use crate::SetupEmitter;

/// Sets the hostname and `/etc/hosts`, returning false when already configured.
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

    // Restart Avahi so it advertises the new hostname immediately.
    if sentryusb_shell::run("systemctl", &["-q", "is-active", "avahi-daemon"]).await.is_ok() {
        let _ = sentryusb_shell::run("systemctl", &["restart", "avahi-daemon"]).await;
    }
    Ok(true)
}

const AVAHI_DAEMON_CONF: &str = "/etc/avahi/avahi-daemon.conf";

/// Advertise IPv4-only mDNS to avoid stale SLAAC addresses and browser PNA blocks.
const AVAHI_IPV4_ONLY: &[(&str, &str, &str)] = &[
    ("server", "use-ipv6", "no"),
    ("publish", "publish-aaaa-on-ipv4", "no"),
];

/// Sets one INI key in its named section, deduplicating only within that section.
fn set_ini_key(content: &str, section: &str, key: &str, value: &str) -> (String, bool) {
    let header = format!("[{section}]");
    let desired = format!("{key}={value}");
    let mut out: Vec<&str> = Vec::new();
    let mut in_section = false;
    let mut found_section = false;
    let mut done = false;
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            if in_section && !done {
                // Keep trailing blank lines attached to their section.
                let mut pos = out.len();
                while pos > 0 && out[pos - 1].trim().is_empty() {
                    pos -= 1;
                }
                out.insert(pos, &desired);
                done = true;
            }
            in_section = trimmed == header;
            found_section |= in_section;
            out.push(line);
            continue;
        }
        if in_section && !trimmed.is_empty() {
            let uncommented = trimmed
                .strip_prefix(['#', ';'])
                .map(str::trim_start)
                .unwrap_or(trimmed);
            if let Some(after_key) = uncommented.strip_prefix(key) {
                if after_key.trim_start().starts_with('=') {
                    if !done {
                        out.push(&desired);
                        done = true;
                    }
                    continue;
                }
            }
        }
        out.push(line);
    }
    if !done {
        if !found_section {
            out.push(&header);
        }
        out.push(&desired);
    }
    let mut new_content = out.join("\n");
    new_content.push('\n');
    let changed = new_content != content;
    (new_content, changed)
}

/// Builds an IPv4-only Avahi config; unreadable existing files are preserved.
fn avahi_ipv4_only_rewrite() -> Option<String> {
    let content = match std::fs::read_to_string(AVAHI_DAEMON_CONF) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => {
            tracing::warn!("avahi-daemon.conf unreadable, leaving as-is: {e}");
            return None;
        }
    };
    let mut current = content;
    let mut changed = false;
    for (section, key, value) in AVAHI_IPV4_ONLY {
        let (next, ch) = set_ini_key(&current, section, key, value);
        current = next;
        changed |= ch;
    }
    changed.then_some(current)
}

/// Configures Avahi discovery and returns false when already healthy.
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
    let mut conf_rewrite = avahi_ipv4_only_rewrite();

    // Correct files do not imply an enabled, running daemon.
    let daemon_healthy = !needs_install
        && sentryusb_shell::run("systemctl", &["-q", "is-enabled", "avahi-daemon"]).await.is_ok()
        && sentryusb_shell::run("systemctl", &["-q", "is-active", "avahi-daemon"]).await.is_ok();

    if !needs_install && content_matches && conf_rewrite.is_none() && daemon_healthy {
        return Ok(false);
    }

    emitter.begin_phase("avahi", "mDNS service");
    emitter.progress("Configuring Avahi mDNS service...");

    if needs_install {
        crate::apt::apt_install(
            |m| emitter.progress(m),
            &["avahi-daemon"],
            Duration::from_secs(600),
        ).await.context("failed to install avahi-daemon")?;
        // Recompute from the package-installed configuration.
        conf_rewrite = avahi_ipv4_only_rewrite();
    }

    if !content_matches {
        let _ = std::fs::create_dir_all("/etc/avahi/services");
        std::fs::write(service_file, desired)?;
    }

    if let Some(new_conf) = conf_rewrite {
        emitter.progress("Restricting mDNS advertising to IPv4...");
        let prev = format!("{AVAHI_DAEMON_CONF}.sentryusb-prev");
        if !Path::new(&prev).exists() {
            let _ = std::fs::copy(AVAHI_DAEMON_CONF, &prev);
        }
        // Replace atomically to survive power loss.
        let tmp = format!("{AVAHI_DAEMON_CONF}.sentryusb-tmp");
        std::fs::write(&tmp, new_conf)?;
        if let Ok(meta) = std::fs::metadata(AVAHI_DAEMON_CONF) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        std::fs::rename(&tmp, AVAHI_DAEMON_CONF)?;
    }

    let _ = sentryusb_shell::run("systemctl", &["enable", "avahi-daemon"]).await;
    let _ = sentryusb_shell::run("systemctl", &["restart", "avahi-daemon"]).await;

    emitter.progress(&format!("mDNS configured: {}.local", hostname));
    Ok(true)
}

/// Applies conservative SSH hardening without disabling user password access.
pub async fn configure_ssh(emitter: &SetupEmitter) -> Result<bool> {
    let sshd_config = Path::new("/etc/ssh/sshd_config");
    if !sshd_config.exists() {
        info!("sshd_config not found, skipping SSH hardening");
        return Ok(false);
    }

    let content = std::fs::read_to_string(sshd_config)?;
    // Keep normal-account password access; restrict root to key authentication.
    let settings = [
        ("PermitRootLogin", "prohibit-password"),
        ("UsePAM", "yes"),
    ];

    // Remove only legacy lockout directives written verbatim by this setup.
    let aggressive_lines = ["PasswordAuthentication no", "ChallengeResponseAuthentication no"];
    let needs_cleanup = content.lines().any(|l| aggressive_lines.contains(&l.trim_start()));

    // Skip when desired directives are active and no legacy lines remain.
    let all_set = settings.iter().all(|(k, v)| {
        let expected = format!("{} {}", k, v);
        content.lines().any(|l| l.trim_start() == expected)
    });
    if all_set && !needs_cleanup {
        return Ok(false);
    }

    emitter.begin_phase("ssh", "SSH hardening");
    emitter.progress("Hardening SSH...");

    // Remove legacy lockout directives before applying current policy.
    let mut lines: Vec<String> = content
        .lines()
        .filter(|l| !aggressive_lines.contains(&l.trim_start()))
        .map(String::from)
        .collect();

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

/// Configures Samba with volatile runtime paths and persistent state on `/mutable`.
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

        // Redirect writable paths before package scripts can start Samba.
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

        // Persist Samba state across read-only-root reboots.
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

        // `smbpasswd` requires a running daemon for initial registration.
        let _ = sentryusb_shell::run("service", &["smbd", "start"]).await;
        set_default_samba_password().await;
        let _ = sentryusb_shell::run("service", &["smbd", "stop"]).await;

        emitter.progress("Samba installed.");
    }

    // Remove the obsolete writable-state mount.
    sed_delete_line_matching("/etc/fstab", |l| {
        l == "tmpfs /mnt/smbexport tmpfs nodev,nosuid 0 0"
    })?;

    // Migrate link state to the mutable partition.
    if !Path::new("/mutable/TeslaCam").is_dir() && Path::new("/backingfiles/TeslaCam").is_dir() {
        emitter.progress("Moving TeslaCam symlink folder from backingfiles to mutable");
        let _ = sentryusb_shell::run(
            "mv", &["/backingfiles/TeslaCam", "/mutable/TeslaCam"],
        ).await;
    }

    // Keep installed Samba behavior aligned with the current template.
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

/// Registers the configured default Samba credential for `pi`.
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

/// Removes matching lines from a file.
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

/// Installs the archiveloop systemd service.
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

/// Installs missing tools, checking binaries because Debian package ownership varies.
pub async fn install_required_packages(emitter: &SetupEmitter) -> Result<bool> {
    // Map runtime binaries to packages; ntpdig sets time and nc probes archives.
    let packages: &[(&str, &str)] = &[
        ("dos2unix", "dos2unix"),
        ("parted", "parted"),
        ("fdisk", "fdisk"),
        ("curl", "curl"),
        ("rsync", "rsync"),
        ("jq", "jq"),
        ("ntpdig", "ntpsec-ntpdig"),
        ("nc", "netcat-openbsd"),
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
    crate::apt::apt_install(
        |m| emitter.progress(m),
        &to_install,
        Duration::from_secs(300),
    ).await.context("failed to install required packages")?;

    Ok(true)
}

/// Sets the system timezone and keeps `/etc/timezone` aligned with `/etc/localtime`.
pub async fn configure_timezone(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let raw = match env.config.get("TIME_ZONE") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => return Ok(false),
    };

    // `auto` must resolve to an IANA zone before `timedatectl`; UTC can
    // misalign local-time clip names with epoch-based telemetry.
    let tz = if raw.eq_ignore_ascii_case("auto") {
        match resolve_timezone_via_geoip().await {
            Some(z) => {
                emitter.progress(&format!("Auto-detected timezone: {}", z));
                z
            }
            None => {
                emitter.progress(
                    "Timezone 'auto' could not be geolocated — leaving system default. \
                     You can set it later in Settings.",
                );
                return Ok(false);
            }
        }
    } else {
        // Normalize aliases absent from minimal modern tzdata packages.
        normalize_timezone(&raw)
    };

    if current_timezone().as_deref() == Some(tz.as_str()) {
        return Ok(false);
    }

    emitter.progress(&format!("Setting timezone to {}", tz));
    // Invalid zones must not create a setup retry loop.
    if let Err(e) = sentryusb_shell::run("timedatectl", &["set-timezone", &tz]).await {
        emitter.progress(&format!(
            "Could not set timezone to {} ({}); leaving system default",
            tz, e
        ));
        return Ok(false);
    }

    // Maintain the legacy text file for consumers that do not read the symlink.
    let _ = std::fs::write("/etc/timezone", format!("{}\n", tz));

    Ok(true)
}

/// Resolves `auto` to an IANA timezone through the first-party geo-IP endpoint.
pub(crate) async fn resolve_timezone_via_geoip() -> Option<String> {
    // Do not fall back to third-party geo-IP services; a later boot can retry.
    const ENDPOINTS: &[&str] = &["https://sentryusb.com/api/geoip/me"];
    for url in ENDPOINTS {
        if let Ok(out) = sentryusb_shell::run("curl", &["-s", "--max-time", "5", url]).await {
            if let Some(tz) = extract_timezone(&out) {
                return Some(tz);
            }
        }
    }
    None
}

/// Extracts an IANA zone from supported flat, nested, or bare response shapes.
fn extract_timezone(body: &str) -> Option<String> {
    let t = body.trim();
    if t.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(t).ok()?;
        // Accept current and compatible legacy field shapes.
        let candidates = [
            v.get("timeZone").and_then(|x| x.as_str()), // sentryusb.com/api/geoip/me
            v.get("timezone").and_then(|x| x.as_str()),
            v.get("time_zone").and_then(|x| x.as_str()),
            v.get("tz").and_then(|x| x.as_str()),
            v.pointer("/location/time_zone").and_then(|x| x.as_str()),
            v.pointer("/location/timezone").and_then(|x| x.as_str()),
        ];
        for z in candidates.into_iter().flatten() {
            let z = z.trim();
            if is_valid_iana_zone(z) {
                return Some(z.to_string());
            }
        }
        return None;
    }
    if is_valid_iana_zone(t) {
        Some(t.to_string())
    } else {
        None
    }
}

/// Rejects values that do not resemble an `Area/Location` IANA zone.
pub(crate) fn is_valid_iana_zone(tz: &str) -> bool {
    !tz.is_empty()
        && tz.len() < 64
        && tz.contains('/')
        && !tz.chars().any(char::is_whitespace)
        && tz.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

/// Non-blocking boot retry for unresolved `TIME_ZONE=auto` installations.
pub async fn ensure_timezone_resolved() {
    let path = sentryusb_config::find_config_path();
    let Ok((active, commented)) = sentryusb_config::parse_file(path) else {
        return;
    };
    let raw = sentryusb_config::get_config_value(&active, &commented, "TIME_ZONE");
    if !matches!(raw.as_deref(), Some(v) if v.eq_ignore_ascii_case("auto")) {
        return;
    }
    // Never override an already resolved non-UTC zone.
    match current_timezone().as_deref() {
        Some("UTC") | Some("Etc/UTC") | None => {}
        Some(_) => return,
    }
    let Some(tz) = resolve_timezone_via_geoip().await else {
        return; // A later boot retries.
    };
    if current_timezone().as_deref() == Some(tz.as_str()) {
        return;
    }
    // Remount only for the persistent timezone write.
    let _ = sentryusb_shell::run(
        "sh",
        &[
            "-c",
            "/root/bin/remountfs_rw 2>/dev/null || mount -o remount,rw / 2>/dev/null || true",
        ],
    )
    .await;
    if sentryusb_shell::run("timedatectl", &["set-timezone", &tz])
        .await
        .is_ok()
    {
        let _ = std::fs::write("/etc/timezone", format!("{}\n", tz));
        tracing::info!("timezone (auto) resolved on boot: {}", tz);
    }
    let _ = sentryusb_shell::run("sh", &["-c", "mount -o remount,ro / 2>/dev/null || true"]).await;
}

/// Translates legacy tzdata aliases to canonical IANA names.
fn normalize_timezone(tz: &str) -> String {
    match tz {
        // US aliases.
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
        // Single-name aliases.
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
mod set_ini_key_tests {
    use super::set_ini_key;

    const STOCK: &str = "[server]\n#use-ipv6=yes\nuse-ipv4=yes\n\n[publish]\n#publish-aaaa-on-ipv4=yes\n";

    #[test]
    fn uncomments_stock_defaults_in_place() {
        let (out, changed) = set_ini_key(STOCK, "server", "use-ipv6", "no");
        assert!(changed);
        assert_eq!(out, "[server]\nuse-ipv6=no\nuse-ipv4=yes\n\n[publish]\n#publish-aaaa-on-ipv4=yes\n");
    }

    #[test]
    fn idempotent_when_already_set() {
        let (once, _) = set_ini_key(STOCK, "publish", "publish-aaaa-on-ipv4", "no");
        let (twice, changed) = set_ini_key(&once, "publish", "publish-aaaa-on-ipv4", "no");
        assert!(!changed);
        assert_eq!(once, twice);
    }

    #[test]
    fn only_touches_key_in_target_section() {
        let conf = "[publish]\n#use-ipv6=yes\n";
        let (out, changed) = set_ini_key(conf, "server", "use-ipv6", "no");
        assert!(changed);
        assert_eq!(out, "[publish]\n#use-ipv6=yes\n[server]\nuse-ipv6=no\n");
    }

    #[test]
    fn inserts_key_when_section_exists_without_it() {
        let conf = "[server]\nuse-ipv4=yes\n\n[wide-area]\nenable-wide-area=yes\n";
        let (out, changed) = set_ini_key(conf, "server", "use-ipv6", "no");
        assert!(changed);
        assert_eq!(out, "[server]\nuse-ipv4=yes\nuse-ipv6=no\n\n[wide-area]\nenable-wide-area=yes\n");
    }

    #[test]
    fn appends_missing_section_at_end() {
        let conf = "[server]\nuse-ipv4=yes\n";
        let (out, changed) = set_ini_key(conf, "publish", "publish-aaaa-on-ipv4", "no");
        assert!(changed);
        assert_eq!(out, "[server]\nuse-ipv4=yes\n[publish]\npublish-aaaa-on-ipv4=no\n");
    }

    #[test]
    fn dedupes_conflicting_assignments_in_section() {
        let conf = "[server]\nuse-ipv6=yes\n# use-ipv6=no\nuse-ipv6=yes\n";
        let (out, changed) = set_ini_key(conf, "server", "use-ipv6", "no");
        assert!(changed);
        assert_eq!(out, "[server]\nuse-ipv6=no\n");
    }

    #[test]
    fn does_not_match_prefixed_keys() {
        let conf = "[server]\nuse-ipv6-foo=yes\n";
        let (out, changed) = set_ini_key(conf, "server", "use-ipv6", "no");
        assert!(changed);
        assert_eq!(out, "[server]\nuse-ipv6-foo=yes\nuse-ipv6=no\n");
    }
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

#[cfg(test)]
mod timezone_extract_tests {
    use super::extract_timezone;

    #[test]
    fn parses_first_party_geoip_json() {
        let body = r#"{"ip":"203.0.113.7","country":"US","region":"CA","city":"Mountain View","timeZone":"America/Los_Angeles"}"#;
        assert_eq!(extract_timezone(body).as_deref(), Some("America/Los_Angeles"));
    }

    #[test]
    fn parses_bare_text_fallback() {
        assert_eq!(extract_timezone("America/New_York\n").as_deref(), Some("America/New_York"));
    }

    #[test]
    fn parses_nested_geolite2_shape() {
        let body = r#"{"location":{"time_zone":"Europe/Berlin"}}"#;
        assert_eq!(extract_timezone(body).as_deref(), Some("Europe/Berlin"));
    }

    #[test]
    fn rejects_rate_limited_and_garbage() {
        assert_eq!(extract_timezone(r#"{"error":"rate_limited","message":"Too many requests."}"#), None);
        assert_eq!(extract_timezone("<!doctype html>"), None);
        assert_eq!(extract_timezone(""), None);
        assert_eq!(extract_timezone("auto"), None);
    }
}

/// Reads `/etc/timezone`, falling back to the `/etc/localtime` symlink.
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

/// Configures the Pi 5 RTC or an external DS3231 on other models.
pub async fn configure_rtc(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    if env.pi_model == crate::env::PiModel::Pi5 {
        return configure_rtc_pi5(env, emitter).await;
    }
    configure_rtc_ds3231(env, emitter).await
}

/// Configures the Pi 5 built-in RTC.
async fn configure_rtc_pi5(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let config_path = match &env.piconfig_path {
        Some(p) => p.clone(),
        None => return Ok(false),
    };

    let enabled = env.get_bool("RTC_BATTERY_ENABLED", false);
    let trickle = env.get_bool("RTC_TRICKLE_CHARGE", false);

    // Skip when service and trickle-charge state already match.
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

        // Do not let fake-hwclock compete with the hardware RTC.
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

        // Seed the RTC before any setup-triggered reboot.
        rtc_sync_systohc(emitter).await;

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

        // Restore fake-hwclock when hardware RTC support is disabled.
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

/// Configures an external DS3231 I²C RTC.
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
            // Normalize existing trickle voltage.
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

/// Writes system time through the RTC ioctl when minimal images lack `hwclock`.
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
