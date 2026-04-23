//! Read-only root filesystem — line-by-line port of `make-root-fs-readonly.sh`.
//!
//! Makes the SD root filesystem read-only and sets up the tmpfs / bind-mount
//! / dispatcher scaffolding the rest of the system needs to keep working when
//! it can't write to /. Getting this wrong bricks networking, DNS, Bluetooth
//! bonds, and fsck on every subsequent boot — so preserve bash semantics
//! exactly and default to best-effort (the bash uses `|| true` everywhere).

use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use tracing::info;

use crate::env::SetupEnv;
use crate::SetupEmitter;

const FSTAB_PATH: &str = "/etc/fstab";

/// Make the root filesystem read-only.
pub async fn make_readonly(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    if env.get_bool("SKIP_READONLY", false) {
        emitter.progress("SKIP_READONLY is set, skipping read-only filesystem setup");
        return Ok(false);
    }

    // Skip if root is already read-only and cmdline.txt already has `ro`.
    if already_readonly(env) {
        return Ok(false);
    }

    emitter.begin_phase("readonly", "Read-only filesystem");
    emitter.progress("Making root filesystem read-only...");

    ensure_boot_rw().await;

    // ---- Disable services that write frequently ----
    emitter.progress("Disabling unnecessary services...");
    for svc in &["apt-daily.timer", "apt-daily-upgrade.timer"] {
        let _ = sentryusb_shell::run("systemctl", &["disable", svc]).await;
    }
    // Conflict with USB gadget / not needed on read-only setups.
    for svc in &["amlogic-adbd", "radxa-adbd", "radxa-usbnet", "armbian-led-state"] {
        let _ = sentryusb_shell::run("systemctl", &["disable", svc]).await;
    }

    // ---- Protect essential packages from autoremove ----
    // Non-Raspbian distros (e.g. DietPi) install these as auto-dependencies
    // that `apt-get autoremove --purge` would otherwise sweep away, killing
    // WiFi on the very reboot we're preparing for.
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

    // ---- Remove packages that write constantly ----
    emitter.progress("Removing packages incompatible with read-only root...");
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(180),
        "apt-get",
        &["remove", "-y", "--purge", "triggerhappy", "logrotate", "dphys-swapfile"],
    ).await;
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(180),
        "apt-get",
        &["-y", "autoremove", "--purge"],
    ).await;

    // ---- Replace log management with busybox + install ntp ----
    emitter.progress("Installing ntp and busybox-syslogd...");
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(180),
        "bash",
        &["-c", "apt-get -y install ntp busybox-syslogd; dpkg --purge rsyslog"],
    ).await;

    emitter.progress("Configuring system...");

    // ---- cmdline.txt: remove `fastboot`, add `fsck.mode=auto` / `noswap` / `ro` ----
    // `fastboot` disables fsck on boot. With a read-only root, fsck running
    // at boot is our only chance to catch corruption — so we explicitly
    // remove fastboot if present and force fsck.mode=auto.
    if let Some(cmdline_path) = &env.cmdline_path {
        remove_cmdline_param(cmdline_path, "fastboot")?;
        append_cmdline_param(cmdline_path, "fsck.mode=auto")?;
        append_cmdline_param(cmdline_path, "noswap")?;
        append_cmdline_param(cmdline_path, "ro")?;
    }

    // ---- tune2fs: check every boot ----
    if let Some(root_dev) = &env.root_partition {
        if let Err(e) = sentryusb_shell::run("tune2fs", &["-c", "1", root_dev]).await {
            info!("tune2fs failed for rootfs ({}): {}", root_dev, e);
        }
    }
    if let Err(e) = sentryusb_shell::run(
        "tune2fs", &["-c", "1", "/dev/disk/by-label/mutable"],
    ).await {
        info!("tune2fs failed for mutable: {}", e);
    }

    // We're not using swap — delete the swap file for some extra space.
    let _ = std::fs::remove_file("/var/swap");

    // ---- fake-hwclock migration ----
    // Must remain functional during setup (configure-rtc.sh may run later and
    // replaces fake-hwclock with real hwclock). Without this migration, any
    // reboot during setup has no time source at all.
    ensure_mutable_mounted(emitter).await;
    let _ = std::fs::create_dir_all("/mutable/etc");

    if !Path::new("/etc/fake-hwclock.data").is_symlink()
        && Path::new("/etc/fake-hwclock.data").exists()
    {
        emitter.progress("Moving fake-hwclock data");
        let _ = std::fs::rename("/etc/fake-hwclock.data", "/mutable/etc/fake-hwclock.data");
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink("/mutable/etc/fake-hwclock.data", "/etc/fake-hwclock.data");
    }
    // Delay fake-hwclock until /mutable is mounted.
    if Path::new("/lib/systemd/system/fake-hwclock.service").exists() {
        sed_in_place(
            "/lib/systemd/system/fake-hwclock.service",
            |line| {
                if line.starts_with("Before=") {
                    "After=mutable.mount".to_string()
                } else {
                    line.to_string()
                }
            },
        )?;
    }

    // ---- /var/lib/NetworkManager runtime state ----
    // A tmpfs (not a symlink to /mutable) because NM's built-in dnsmasq
    // writes lease files here. If not writable, the AP connection enters an
    // enable/disable loop that thrashes the radio and kills all WiFi.
    if Path::new("/var/lib/NetworkManager").is_dir()
        && !Path::new("/var/lib/NetworkManager").is_symlink()
    {
        emitter.progress("Backing up /var/lib/NetworkManager to mutable");
        let _ = std::fs::create_dir_all("/mutable/var/lib");
        let _ = sentryusb_shell::run(
            "cp", &["-a", "/var/lib/NetworkManager", "/mutable/var/lib/"],
        ).await;
    }
    // Undo any symlink left by a previous broken setup.
    if Path::new("/var/lib/NetworkManager").is_symlink() {
        emitter.progress("Replacing /var/lib/NetworkManager symlink with directory for tmpfs");
        let _ = std::fs::remove_file("/var/lib/NetworkManager");
        let _ = std::fs::create_dir_all("/var/lib/NetworkManager");
    }

    // ---- NetworkManager connection profiles ----
    // Keep on root FS so they're available at boot even if /mutable (on USB)
    // hasn't mounted yet. Back up a copy to /mutable for reference/restore.
    if Path::new("/etc/NetworkManager/system-connections").is_dir()
        && !Path::new("/etc/NetworkManager/system-connections").is_symlink()
    {
        emitter.progress("Backing up NetworkManager connection profiles to mutable");
        let _ = std::fs::create_dir_all("/mutable/etc/NetworkManager");
        let _ = sentryusb_shell::run(
            "cp", &["-a", "/etc/NetworkManager/system-connections", "/mutable/etc/NetworkManager/"],
        ).await;
    }
    // Undo broken symlink — restore real directory from /mutable if possible.
    if Path::new("/etc/NetworkManager/system-connections").is_symlink() {
        emitter.progress("Restoring NetworkManager connection profiles to root FS");
        let _ = std::fs::remove_file("/etc/NetworkManager/system-connections");
        if Path::new("/mutable/etc/NetworkManager/system-connections").is_dir() {
            let _ = sentryusb_shell::run(
                "cp", &["-a", "/mutable/etc/NetworkManager/system-connections", "/etc/NetworkManager/"],
            ).await;
        } else {
            let _ = std::fs::create_dir_all("/etc/NetworkManager/system-connections");
        }
    }

    // ---- BlueZ bond storage ----
    // BlueZ persists pairing keys to /var/lib/bluetooth. On a read-only root
    // the write fails and bluetooth.service can crash during pairing.
    // Bind-mount from `.bluetooth` (dot-prefixed so the folder is hidden
    // from Finder/Explorer when a user plugs the drive into a computer).
    if !Path::new("/mutable/.bluetooth").is_dir() {
        emitter.progress("Creating /mutable/.bluetooth for BlueZ bond persistence");
        let _ = std::fs::create_dir_all("/mutable/.bluetooth");
        if Path::new("/var/lib/bluetooth").is_dir()
            && std::fs::read_dir("/var/lib/bluetooth")
                .map(|mut e| e.next().is_some())
                .unwrap_or(false)
        {
            let _ = sentryusb_shell::run(
                "cp", &["-a", "/var/lib/bluetooth/.", "/mutable/.bluetooth/"],
            ).await;
        }
        let _ = sentryusb_shell::run("chmod", &["700", "/mutable/.bluetooth"]).await;
    }

    // ---- DHCP lease directories: real dirs for tmpfs (not symlinks) ----
    if Path::new("/var/lib/dhcp").is_symlink() {
        emitter.progress("Replacing /var/lib/dhcp symlink with directory for tmpfs");
        let _ = std::fs::remove_file("/var/lib/dhcp");
        let _ = std::fs::create_dir_all("/var/lib/dhcp");
    }
    if Path::new("/var/lib/dhcpcd").is_symlink() {
        emitter.progress("Replacing /var/lib/dhcpcd symlink with directory for tmpfs");
        let _ = std::fs::remove_file("/var/lib/dhcpcd");
        let _ = std::fs::create_dir_all("/var/lib/dhcpcd");
    }

    // Make sure /mutable/configs exists for user configuration overlays.
    let _ = std::fs::create_dir_all("/mutable/configs");

    // ---- /var/spool: move to tmpfs ----
    if Path::new("/var/spool").is_symlink() {
        emitter.progress("fixing /var/spool");
        let _ = std::fs::remove_file("/var/spool");
        let _ = std::fs::create_dir_all("/var/spool");
        let _ = sentryusb_shell::run("chmod", &["755", "/var/spool"]).await;
    } else if Path::new("/var/spool").is_dir() {
        // Wipe existing contents so the tmpfs mount doesn't hide stale data.
        for entry in std::fs::read_dir("/var/spool").into_iter().flatten().flatten() {
            let _ = if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                std::fs::remove_dir_all(entry.path())
            } else {
                std::fs::remove_file(entry.path())
            };
        }
    }

    // Change spool permissions in var.conf (rondie/Margaret fix).
    // tmpfs /var/spool gets default permissions from /usr/lib/tmpfiles.d/var.conf;
    // bump from 0755 to 1777 so non-root processes (e.g. cron, at) can write.
    if Path::new("/usr/lib/tmpfiles.d/var.conf").exists() {
        sed_in_place("/usr/lib/tmpfiles.d/var.conf", |line| {
            // spool  0755  ... → spool 1777 ...
            if let Some(idx) = line.find("spool") {
                let (prefix, rest) = line.split_at(idx + "spool".len());
                let trimmed = rest.trim_start();
                if let Some(after_mode) = trimmed.strip_prefix("0755") {
                    return format!("{} 1777{}", prefix, after_mode);
                }
            }
            line.to_string()
        })?;
    }

    // ---- resolv.conf → /tmp/resolv.conf ----
    // /tmp is a tmpfs that is always writable at boot. Previous versions
    // symlinked to /mutable, which broke if the USB drive was slow.
    // Also redirect away from systemd-resolved's stub path (/run/systemd/resolve/…)
    // because we set NM to dns=none below and use a dispatcher to populate
    // resolv.conf directly — systemd-resolved would conflict.
    let resolv_target = std::fs::read_link("/etc/resolv.conf")
        .ok()
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

    // tmpfiles.d rule to seed /tmp/resolv.conf on every boot so the symlink
    // doesn't dangle while DHCP/NM come up.
    emitter.progress("Installing tmpfiles.d rule for resolv.conf");
    let _ = std::fs::create_dir_all("/etc/tmpfiles.d");
    std::fs::write(
        "/etc/tmpfiles.d/resolv-fallback.conf",
        "f /tmp/resolv.conf 0644 root root - nameserver 1.1.1.1\n",
    )?;

    // ---- DHCP client hooks to populate /tmp/resolv.conf ----
    install_nm_dns_config(emitter).await?;
    install_dhcpcd_hook(emitter).await?;
    install_dhclient_hook(emitter).await?;

    // ---- Disable systemd-resolved (conflicts with our setup) ----
    if sentryusb_shell::run("systemctl", &["is-active", "--quiet", "systemd-resolved"])
        .await
        .is_ok()
    {
        emitter.progress("Disabling systemd-resolved (dispatcher handles DNS directly)");
        let _ = sentryusb_shell::run("systemctl", &["stop", "systemd-resolved"]).await;
        let _ = sentryusb_shell::run("systemctl", &["disable", "systemd-resolved"]).await;
    }

    // ---- Bluetooth rfkill ----
    // Unblock right now for the remainder of setup.
    let _ = sentryusb_shell::run("rfkill", &["unblock", "bluetooth"]).await;

    // Install a oneshot systemd service to unblock BT on every boot. The BT
    // radio starts soft-blocked by default on RPi; on a read-only root the
    // block never clears, breaking BLE (Tesla BLE key).
    emitter.progress("Installing Bluetooth rfkill-unblock boot service");
    std::fs::write(
        "/etc/systemd/system/rfkill-unblock-bluetooth.service",
        BLUETOOTH_UNBLOCK_SERVICE,
    )?;
    let _ = sentryusb_shell::run(
        "systemctl", &["enable", "rfkill-unblock-bluetooth.service"],
    ).await;

    // ---- Reload NM config (dns=none + dispatcher) non-disruptively ----
    // A full restart would drop WiFi and kill SSH sessions mid-setup. The
    // reboot that follows will fully apply the new config.
    if sentryusb_shell::run("systemctl", &["is-active", "--quiet", "NetworkManager"])
        .await
        .is_ok()
    {
        emitter.progress("Reloading NetworkManager configuration");
        let _ = sentryusb_shell::run("nmcli", &["general", "reload"]).await;
    }

    // ---- fstab: ro on boot + root, tmpfs for writables ----
    update_fstab()?;

    // Work around mount warning printed when /etc/fstab is newer than
    // /run/systemd/systemd-units-load.
    let _ = sentryusb_shell::run("touch", &["-t", "197001010000", FSTAB_PATH]).await;

    // ---- autofs dependency trim ----
    // autofs by default depends on network services (NFS mounting). We don't
    // use NFS; removing the deps speeds up boot.
    if !Path::new("/etc/systemd/system/autofs.service").exists()
        && Path::new("/lib/systemd/system/autofs.service").exists()
    {
        let orig = std::fs::read_to_string("/lib/systemd/system/autofs.service")
            .unwrap_or_default();
        let filtered: String = orig
            .lines()
            .filter(|l| !l.starts_with("Wants=") && !l.starts_with("After="))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write("/etc/systemd/system/autofs.service", filtered + "\n")?;
    }

    // ---- remountfs_rw helper (bash wrapper for compatibility) ----
    let _ = std::fs::create_dir_all("/root/bin");
    std::fs::write("/root/bin/remountfs_rw", "#!/bin/bash\nmount / -o remount,rw\n")?;
    let _ = sentryusb_shell::run("chmod", &["+x", "/root/bin/remountfs_rw"]).await;

    emitter.progress("Read-only filesystem setup complete.");
    Ok(true)
}

