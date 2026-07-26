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

    // Avahi keeps advertising the old name until restarted, leaving
    // `<hostname>.local` dead while the IP works (the bash-era
    // configure_hostname restarted it too).
    if sentryusb_shell::run("systemctl", &["-q", "is-active", "avahi-daemon"]).await.is_ok() {
        let _ = sentryusb_shell::run("systemctl", &["restart", "avahi-daemon"]).await;
    }
    Ok(true)
}

const AVAHI_DAEMON_CONF: &str = "/etc/avahi/avahi-daemon.conf";

/// Daemon settings forcing IPv4-only mDNS advertising. Windows/Chrome
/// prefer the AAAA answer for .local names; the Pi's global SLAAC address
/// rotates (privacy extensions) so the advertised AAAA goes stale (slow
/// loads), and Chrome classifies global IPv6 as *public* address space, so
/// the plain-http Web UI reached through it hits Private Network Access
/// blocks that surface as CORS errors. A-record-only sidesteps both;
/// kernel/socket IPv6 on the device is untouched.
/// Mirrored for the shell paths in setup/pi/avahi-ipv4-only.sh.
const AVAHI_IPV4_ONLY: &[(&str, &str, &str)] = &[
    ("server", "use-ipv6", "no"),
    ("publish", "publish-aaaa-on-ipv4", "no"),
];

/// Set `key=value` inside `[section]` of INI-style content. Section-aware:
/// replaces the first active or commented assignment of the key within that
/// section (dropping repeats), inserts after the section header when absent,
/// appends the section itself when missing. Assignments of the same key in
/// *other* sections are left alone — avahi can refuse to start on a key
/// planted in the wrong group. Returns (new_content, changed).
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
                // Insert before the section's trailing blank lines, not
                // between them and the next header.
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

/// Compute the IPv4-only rewrite of avahi-daemon.conf, or `None` when the
/// file is already correct. A missing conf is rebuilt from scratch (it's
/// abnormal once avahi is installed, and before installation the caller
/// recomputes after apt lays the real one down); an *unreadable* one is
/// left alone — clobbering a conf we couldn't inspect is worse than
/// advertising AAAA for one more release.
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

/// Set up Avahi mDNS service for local network discovery.
///
/// Idempotent: if the service file is already present and matches, and the
/// daemon config already advertises IPv4-only, do nothing and return
/// `false` so the caller can skip announcing this phase.
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

    // Also verify the daemon is actually enabled and running — a masked or
    // dead avahi with correct config files means .local is silently broken,
    // and returning early here would never repair it. Falling through
    // reaches the enable+restart at the end.
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
        // The package install just laid down avahi-daemon.conf — recompute
        // from the real file, not whatever preceded it.
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
        // Write-then-rename so a power loss mid-write can't leave a
        // truncated conf that stops avahi from starting.
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

