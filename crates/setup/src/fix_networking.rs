//! Networking repair for old broken installs — port of
//! `fix-readonly-networking.sh`.
//!
//! Older SentryUSB setups symlinked `/var/lib/NetworkManager`, the NM
//! connection-profile directory, `/var/lib/dhcp*`, and `/etc/resolv.conf`
//! to paths under `/mutable`. That pattern broke whenever `/mutable` (on
//! the USB drive) wasn't mounted in time at boot — NM thrashed, DNS
//! dangled, pairings disappeared. This module detects that state and
//! converts it to the current tmpfs-based layout without reflashing.
//!
//! Safe to run on already-correct systems — the first block short-circuits
//! when no repair is needed.

use std::path::Path;

use anyhow::Result;

use crate::SetupEmitter;

/// Run the networking fix. Returns `true` if a repair was actually applied.
pub async fn fix_readonly_networking(emitter: &SetupEmitter) -> Result<bool> {
    if !am_root() {
        anyhow::bail!("fix_readonly_networking must run as root");
    }

    if !needs_fix().await {
        emitter.progress(
            "fix-readonly-networking: no fix needed — networking already \
             uses tmpfs / root (not symlinks to /mutable).",
        );
        return Ok(false);
    }

    emitter.begin_phase("fix_networking", "Repair read-only networking");
    emitter.progress("Applying networking fix for read-only root...");

    // Ensure /mutable is mounted so we can copy from it if needed.
    if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"])
        .await
        .is_err()
    {
        let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
        if fstab.contains("LABEL=mutable") {
            if sentryusb_shell::run("mount", &["/mutable"]).await.is_err() {
                emitter.progress(
                    "Warning: could not mount /mutable, will create empty dirs where needed",
                );
            }
        }
    }

    // /var/lib/NetworkManager: must be a real dir so tmpfs can mount over it.
    if Path::new("/var/lib/NetworkManager").is_symlink() {
        emitter.progress("Replacing /var/lib/NetworkManager symlink with directory");
        let _ = std::fs::remove_file("/var/lib/NetworkManager");
        let _ = std::fs::create_dir_all("/var/lib/NetworkManager");
    }

    // NM connection profiles: restore to root so they exist before /mutable mounts.
    if Path::new("/etc/NetworkManager/system-connections").is_symlink() {
        emitter.progress("Restoring NetworkManager connection profiles to root FS");
        let _ = std::fs::remove_file("/etc/NetworkManager/system-connections");
        if Path::new("/mutable/etc/NetworkManager/system-connections").is_dir() {
            let _ = sentryusb_shell::run(
                "cp",
                &[
                    "-a",
                    "/mutable/etc/NetworkManager/system-connections",
                    "/etc/NetworkManager/",
                ],
            ).await;
        } else {
            let _ = std::fs::create_dir_all("/etc/NetworkManager/system-connections");
        }
    }

    // DHCP lease dirs: real dirs for tmpfs.
    for d in &["/var/lib/dhcp", "/var/lib/dhcpcd"] {
        if Path::new(d).is_symlink() {
            emitter.progress(&format!("Replacing {} symlink with directory", d));
            let _ = std::fs::remove_file(d);
            let _ = std::fs::create_dir_all(d);
        }
    }

    // resolv.conf → /tmp (always writable). Also redirect away from
    // systemd-resolved's stub (we configure dns=none below and use a
    // dispatcher to populate resolv.conf directly).
    let resolv_target = std::fs::read_link("/etc/resolv.conf")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if resolv_target != "/tmp/resolv.conf" {
        emitter.progress(&format!(
            "Redirecting resolv.conf to /tmp (was: {})",
            if resolv_target.is_empty() { "empty" } else { &resolv_target }
        ));
        seed_tmp_resolv(&resolv_target).await;
        let _ = std::fs::remove_file("/etc/resolv.conf");
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink("/tmp/resolv.conf", "/etc/resolv.conf");
    }

    // tmpfiles.d rule to seed /tmp/resolv.conf on every boot.
    emitter.progress("Installing tmpfiles.d rule for resolv.conf");
    let _ = std::fs::create_dir_all("/etc/tmpfiles.d");
    std::fs::write(
        "/etc/tmpfiles.d/resolv-fallback.conf",
        "f /tmp/resolv.conf 0644 root root - nameserver 1.1.1.1\n",
    )?;

    // DHCP client hooks — whichever clients are present.
    install_nm_dns_config(emitter).await?;
    install_dhcpcd_hook(emitter).await?;
    install_dhclient_hook(emitter).await?;

    // Disable systemd-resolved (conflicts with our resolv.conf handling).
    if sentryusb_shell::run("systemctl", &["is-active", "--quiet", "systemd-resolved"])
        .await
        .is_ok()
    {
        emitter.progress("Disabling systemd-resolved (dispatcher handles DNS directly)");
        let _ = sentryusb_shell::run("systemctl", &["stop", "systemd-resolved"]).await;
        let _ = sentryusb_shell::run("systemctl", &["disable", "systemd-resolved"]).await;
    }

    // Unblock bluetooth + install boot service if missing.
    let _ = sentryusb_shell::run("rfkill", &["unblock", "bluetooth"]).await;
    if !Path::new("/etc/systemd/system/rfkill-unblock-bluetooth.service").exists() {
        emitter.progress("Installing Bluetooth rfkill-unblock boot service");
        std::fs::write(
            "/etc/systemd/system/rfkill-unblock-bluetooth.service",
            BT_UNBLOCK_SERVICE,
        )?;
        let _ = sentryusb_shell::run(
            "systemctl", &["enable", "rfkill-unblock-bluetooth.service"],
        ).await;
    }

    // Reload NM without restarting — reloads dns=none + dispatcher config
    // without dropping WiFi or killing SSH sessions.
    if sentryusb_shell::run("systemctl", &["is-active", "--quiet", "NetworkManager"])
        .await
        .is_ok()
    {
        emitter.progress("Reloading NetworkManager configuration");
        let _ = sentryusb_shell::run("nmcli", &["general", "reload"]).await;
    }

    // fstab: tmpfs entries + mutable/backingfiles nofail.
    update_fstab(emitter)?;

    // Suppress the mount warning about /etc/fstab being newer than the
    // systemd state.
    let _ = sentryusb_shell::run("touch", &["-t", "197001010000", "/etc/fstab"]).await;

    emitter.progress("Done. Reboot for changes to take effect.");
    Ok(true)
}

