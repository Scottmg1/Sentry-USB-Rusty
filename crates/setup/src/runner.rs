//! Setup runner — the main orchestrator that replaces `setup-sentryusb`.
//!
//! Every phase function owns its own idempotency check and only announces
//! itself via `emitter.begin_phase(..)` when it actually has work to do,
//! so the wizard's live phase list shows only the phases that are being
//! executed this run. Re-runs after a mid-setup reboot silently skip the
//! already-completed phases instead of re-lighting every step.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};
use tracing::info;

use crate::env::SetupEnv;
use crate::SetupEmitter;

const SETUP_LOG: &str = "/sentryusb/sentryusb-setup.log";
const SETUP_PHASES_FILE: &str = "/sentryusb/setup-phases.jsonl";
const SETUP_FINISHED_MARKER: &str = "/sentryusb/SENTRYUSB_SETUP_FINISHED";
const SETUP_STARTED_MARKER: &str = "/sentryusb/SENTRYUSB_SETUP_STARTED";

/// Build a `SetupEmitter` whose progress callback writes to the setup log
/// file and whose phase callback appends to `setup-phases.jsonl`. The two
/// extra closures are invoked after the file I/O so callers can forward
/// events over WebSocket (etc).
pub fn make_emitter(
    progress_extra: impl Fn(&str) + Send + Sync + 'static,
    phase_extra: impl Fn(&str, &str) + Send + Sync + 'static,
) -> SetupEmitter {
    let progress = move |msg: &str| {
        let stamped = format!("{} : {}", chrono_now(), msg);
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(SETUP_LOG)
        {
            use std::io::Write;
            let _ = writeln!(f, "{}", stamped);
        }
        info!("[setup] {}", msg);
        progress_extra(msg);
    };
    let phase = move |id: &str, label: &str| {
        let line = serde_json::json!({"id": id, "label": label}).to_string();
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true).append(true).open(SETUP_PHASES_FILE)
        {
            use std::io::Write;
            let _ = writeln!(f, "{}", line);
        }
        info!("[setup] phase: {} ({})", label, id);
        phase_extra(id, label);
    };
    SetupEmitter::new(progress, phase)
}

fn chrono_now() -> String {
    std::process::Command::new("date")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "???".to_string())
}