// -------------------- helpers --------------------

fn already_readonly(env: &SetupEnv) -> bool {
    let existing_fstab = std::fs::read_to_string(FSTAB_PATH).unwrap_or_default();
    let root_ro = existing_fstab.lines().any(|l| {
        !l.starts_with('#') && l.contains(" / ") && l.contains("ext4") && l.contains(",ro")
    });
    let cmdline_ro = env
        .cmdline_path
        .as_deref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|c| c.split_whitespace().any(|w| w == "ro"))
        .unwrap_or(false);
    root_ro && cmdline_ro
}

async fn ensure_boot_rw() {
    // /sentryusb is the Rust-preferred symlink; /teslausb is the legacy name
    // some upgraded installs still have. /boot/firmware is the bookworm path.
    for mp in &["/sentryusb", "/teslausb", "/boot/firmware", "/boot"] {
        if is_mount_point(mp).await {
            let _ = sentryusb_shell::run("mount", &[mp, "-o", "remount,rw"]).await;
            break;
        }
    }
}

async fn is_mount_point(path: &str) -> bool {
    sentryusb_shell::run("findmnt", &[path]).await.is_ok()
}

async fn ensure_mutable_mounted(emitter: &SetupEmitter) {
    if is_mount_point("/mutable").await {
        return;
    }
    let fstab = std::fs::read_to_string(FSTAB_PATH).unwrap_or_default();
    if !fstab.contains("LABEL=mutable") {
        return;
    }
    emitter.progress("Mounting the mutable partition...");
    let _ = sentryusb_shell::run("mount", &["/mutable"]).await;
}