/// Harden SSH configuration. Idempotent — silent when no changes are needed.
pub async fn configure_ssh(emitter: &SetupEmitter) -> Result<bool> {
    let sshd_config = Path::new("/etc/ssh/sshd_config");
    if !sshd_config.exists() {
        info!("sshd_config not found, skipping SSH hardening");
        return Ok(false);
    }

    let content = std::fs::read_to_string(sshd_config)?;
    // Don't disable password auth automatically. Locking the user out
    // of SSH on a fresh install — when they may not have copied a
    // public key into the wizard yet — is hostile. Pi OS already
    // defaults to PermitRootLogin=prohibit-password (root only via
    // key); we re-assert that, leave the user's normal-account
    // password auth alone, and let them harden further from Settings
    // if they want to.
    let settings = [
        ("PermitRootLogin", "prohibit-password"),
        ("UsePAM", "yes"),
    ];

    // Earlier setup runs wrote `PasswordAuthentication no` and
    // `ChallengeResponseAuthentication no`, which locked out anyone
    // who hadn't placed a public key in their authorized_keys before
    // running the wizard. If those exact lines are still present from
    // a prior install, drop them so the OS default (password auth on)
    // is restored on the next sshd reload. We only touch lines that
    // exactly match what the previous setup wrote — anything the user
    // edited by hand stays untouched.
    let aggressive_lines = ["PasswordAuthentication no", "ChallengeResponseAuthentication no"];
    let needs_cleanup = content.lines().any(|l| aggressive_lines.contains(&l.trim_start()));

    // Quick idempotency check — if every setting already has an active line
    // with the desired value, AND no leftover aggressive lines need
    // removing, there's nothing to do.
    let all_set = settings.iter().all(|(k, v)| {
        let expected = format!("{} {}", k, v);
        content.lines().any(|l| l.trim_start() == expected)
    });
    if all_set && !needs_cleanup {
        return Ok(false);
    }

    emitter.begin_phase("ssh", "SSH hardening");
    emitter.progress("Hardening SSH...");

    // Drop any leftover aggressive lines first.
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

/// Configure Samba shares if enabled.
///
/// Critical bits:
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

/// Install the archive loop systemd service (runs the bash archiveloop
/// script).
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
    //
    // `netcat-openbsd` provides the `nc` binary used by
    // `run/{nfs,cifs}_archive/archive-is-reachable.sh` (`nc -z -w 5
    // $HOST 2049|445`). The official Pi OS image preinstalls it, but
    // alternative images (DietPi, Radxa Debian, etc.) don't, so without
    // this every archive cycle exits 127 from the reachability probe
    // and archiveloop is permanently stuck on "Waiting for archive to
    // be reachable..." — clips and music never sync. The bash flow
    // installed it inside `verify-and-configure-archive.sh`, which is
    // no longer executed in the Rust port.
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

    // The setup wizard ships "auto" as the default timezone, but "auto" is
    // NOT a valid IANA zone — `timedatectl set-timezone auto` fails with
    // "Invalid or not installed time zone". Left unhandled that either
    // loops the setup phase (the error propagates and the phase retries
    // forever) or silently leaves the Pi on UTC. A UTC Pi then mis-links
    // drive telemetry: Tesla clip filenames are in the car's LOCAL clock
    // but `telemetry_samples` are UTC epoch, so the odometer/battery/temps
    // join pulls from the wrong (tz-offset) window. Resolve "auto" to a
    // real zone via IP geolocation; fall back gracefully if that fails.
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
        // Newer Pi OS / Debian images (bookworm and later) ship only the
        // canonical IANA tzdata zones and drop the legacy `US/*` and
        // single-name aliases that older images still carried. Configs
        // saved with one of those shortcuts then fail timedatectl with
        // "Invalid or not installed time zone". Map them up front so we
        // hand timedatectl a name every shipped tzdata version recognizes.
        normalize_timezone(&raw)
    };

    if current_timezone().as_deref() == Some(tz.as_str()) {
        return Ok(false);
    }

    emitter.progress(&format!("Setting timezone to {}", tz));
    // Non-fatal: an unknown/invalid zone must never loop the setup phase.
    if let Err(e) = sentryusb_shell::run("timedatectl", &["set-timezone", &tz]).await {
        emitter.progress(&format!(
            "Could not set timezone to {} ({}); leaving system default",
            tz, e
        ));
        return Ok(false);
    }

    // Keep /etc/timezone in sync with the symlink. On images where the
    // file is missing this also creates it, which makes our own
    // idempotency check cheap on the next resume.
    let _ = std::fs::write("/etc/timezone", format!("{}\n", tz));

    Ok(true)
}