/// Run the full setup process. Idempotent on re-runs after reboots.
pub async fn run_full_setup(emitter: SetupEmitter) -> Result<()> {
    if !am_root() {
        bail!("Setup must run as root");
    }

    let resuming = Path::new(SETUP_STARTED_MARKER).exists();

    let _ = std::fs::remove_file(SETUP_FINISHED_MARKER);
    let _ = std::fs::create_dir_all("/sentryusb");
    let _ = std::fs::write(SETUP_STARTED_MARKER, "");

    // Clear the phases ledger on a fresh start so the UI list starts empty.
    // Only truncate on the very first run (no STARTED marker yet, no
    // partitions). Resumes after a mid-flow reboot must preserve the ledger.
    if !resuming && !crate::partition::partitions_exist().await {
        let _ = std::fs::remove_file(SETUP_PHASES_FILE);
    }

    if !resuming {
        emitter.progress("=== SentryUSB Setup Starting ===");
    } else {
        emitter.progress("--- Resuming setup after reboot ---");
    }

    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    // Phase: detect environment (no UI phase — always fast)
    let env = SetupEnv::detect().await?;
    if !resuming {
        emitter.progress(&format!("Detected: {}", env.pi_model.display_name()));
    }

    // Pre-setup sanity checks — hardware model, XFS + reflink support,
    // required config vars, disk space. Deliberately does NOT include
    // the UDC check yet: on a fresh Pi OS image (install-pi.sh path)
    // `dtoverlay=dwc2` isn't in config.txt yet, so `/sys/class/udc/` is
    // empty and a UDC check here would always fail before we got a
    // chance to add the overlay. See verify.rs for the rationale.
    //
    // On a resume we skip these — the first pass already passed, and
    // the XFS loopback check is expensive to redo.
    if !resuming {
        crate::verify::early_verify(&env, &emitter).await?;
    }

    // WiFi regulatory (silent no-op when already set)
    configure_wifi_regulatory(&env, &emitter).await?;

    // dwc2 USB gadget overlay — reboots if added.
    if configure_dwc2_overlay(&env, &emitter).await? {
        emitter.progress("Rebooting to apply dwc2 overlay change...");
        reboot().await;
        return Ok(());
    }

    // dwc2 is now either already-loaded (the normal resume path) or
    // already in config.txt from a previous run. Either way the kernel
    // should have the DWC2 UDC exposed under /sys/class/udc/. Bail
    // loudly if not — proceeding into partitioning / gadget setup with
    // a missing UDC gives confusing downstream errors.
    crate::verify::verify_udc()?;

    // Root partition shrink (reboots twice in its own flow)
    if check_root_shrink(&env, &emitter).await? {
        return Ok(());
    }

    // Disk-space verification. Runs here (AFTER root shrink) rather
    // than in early_verify because on a fresh Pi OS install the root
    // partition fills the entire SD/SSD and `sfdisk -F` reports 0
    // bytes unpartitioned — the shrink above is what creates the
    // 8 GB we need for backingfiles+mutable. Fast path via the
    // `/dev/disk/by-label/backingfiles` check short-circuits on
    // repeat runs.
    crate::verify::verify_disk_space(&env, &emitter).await?;

    // Hostname + timezone (grouped under "System configuration")
    let hostname_changed = crate::system::configure_hostname(&env, &emitter).await?;
    let tz_changed = crate::system::configure_timezone(&env, &emitter).await?;
    if hostname_changed || tz_changed {
        // Progress already written; emit the phase retroactively so the UI
        // records this grouping exactly once, without double-announcing.
        emitter.begin_phase("system_basics", "System configuration");
    }

    // Package index refresh.
    update_package_index(&emitter).await?;

    // cmdline.txt modules — reboots if changed.
    if fix_cmdline_modules(&env, &emitter).await? {
        emitter.progress("Rebooting to apply cmdline.txt change...");
        reboot().await;
        return Ok(());
    }

    // Required packages (announces its own phase on work).
    crate::system::install_required_packages(&emitter).await?;

    // Runtime helper scripts.
    crate::scripts::install_runtime_scripts(&emitter).await?;

    // UAS quirks (silent unless it added an entry).
    fix_uas_quirks(&env, &emitter).await?;

    // Partitioning.
    if env.data_drive.is_some() {
        crate::partition::setup_data_drive(&env, &emitter).await?;
    } else {
        crate::partition::setup_sd_card(&env, &emitter).await?;
    }

    // Mount partitions (helper phase with its own idempotency).
    mount_partitions(&emitter).await?;

    // Disk images.
    crate::disk_images::create_disk_images(&env, &emitter).await?;

    update_image_fstab_entries().await?;
    initialize_drive_directories().await?;

    // Archive configuration.
    if env.get_bool("CONFIGURE_ARCHIVING", true) {
        crate::archive::configure_archive(&env, &emitter).await?;
    }

    // Samba.
    crate::system::configure_samba(&env, &emitter).await?;

    // WiFi AP — only when both SSID and a valid password are set.
    let has_ap_ssid = env.config.get("AP_SSID").is_some_and(|v| !v.is_empty());
    let has_ap_pass = env.config.get("AP_PASS").is_some_and(|v| v.len() >= 8);
    if has_ap_ssid && has_ap_pass {
        crate::network::configure_ap(&env, &emitter).await?;
    }

    // SSH hardening.
    crate::system::configure_ssh(&emitter).await?;

    // Avahi mDNS.
    crate::system::configure_avahi(&env, &emitter).await?;

    // Snapshot automount (autofs → /tmp/snapshots). Needed before the
    // readonly phase so /etc/auto.master.d is writable.
    crate::automount::configure_automount(&emitter).await?;

    // TeslaCam FUSE mount wiring (cttseraser). Writes /sbin/mount.ctts,
    // fstab entry, enables user_allow_other. Must run before readonly so
    // /etc/fstab and /etc/fuse.conf are writable.
    crate::cttseraser_mount::configure_web_mount(&emitter).await?;

    // RTC.
    crate::system::configure_rtc(&env, &emitter).await?;

    // Tesla BLE peripheral (silent if VIN unset or already configured).
    crate::archive::configure_tesla_ble(&env, &emitter).await?;

    // Read-only filesystem.
    crate::readonly::make_readonly(&env, &emitter).await?;

    // Optional package upgrade.
    if env.get_bool("UPGRADE_PACKAGES", false) {
        emitter.begin_phase("upgrade_packages", "Upgrading packages");
        emitter.progress("Upgrading installed packages...");
        let _ = sentryusb_shell::run("apt-get", &["clean"]).await;
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(600), "apt-get", &["--assume-yes", "upgrade"],
        ).await;
        let _ = sentryusb_shell::run("apt-get", &["clean"]).await;
    }

    let _ = std::fs::remove_file(SETUP_STARTED_MARKER);
    let _ = std::fs::write(SETUP_FINISHED_MARKER, "");

    emitter.progress("=== SentryUSB Setup Complete ===");
    emitter.progress("Rebooting in 5 seconds to apply changes...");

    // Auto-reboot so read-only root, cmdline.txt changes, and partition table
    // updates take effect without a manual step. Small delay lets SSE clients
    // flush the completion message.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let _ = sentryusb_shell::run("systemctl", &["reboot"]).await;
    });

    Ok(())
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