async fn needs_fix() -> bool {
    if Path::new("/var/lib/NetworkManager").is_symlink() {
        return true;
    }
    if Path::new("/etc/NetworkManager/system-connections").is_symlink() {
        return true;
    }
    if Path::new("/var/lib/dhcp").is_symlink() || Path::new("/var/lib/dhcpcd").is_symlink() {
        return true;
    }
    let resolv_target = std::fs::read_link("/etc/resolv.conf")
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if resolv_target.contains("/mutable") || resolv_target.contains("/run/systemd/resolve") {
        return true;
    }
    if sentryusb_shell::run("systemctl", &["is-active", "--quiet", "systemd-resolved"])
        .await
        .is_ok()
    {
        return true;
    }
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    if !fstab_has_word(&fstab, "/var/lib/NetworkManager") {
        return true;
    }
    if fstab.contains("LABEL=mutable") && !line_has_nofail(&fstab, "LABEL=mutable") {
        return true;
    }
    if fstab.contains("LABEL=backingfiles") && !line_has_nofail(&fstab, "LABEL=backingfiles") {
        return true;
    }
    if !Path::new("/etc/tmpfiles.d/resolv-fallback.conf").exists() {
        return true;
    }
    if !fstab_has_word(&fstab, "/var/lib/systemd/rfkill") {
        return true;
    }
    false
}