/// Append a parameter to cmdline.txt if it's not already present.
fn append_cmdline_param(path: &str, param: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let trimmed = content.trim();
    if trimmed.split_whitespace().any(|w| w == param) {
        return Ok(());
    }
    std::fs::write(path, format!("{} {}\n", trimmed, param))?;
    info!("Added '{}' to {}", param, path);
    Ok(())
}

/// Remove a parameter from cmdline.txt if present. Preserves the rest of the
/// one-line kernel command line exactly.
fn remove_cmdline_param(path: &str, param: &str) -> Result<()> {
    let content = std::fs::read_to_string(path)?;
    let trimmed = content.trim();
    let words: Vec<&str> = trimmed.split_whitespace().filter(|w| *w != param).collect();
    let new = words.join(" ");
    if new != trimmed {
        std::fs::write(path, format!("{}\n", new))?;
        info!("Removed '{}' from {}", param, path);
    }
    Ok(())
}

/// Read-modify-write a file line-by-line.
fn sed_in_place<F>(path: &str, mut f: F) -> Result<()>
where
    F: FnMut(&str) -> String,
{
    let content = std::fs::read_to_string(path)?;
    let had_trailing_newline = content.ends_with('\n');
    let new: Vec<String> = content.lines().map(|l| f(l)).collect();
    let mut out = new.join("\n");
    if had_trailing_newline {
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(())
}

async fn seed_tmp_resolv(existing_target: &str) {
    // Try nmcli first, then the existing resolv.conf, then fallback to 1.1.1.1.
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
                let _ = std::fs::write(
                    "/tmp/resolv.conf",
                    format!("{}\n", ns_lines),
                );
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
        NM_DISPATCHER_SCRIPT,
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
    std::fs::write(
        "/lib/dhcpcd/dhcpcd-hooks/90-sentryusb-resolv",
        DHCPCD_HOOK,
    )?;
    let _ = sentryusb_shell::run(
        "chmod", &["0644", "/lib/dhcpcd/dhcpcd-hooks/90-sentryusb-resolv"],
    ).await;
    Ok(())
}

async fn install_dhclient_hook(emitter: &SetupEmitter) -> Result<()> {
    // Only for systems using /etc/network/interfaces + dhclient (no NM, no dhcpcd).
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
    std::fs::write(
        "/etc/dhcp/dhclient-exit-hooks.d/sentryusb-resolv",
        DHCLIENT_HOOK,
    )?;
    let _ = sentryusb_shell::run(
        "chmod", &["0755", "/etc/dhcp/dhclient-exit-hooks.d/sentryusb-resolv"],
    ).await;
    Ok(())
}

fn update_fstab() -> Result<()> {
    let mut fstab = std::fs::read_to_string(FSTAB_PATH).unwrap_or_default();

    // --- add `,ro` to boot + root vfat/ext4 lines (if not already present) ---
    let mut lines: Vec<String> = Vec::new();
    for line in fstab.lines() {
        let commented = line.trim_start().starts_with('#');
        if commented {
            lines.push(line.to_string());
            continue;
        }

        let fields: Vec<&str> = line.split_whitespace().collect();
        let (mp, fstype, opts_idx) = match fields.as_slice() {
            [_, mp, fstype, ..] => (*mp, *fstype, 3usize),
            _ => {
                lines.push(line.to_string());
                continue;
            }
        };

        let add_ro = matches!(
            (mp, fstype),
            ("/boot", "vfat") | ("/boot/firmware", "vfat") | ("/", "ext4")
        );
        if !add_ro {
            lines.push(line.to_string());
            continue;
        }

        let opts = fields.get(opts_idx).copied().unwrap_or("defaults");
        if opts.split(',').any(|o| o == "ro") {
            lines.push(line.to_string());
            continue;
        }

        // Reconstruct the line with `,ro` appended to the options field.
        let mut new_fields: Vec<String> = fields.iter().map(|s| s.to_string()).collect();
        let new_opts = if opts == "defaults" {
            "defaults,ro".to_string()
        } else {
            format!("{},ro", opts)
        };
        if new_fields.len() > opts_idx {
            new_fields[opts_idx] = new_opts;
        }
        lines.push(new_fields.join(" "));
    }
    fstab = lines.join("\n");
    if !fstab.ends_with('\n') {
        fstab.push('\n');
    }

    // --- ensure tmpfs entries exist ---
    let tmpfs_entries: &[(&str, &str)] = &[
        ("/var/log", "tmpfs /var/log tmpfs nodev,nosuid 0 0"),
        ("/var/tmp", "tmpfs /var/tmp tmpfs nodev,nosuid 0 0"),
        ("/tmp", "tmpfs /tmp    tmpfs nodev,nosuid 0 0"),
        ("/var/spool", "tmpfs /var/spool tmpfs nodev,nosuid 0 0"),
        ("/var/lib/ntp", "tmpfs /var/lib/ntp tmpfs nodev,nosuid 0 0"),
        // NetworkManager needs mode=0700 so dnsmasq lease files have the
        // right permissions for NM's internal access checks.
        (
            "/var/lib/NetworkManager",
            "tmpfs /var/lib/NetworkManager tmpfs nodev,nosuid,mode=0700 0 0",
        ),
        ("/var/lib/dhcp", "tmpfs /var/lib/dhcp tmpfs nodev,nosuid 0 0"),
        ("/var/lib/dhcpcd", "tmpfs /var/lib/dhcpcd tmpfs nodev,nosuid 0 0"),
        // rfkill state on tmpfs so systemd-rfkill doesn't restore a stale
        // soft-block from the moment root went read-only — otherwise
        // Bluetooth stays blocked on every boot and BLE (Tesla key) breaks.
        (
            "/var/lib/systemd/rfkill",
            "tmpfs /var/lib/systemd/rfkill tmpfs nodev,nosuid 0 0",
        ),
    ];

    for (mp, entry) in tmpfs_entries {
        if fstab_has_mountpoint(&fstab, mp) {
            continue;
        }
        // Ensure the mount point directory exists, since tmpfs mounts over it.
        // /var/lib/ntp is a special case: bash wipes and recreates it.
        if *mp == "/var/lib/ntp" && !Path::new(mp).is_dir() {
            let _ = std::fs::remove_file(mp);
            let _ = std::fs::create_dir_all(mp);
        } else {
            let _ = std::fs::create_dir_all(mp);
        }
        fstab.push_str(entry);
        fstab.push('\n');
    }

    // Bind-mount /mutable/.bluetooth over /var/lib/bluetooth so BlueZ can
    // persist bond keys on the read-only root FS. x-systemd.requires-mounts-for
    // guarantees /mutable mounts first; x-systemd.before ensures the bind is
    // in place before bluetoothd starts.
    if !fstab_has_mountpoint(&fstab, "/var/lib/bluetooth") {
        fstab.push_str(
            "/mutable/.bluetooth /var/lib/bluetooth none \
             bind,x-systemd.requires-mounts-for=/mutable,x-systemd.before=bluetooth.service 0 0\n",
        );
    }

    std::fs::write(FSTAB_PATH, fstab)?;
    Ok(())
}

fn fstab_has_mountpoint(fstab: &str, mountpoint: &str) -> bool {
    fstab.lines().any(|line| {
        if line.trim_start().starts_with('#') {
            return false;
        }
        let mut fields = line.split_whitespace();
        fields.next(); // spec
        fields.next() == Some(mountpoint)
    })
}

// -------------------- embedded resources --------------------

const NM_DISPATCHER_SCRIPT: &str = r#"#!/bin/bash
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

const BLUETOOTH_UNBLOCK_SERVICE: &str = r#"[Unit]
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