async fn reboot() {
    // Don't go through logind — it may be broken on minimal images and
    // stall for 25s+ per dbus-activation timeout. Talk to systemd directly
    // and fall back to kernel reboot.
    if tokio::process::Command::new("systemctl")
        .args(["--force", "reboot"])
        .spawn()
        .is_ok()
    {
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    let _ = tokio::process::Command::new("reboot").arg("-f").spawn();
}

/// Persist US regulatory domain via module param and /etc/default/crda if not
/// already set. Silent no-op otherwise.
async fn configure_wifi_regulatory(_env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    if sentryusb_shell::run("systemctl", &["-q", "is-enabled", "NetworkManager.service"]).await.is_err() {
        return Ok(());
    }
    let output = sentryusb_shell::run(
        "bash", &["-c", "iw reg get 2>/dev/null | grep -oP '(?<=country )\\w+' | head -1"],
    ).await.unwrap_or_default();
    let reg = output.trim();
    if !(reg.is_empty() || reg == "00") {
        return Ok(());
    }

    emitter.begin_phase("wifi_regdom", "WiFi regulatory domain");
    emitter.progress("Setting WiFi regulatory domain to US");
    let _ = sentryusb_shell::run("iw", &["reg", "set", "US"]).await;
    let _ = std::fs::write("/etc/default/crda", "REGDOMAIN=US\n");
    let _ = sentryusb_shell::run(
        "bash", &["-c", "mkdir -p /etc/modprobe.d && echo 'options cfg80211 ieee80211_regdom=US' > /etc/modprobe.d/cfg80211.conf"],
    ).await;
    Ok(())
}

/// Configure the dwc2 USB gadget overlay in config.txt with proper per-model
/// sections. Returns true if a change was made (requires a reboot).
async fn configure_dwc2_overlay(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let config_path = match &env.piconfig_path {
        Some(p) => p.clone(),
        None => return Ok(false),
    };

    let config = std::fs::read_to_string(&config_path).unwrap_or_default();
    let section = env.pi_model.config_section();

    let overlay_line = if env.pi_model == crate::env::PiModel::Pi3 {
        "dtoverlay=dwc2,dr_mode=peripheral"
    } else {
        "dtoverlay=dwc2"
    };

    if section == "all" {
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

        emitter.begin_phase("dwc2_overlay", "USB gadget overlay");
        if config.contains("[all]") {
            let new = config.replacen("[all]", &format!("[all]\n{}", overlay_line), 1);
            std::fs::write(&config_path, new)?;
        } else {
            let mut f = std::fs::OpenOptions::new().append(true).open(&config_path)?;
            use std::io::Write;
            writeln!(f, "\n{}", overlay_line)?;
        }
    } else {
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

        emitter.begin_phase("dwc2_overlay", "USB gadget overlay");
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
                continue;
            }
            lines.push(line.to_string());
        }
        std::fs::write(&config_path, lines.join("\n") + "\n")?;
    }

    emitter.progress(&format!("Added {} to config.txt under [{}]", overlay_line, section));
    Ok(true)
}