async fn seed_tmp_resolv(existing_target: &str) {
    let _ = std::fs::write("/tmp/resolv.conf", "");
    if sentryusb_shell::run("nmcli", &["--version"]).await.is_ok() {
        let cmd = "nmcli --terse --fields IP4.DNS dev show 2>/dev/null | \
                   sed -n 's/^IP4\\.DNS\\[.*\\]:/nameserver /p' | head -3 \
                   >> /tmp/resolv.conf";
        let _ = sentryusb_shell::run("bash", &["-c", cmd]).await;
    }
    let has_ns = std::fs::read_to_string("/tmp/resolv.conf")
        .map(|c| c.lines().any(|l| l.starts_with("nameserver")))
        .unwrap_or(false);
    if !has_ns && !existing_target.is_empty() {
        if let Ok(c) = std::fs::read_to_string(existing_target) {
            let ns_lines: String = c
                .lines()
                .filter(|l| l.starts_with("nameserver"))
                .collect::<Vec<_>>()
                .join("\n");
            if !ns_lines.is_empty() {
                let _ = std::fs::write("/tmp/resolv.conf", format!("{}\n", ns_lines));
            }
        }
    }
    let has_ns = std::fs::read_to_string("/tmp/resolv.conf")
        .map(|c| c.lines().any(|l| l.starts_with("nameserver")))
        .unwrap_or(false);
    if !has_ns {
        let _ = std::fs::write("/tmp/resolv.conf", "nameserver 1.1.1.1\n");
    }
}

async fn install_nm_dns_config(emitter: &SetupEmitter) -> Result<()> {
    if sentryusb_shell::run("nmcli", &["--version"]).await.is_err() {
        return Ok(());
    }
    emitter.progress("Configuring NetworkManager DNS handling (dns=none + dispatcher)");
    std::fs::create_dir_all("/etc/NetworkManager/conf.d")?;
    std::fs::write(
        "/etc/NetworkManager/conf.d/sentryusb-dns.conf",
        "[main]\ndns=none\n",
    )?;
    std::fs::create_dir_all("/etc/NetworkManager/dispatcher.d")?;
    std::fs::write(
        "/etc/NetworkManager/dispatcher.d/50-write-resolv-conf",
        NM_DISPATCHER,
    )?;
    let _ = sentryusb_shell::run(
        "chmod", &["0755", "/etc/NetworkManager/dispatcher.d/50-write-resolv-conf"],
    ).await;
    Ok(())
}

async fn install_dhcpcd_hook(emitter: &SetupEmitter) -> Result<()> {
    if sentryusb_shell::run("dhcpcd", &["--version"]).await.is_err() {
        return Ok(());
    }
    emitter.progress("Installing dhcpcd hook for resolv.conf");
    std::fs::create_dir_all("/lib/dhcpcd/dhcpcd-hooks")?;
    std::fs::write("/lib/dhcpcd/dhcpcd-hooks/90-sentryusb-resolv", DHCPCD_HOOK)?;
    let _ = sentryusb_shell::run(
        "chmod", &["0644", "/lib/dhcpcd/dhcpcd-hooks/90-sentryusb-resolv"],
    ).await;
    Ok(())
}

async fn install_dhclient_hook(emitter: &SetupEmitter) -> Result<()> {
    if !Path::new("/etc/network").exists() {
        return Ok(());
    }
    if sentryusb_shell::run("nmcli", &["--version"]).await.is_ok() {
        return Ok(());
    }
    if sentryusb_shell::run("dhcpcd", &["--version"]).await.is_ok() {
        return Ok(());
    }
    emitter.progress("Installing ifupdown hook for resolv.conf");
    std::fs::create_dir_all("/etc/dhcp/dhclient-exit-hooks.d")?;
    std::fs::write("/etc/dhcp/dhclient-exit-hooks.d/sentryusb-resolv", DHCLIENT_HOOK)?;
    let _ = sentryusb_shell::run(
        "chmod", &["0755", "/etc/dhcp/dhclient-exit-hooks.d/sentryusb-resolv"],
    ).await;
    Ok(())
}