/// Best-effort IANA timezone via IP geolocation, used when the wizard
/// leaves the timezone as "auto". The setup phase already requires
/// network, so we shell `curl`. Tries several free, no-key providers in
/// turn so one being down or rate-limited doesn't break timezone setup;
/// each returns the bare zone name (e.g. "America/New_York"). Returns
/// `None` only when ALL providers fail, so the caller can fall back.
pub(crate) async fn resolve_timezone_via_geoip() -> Option<String> {
    // First-party ONLY: SentryUSB's own server geolocates the caller's IP
    // (MaxMind GeoLite2) and returns it as JSON, so IP processing stays
    // under the SentryUSB privacy policy (sentryusb.com/legal/privacy) with
    // NO third party. We deliberately do NOT fall back to public geo-IP
    // services — if this is unreachable / rate-limited we leave the system
    // default (UTC) and the boot-time retry resolves it on a later boot.
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

/// Pull an IANA zone from the first-party geo-IP response. The
/// `sentryusb.com/api/geoip/me` endpoint returns JSON with a `timeZone`
/// field (e.g. `{"timeZone":"America/New_York", ...}`); we also accept
/// `timezone`/`time_zone`, nested `location.time_zone`, and a bare-text
/// body — defensively, so a future response-shape tweak won't break it.
fn extract_timezone(body: &str) -> Option<String> {
    let t = body.trim();
    if t.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(t).ok()?;
        // Flat key (preferred: `{"timezone":"America/New_York", ...}`)
        // or GeoLite2's native nested `{"location":{"time_zone":"..."}}`.
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

/// Cheap sanity check that a string looks like an IANA zone
/// ("Area/Location") and not an error page / empty body.
pub(crate) fn is_valid_iana_zone(tz: &str) -> bool {
    !tz.is_empty()
        && tz.len() < 64
        && tz.contains('/')
        && !tz.chars().any(char::is_whitespace)
        && tz.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+'))
}

/// Boot-time safety net for `TIME_ZONE=auto`. If setup couldn't reach a
/// geo-IP provider (e.g. the network wasn't up yet during setup), the Pi
/// is left on UTC and drive telemetry mis-links — clip filenames are in
/// the car's local clock but `telemetry_samples` are UTC epoch.
///
/// The main service spawns this NON-BLOCKING on startup, so it never
/// delays boot. It re-resolves once and sets the zone only if we're still
/// on UTC; it's a cheap no-op when `TIME_ZONE` isn't "auto" or the zone is
/// already a real (non-UTC) one. If geolocation still fails (no network
/// yet) it simply leaves UTC and a later boot tries again.
pub async fn ensure_timezone_resolved() {
    let path = sentryusb_config::find_config_path();
    let Ok((active, commented)) = sentryusb_config::parse_file(path) else {
        return;
    };
    // Only act when the user left the wizard on "auto".
    let raw = sentryusb_config::get_config_value(&active, &commented, "TIME_ZONE");
    if !matches!(raw.as_deref(), Some(v) if v.eq_ignore_ascii_case("auto")) {
        return;
    }
    // Already on a real zone? Nothing to do (don't override a resolved tz).
    match current_timezone().as_deref() {
        Some("UTC") | Some("Etc/UTC") | None => {}
        Some(_) => return,
    }
    let Some(tz) = resolve_timezone_via_geoip().await else {
        return; // still no network — a later boot will retry
    };
    if current_timezone().as_deref() == Some(tz.as_str()) {
        return;
    }
    // RO root → flip to rw just for the write, then back.
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
        // A use-ipv6 line under [publish] must be left alone — rewriting it
        // there could produce a conf avahi refuses to load.
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
        // "use-ipv6" must not swallow a hypothetical "use-ipv6-foo=..." line.
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
        // Shape of sentryusb.com/api/geoip/me (camelCase `timeZone`).
        // Placeholder values — RFC 5737 documentation IP, no real location.
        let body = r#"{"ip":"203.0.113.7","country":"US","region":"CA","city":"Mountain View","timeZone":"America/Los_Angeles"}"#;
        assert_eq!(extract_timezone(body).as_deref(), Some("America/Los_Angeles"));
    }

    #[test]
    fn parses_bare_text_fallback() {
        // ipinfo.io/timezone / ipapi.co/timezone style.
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

/// Pi 5 built-in RTC.
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