/// Check whether the root partition needs shrinking (Pi Imager auto-expand case).
async fn check_root_shrink(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    if crate::partition::partitions_exist().await {
        return Ok(false);
    }

    let boot_disk = match &env.boot_disk {
        Some(d) => d.clone(),
        None => return Ok(false),
    };

    let root_dev = sentryusb_shell::run(
        "bash", &["-c", "lsblk -dpno name \"$(findmnt -D -no SOURCE --target /)\""],
    ).await.unwrap_or_default().trim().to_string();

    let root_part_num = sentryusb_shell::run(
        "bash", &["-c", &format!("echo '{}' | grep -o '[0-9]*$'", root_dev)],
    ).await.unwrap_or_default().trim().to_string();

    let resize_result_file = "/root/RESIZE_RESULT";
    let resize_marker = "/root/RESIZE_ATTEMPTED";

    if Path::new(resize_result_file).exists() {
        let result = std::fs::read_to_string(resize_result_file).unwrap_or_default();
        let result = result.trim();
        let _ = std::fs::remove_file(resize_result_file);

        if result == "success" {
            emitter.begin_phase("root_shrink", "Shrinking root partition");
            emitter.progress("Root filesystem resize completed successfully during boot.");
            let _ = std::fs::remove_file(resize_marker);

            emitter.progress("Shrinking root partition table entry to match filesystem...");
            let sector_size = sentryusb_shell::run(
                "bash", &["-c", &format!(
                    "cat /sys/block/$(lsblk -no pkname '{}')/queue/hw_sector_size", root_dev
                )],
            ).await.unwrap_or_else(|_| "512".to_string()).trim().parse::<u64>().unwrap_or(512);

            let tune2fs_out = sentryusb_shell::run(
                "bash", &["-c", &format!(
                    "tune2fs -l '{}' | grep 'Block count:\\|Block size:' | awk '{{print $2}}' FS=: | tr -d ' '",
                    root_dev
                )],
            ).await.unwrap_or_default();
            let parts: Vec<&str> = tune2fs_out.trim().lines().collect();
            if parts.len() == 2 {
                let block_count: u64 = parts[0].trim().parse().unwrap_or(0);
                let block_size: u64 = parts[1].trim().parse().unwrap_or(4096);
                let fs_sectors = block_count * block_size / sector_size;

                let start_sector = sentryusb_shell::run(
                    "bash", &["-c", &format!("partx --show -g -o START '{}'", root_dev)],
                ).await.unwrap_or_default().trim().to_string();

                emitter.progress(&format!("Resizing partition to {} sectors", fs_sectors));
                let _ = sentryusb_shell::run_with_timeout(
                    Duration::from_secs(30),
                    "bash", &["-c", &format!(
                        "echo '{},{}' | sfdisk --force --no-reread '{}' -N {}",
                        start_sector, fs_sectors, boot_disk, root_part_num
                    )],
                ).await;
            }

            if Path::new("/sentryusb/config.txt").exists() {
                let config = std::fs::read_to_string("/sentryusb/config.txt").unwrap_or_default();
                if config.contains("SENTRYUSB-REMOVE") {
                    let cleaned: String = config.lines()
                        .filter(|l| !l.contains("SENTRYUSB-REMOVE"))
                        .collect::<Vec<_>>().join("\n");
                    let _ = std::fs::write("/sentryusb/config.txt", cleaned + "\n");
                    let initrd = format!("initrd.img-{}", std::env::consts::ARCH);
                    let _ = std::fs::remove_file(format!("/boot/{}", initrd));
                } else {
                    let _ = sentryusb_shell::run("update-initramfs", &["-u"]).await;
                }
            }

            emitter.progress("Root partition shrink complete, rebooting...");
            reboot().await;
            return Ok(true);

        } else if result.starts_with("fail:") {
            let _ = std::fs::remove_file(resize_marker);
            emitter.progress(&format!(
                "FATAL: Root filesystem resize failed: {}. Try reflashing with Balena Etcher instead of Raspberry Pi Imager.",
                result
            ));
            bail!("Root resize failed: {}", result);
        } else {
            emitter.progress(&format!("WARNING: Unrecognized resize result: {}", result));
            let _ = std::fs::remove_file(resize_marker);
        }
    }

    let output = sentryusb_shell::run(
        "bash", &["-c", &format!(
            "sfdisk -F '{}' 2>/dev/null | grep -o '[0-9]* bytes' | head -1 | awk '{{print $1}}'",
            boot_disk
        )],
    ).await.unwrap_or_default();
    let unpart_bytes: u64 = output.trim().parse().unwrap_or(0);
    let min_space: u64 = 8 * 1024 * 1024 * 1024;

    if unpart_bytes >= min_space {
        return Ok(false);
    }

    if Path::new(resize_marker).exists() {
        emitter.progress("FATAL: Previous root shrink attempt failed. Delete /root/RESIZE_ATTEMPTED and try again, or reflash with Balena Etcher.");
        bail!("Previous root shrink attempt failed. Reflash with Balena Etcher instead of Raspberry Pi Imager.");
    }

    emitter.begin_phase("root_shrink", "Shrinking root partition");
    emitter.progress(&format!(
        "Insufficient unpartitioned space ({} MB). Root partition needs shrinking.",
        unpart_bytes / 1024 / 1024
    ));
    emitter.progress("This usually happens when Raspberry Pi Imager is used to flash the image.");

    let last_part = sentryusb_shell::run(
        "bash", &["-c", &format!(
            "sfdisk -q -l '{}' | tail +2 | sort -n -k 2 | tail -1 | awk '{{print $1}}'",
            boot_disk
        )],
    ).await.unwrap_or_default().trim().to_string();
    if root_dev != last_part {
        emitter.progress("FATAL: Root is not the last partition. Cannot shrink. Reflash with Balena Etcher.");
        bail!("Root is not the last partition, cannot shrink");
    }

    let used_output = sentryusb_shell::run(
        "bash", &["-c", "df --output=used -k / | tail -1 | tr -d ' '"],
    ).await?;
    let used_kb: u64 = used_output.trim().parse().unwrap_or(0);
    let target_gb = ((used_kb / 1024 / 1024) + 2).max(6);

    emitter.progress(&format!("Shrinking root filesystem to {}GB to free space for setup...", target_gb));

    let _ = std::fs::write(resize_marker, "");

    let kernel_ver = sentryusb_shell::run("uname", &["-r"]).await?.trim().to_string();
    let initrd_name = format!("initrd.img-{}", kernel_ver);
    let boot_part = std::fs::read_link("/sentryusb")
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "/boot".to_string());

    let initrd_on_boot = format!("{}/{}", boot_part, initrd_name);
    let initrd_in_boot = format!("/boot/{}", initrd_name);

    if !Path::new(&initrd_on_boot).exists() && !Path::new(&initrd_in_boot).exists() {
        if Path::new("/sentryusb/config.txt").exists() {
            emitter.progress("Temporarily enabling initramfs for root resize...");
            let _ = sentryusb_shell::run_with_timeout(
                Duration::from_secs(120),
                "update-initramfs", &["-c", "-k", &kernel_ver],
            ).await;
            let mut f = std::fs::OpenOptions::new().append(true).open("/sentryusb/config.txt")?;
            use std::io::Write;
            writeln!(f, "initramfs {} followkernel # SENTRYUSB-REMOVE", initrd_name)?;
        } else {
            let _ = std::fs::remove_file(resize_marker);
            emitter.progress("FATAL: Cannot shrink root automatically. Reflash with Balena Etcher.");
            bail!("Cannot shrink root: no initramfs and no config.txt");
        }
    }

    if boot_part != "/boot" && Path::new(&initrd_in_boot).exists() {
        let _ = std::fs::copy(&initrd_in_boot, &initrd_on_boot);
    }

    emitter.progress("Installing initramfs resize hooks...");
    install_initramfs_resize_scripts(target_gb, &kernel_ver).await?;

    emitter.progress("Rebooting into initramfs to shrink root filesystem...");
    reboot().await;
    Ok(true)
}