fn update_fstab(emitter: &SetupEmitter) -> Result<()> {
    let mut fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let mut changed = false;

    let adds: &[(&str, &str, &str)] = &[
        (
            "/var/lib/NetworkManager",
            "tmpfs /var/lib/NetworkManager tmpfs nodev,nosuid,mode=0700 0 0",
            "/var/lib/NetworkManager",
        ),
        (
            "/var/lib/dhcp",
            "tmpfs /var/lib/dhcp tmpfs nodev,nosuid 0 0",
            "/var/lib/dhcp",
        ),
        (
            "/var/lib/dhcpcd",
            "tmpfs /var/lib/dhcpcd tmpfs nodev,nosuid 0 0",
            "/var/lib/dhcpcd",
        ),
        (
            "/var/lib/systemd/rfkill",
            "tmpfs /var/lib/systemd/rfkill tmpfs nodev,nosuid 0 0",
            "/var/lib/systemd/rfkill",
        ),
    ];

    for (word, entry, mkdir) in adds {
        if fstab_has_word(&fstab, word) {
            continue;
        }
        emitter.progress(&format!("Adding tmpfs fstab entry for {}", word));
        let _ = std::fs::create_dir_all(mkdir);
        if !fstab.is_empty() && !fstab.ends_with('\n') {
            fstab.push('\n');
        }
        fstab.push_str(entry);
        fstab.push('\n');
        changed = true;
    }

    for label in &["mutable", "backingfiles"] {
        if !fstab.contains(&format!("LABEL={}", label)) {
            continue;
        }
        if line_has_nofail(&fstab, &format!("LABEL={}", label)) {
            continue;
        }
        emitter.progress(&format!("Adding nofail to LABEL={} in fstab", label));
        fstab = fstab
            .lines()
            .map(|l| {
                if !l.contains(&format!("LABEL={}", label)) {
                    return l.to_string();
                }
                let mut out = l.to_string();
                out = out.replace("auto,rw,noatime", "auto,rw,noatime,nofail");
                // Second pass catches the shorter form if the first didn't fire.
                if !out.contains("nofail") {
                    out = out.replace("auto,rw", "auto,rw,nofail");
                }
                out
            })
            .collect::<Vec<_>>()
            .join("\n");
        if !fstab.ends_with('\n') {
            fstab.push('\n');
        }
        changed = true;
    }

    if changed {
        std::fs::write("/etc/fstab", fstab)?;
    }
    Ok(())
}

fn fstab_has_word(fstab: &str, needle: &str) -> bool {
    fstab.lines().any(|line| {
        if line.trim_start().starts_with('#') {
            return false;
        }
        line.split_whitespace().any(|w| w == needle)
    })
}

fn line_has_nofail(fstab: &str, needle: &str) -> bool {
    fstab.lines().any(|l| l.contains(needle) && l.contains("nofail"))
}

fn am_root() -> bool {
    #[cfg(target_os = "linux")]
    {
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

// -------------------- embedded resources --------------------

const NM_DISPATCHER: &str = r#"#!/bin/bash
# Populate /tmp/resolv.conf with DHCP-provided DNS servers.
case "$2" in
  up|dhcp4-change)
    _servers="${DHCP4_DOMAIN_NAME_SERVERS:-${IP4_NAMESERVERS:-}}"
    if [ -n "$_servers" ]; then
      {
        for _ns in $_servers; do
          echo "nameserver $_ns"
        done
        _domain="${DHCP4_DOMAIN_NAME:-}"
        [ -n "$_domain" ] && echo "search $_domain"
      } > /tmp/resolv.conf
    fi
    ;;
esac
"#;

const DHCPCD_HOOK: &str = r#"# Write DHCP-provided DNS servers to /tmp/resolv.conf.
# /etc/resolv.conf is a symlink to /tmp/resolv.conf on SentryUSB.
if [ -n "${new_domain_name_servers:-}" ]; then
  {
    for ns in $new_domain_name_servers; do
      echo "nameserver $ns"
    done
    [ -n "${new_domain_name:-}" ] && echo "search $new_domain_name"
  } > /tmp/resolv.conf
fi
"#;

const DHCLIENT_HOOK: &str = r#"# Write DHCP-provided DNS to /tmp/resolv.conf (SentryUSB read-only root).
if [ -n "${new_domain_name_servers:-}" ]; then
  {
    for ns in $new_domain_name_servers; do
      echo "nameserver $ns"
    done
    [ -n "${new_domain_name:-}" ] && echo "search $new_domain_name"
  } > /tmp/resolv.conf
fi
"#;

const BT_UNBLOCK_SERVICE: &str = r#"[Unit]
Description=Unblock Bluetooth RF-kill
DefaultDependencies=no
Before=bluetooth.service hciuart.service
After=sysinit.target

[Service]
Type=oneshot
ExecStart=/usr/sbin/rfkill unblock bluetooth

[Install]
WantedBy=multi-user.target
"#;