async fn install_initramfs_resize_scripts(target_gb: u64, kernel_ver: &str) -> Result<()> {
    let hook = r#"#!/bin/sh
PREREQ=""
prereqs() { echo "$PREREQ"; }
case "$1" in prereqs) prereqs; exit 0;; esac
. /usr/share/initramfs-tools/hook-functions
copy_exec $(readlink -f /sbin/findfs) /sbin/findfs-full
copy_exec /sbin/e2fsck /sbin
copy_exec /sbin/resize2fs /sbin
copy_exec /bin/mount /bin
copy_exec /bin/umount /bin
"#;
    std::fs::create_dir_all("/etc/initramfs-tools/hooks")?;
    std::fs::write("/etc/initramfs-tools/hooks/resize2fs", hook)?;
    let _ = sentryusb_shell::run("chmod", &["+x", "/etc/initramfs-tools/hooks/resize2fs"]).await;

    let premount = format!(r#"#!/bin/sh
PREREQ=""
ROOT_SIZE="{target_gb}G"
prereqs() {{ echo "$PREREQ"; }}
case "$1" in prereqs) prereqs; exit 0;; esac
echo
echo "root=${{ROOT}}  "
while [ ! -d /dev/disk/by-partuuid ]; do
  echo "waiting for /dev/disk/by-partuuid"
  sleep 1
done
ROOT_DEVICE="$(/sbin/findfs-full "$ROOT")"
echo "root device name is ${{ROOT_DEVICE}}  "
if [ -x /sbin/vgchange ]; then
    /sbin/vgchange -a y || echo "vgchange: $?  "
fi
write_resize_marker() {{
  mkdir -p /tmp/rootmnt
  if mount "$ROOT_DEVICE" /tmp/rootmnt 2>/dev/null; then
    echo "$1" > /tmp/rootmnt/root/RESIZE_RESULT
    umount /tmp/rootmnt 2>/dev/null || true
  fi
  rmdir /tmp/rootmnt 2>/dev/null || true
}}
/sbin/e2fsck -y -v -f "$ROOT_DEVICE"
FSCK_RC=$?
if [ "$FSCK_RC" -le 2 ]; then
  if [ "$FSCK_RC" -ne 0 ]; then
    echo "e2fsck corrected filesystem errors (rc=$FSCK_RC), continuing with resize"
  fi
  if /sbin/resize2fs -f -d 8 "$ROOT_DEVICE" "$ROOT_SIZE"; then
    echo "resize2fs completed successfully"
    write_resize_marker "success"
  else
    RC=$?
    echo "resize2fs failed with exit code $RC"
    write_resize_marker "fail:resize2fs:$RC"
  fi
else
  echo "e2fsck $ROOT_DEVICE failed with uncorrectable errors (rc=$FSCK_RC)"
  write_resize_marker "fail:e2fsck:$FSCK_RC"
fi
"#);
    std::fs::create_dir_all("/etc/initramfs-tools/scripts/init-premount")?;
    std::fs::write("/etc/initramfs-tools/scripts/init-premount/resize", premount)?;
    let _ = sentryusb_shell::run("chmod", &["+x", "/etc/initramfs-tools/scripts/init-premount/resize"]).await;

    sentryusb_shell::run_with_timeout(
        Duration::from_secs(120),
        "update-initramfs", &["-v", "-u", "-k", kernel_ver],
    ).await?;

    let initrd_name = format!("initrd.img-{}", kernel_ver);
    let boot_part = std::fs::read_link("/sentryusb")
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "/boot".to_string());
    if boot_part != "/boot" {
        let src = format!("/boot/{}", initrd_name);
        let dst = format!("{}/{}", boot_part, initrd_name);
        if Path::new(&src).exists() {
            let _ = std::fs::copy(&src, &dst);
        }
    }

    let _ = std::fs::remove_file("/etc/initramfs-tools/hooks/resize2fs");
    let _ = std::fs::remove_file("/etc/initramfs-tools/scripts/init-premount/resize");

    Ok(())
}

/// Ensure cmdline.txt has `modules-load=dwc2,g_ether`. Returns true if a change
/// was made (caller should reboot).
async fn fix_cmdline_modules(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let cmdline_path = match &env.cmdline_path {
        Some(p) => p.clone(),
        None => return Ok(false),
    };

    let content = std::fs::read_to_string(&cmdline_path)?;
    if content.contains("dwc2") && content.contains("g_ether") {
        return Ok(false);
    }

    emitter.begin_phase("cmdline_modules", "Boot configuration");

    let new_content = content.trim().to_string();
    let new_content = if let Some(start) = new_content.find("modules-load=") {
        let end = new_content[start..].find(' ').unwrap_or(new_content.len() - start);
        format!("{}{}", &new_content[..start], &new_content[start + end..])
    } else {
        new_content
    };

    let final_content = format!("{} modules-load=dwc2,g_ether\n", new_content.trim());
    std::fs::write(&cmdline_path, final_content)?;
    emitter.progress("Updated cmdline.txt with modules-load=dwc2,g_ether");
    Ok(true)
}

/// Add UAS quirks for known problematic USB drives. Silent no-op when already present.
async fn fix_uas_quirks(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
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

    emitter.begin_phase("uas_quirks", "USB drive compatibility");
    let joined = new_entries.join(",");
    let new_content = if content.contains("usb-storage.quirks=") {
        content.replace(
            "usb-storage.quirks=",
            &format!("usb-storage.quirks={},", joined),
        )
    } else {
        format!("{} usb-storage.quirks={}\n", content.trim(), joined)
    };

    std::fs::write(&cmdline_path, new_content)?;
    emitter.progress(&format!("Added UAS quirks: {}", joined));
    Ok(())
}

/// Update the package index. Only announces if we actually need to run apt-get update.
async fn update_package_index(emitter: &SetupEmitter) -> Result<()> {
    // Quick heuristic: if /var/lib/apt/lists has been touched in the last
    // 6 hours, skip. Otherwise we always run it (safe to re-run, but slow).
    let lists_dir = Path::new("/var/lib/apt/lists");
    if let Ok(meta) = std::fs::metadata(lists_dir) {
        if let Ok(modified) = meta.modified() {
            if let Ok(elapsed) = modified.elapsed() {
                if elapsed < Duration::from_secs(6 * 3600) {
                    return Ok(());
                }
            }
        }
    }

    emitter.begin_phase("apt_update", "Refreshing package index");
    emitter.progress("Updating package index...");
    let _ = sentryusb_shell::run("dpkg", &["--configure", "-a"]).await;

    for attempt in 0..3 {
        if sentryusb_shell::run_with_timeout(
            Duration::from_secs(300),
            "apt-get", &["update"],
        ).await.is_ok() {
            return Ok(());
        }
        emitter.progress(&format!("apt-get update failed (attempt {}), retrying...", attempt + 1));
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "apt-get", &["update", "--allow-releaseinfo-change"],
    ).await?;
    Ok(())
}

/// Mount backingfiles and mutable partitions. Silent when already mounted.
async fn mount_partitions(emitter: &SetupEmitter) -> Result<()> {
    let _ = std::fs::create_dir_all("/backingfiles");
    let _ = std::fs::create_dir_all("/mutable");

    let bf_mounted = sentryusb_shell::run("findmnt", &["--mountpoint", "/backingfiles"]).await.is_ok();
    let mut_mounted = sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"]).await.is_ok();
    if bf_mounted && mut_mounted {
        return Ok(());
    }

    emitter.begin_phase("mount_partitions", "Mounting partitions");

    if !bf_mounted {
        emitter.progress("Mounting backingfiles partition...");
        if let Ok(dev) = sentryusb_shell::run("findfs", &["LABEL=backingfiles"]).await {
            let _ = sentryusb_shell::run_with_timeout(
                Duration::from_secs(60), "xfs_repair", &["-L", dev.trim()],
            ).await;
        }
        sentryusb_shell::run("mount", &["/backingfiles"]).await?;
    }

    if !mut_mounted {
        emitter.progress("Mounting mutable partition...");
        sentryusb_shell::run("mount", &["/mutable"]).await?;
    }

    Ok(())
}

/// Update fstab with sentryusb mount entries for disk image files.
async fn update_image_fstab_entries() -> Result<()> {
    let images = [
        ("/backingfiles/cam_disk.bin", "/mnt/cam"),
        ("/backingfiles/music_disk.bin", "/mnt/music"),
        ("/backingfiles/lightshow_disk.bin", "/mnt/lightshow"),
        ("/backingfiles/boombox_disk.bin", "/mnt/boombox"),
        ("/backingfiles/wraps_disk.bin", "/mnt/wraps"),
    ];

    let mut fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();

    let fstab_lines: Vec<&str> = fstab.lines()
        .filter(|l| !images.iter().any(|(img, _)| l.starts_with(img)))
        .collect();
    fstab = fstab_lines.join("\n");

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
async fn initialize_drive_directories() -> Result<()> {
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
