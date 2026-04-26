//! Partition management — replaces `create-backingfiles-partition.sh`.
//!
//! Handles detecting existing partitions, creating new backingfiles (XFS) and
//! mutable (ext4) partitions, and updating /etc/fstab.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::env::SetupEnv;
use crate::SetupEmitter;

const BACKINGFILES_MOUNT: &str = "/backingfiles";
const MUTABLE_MOUNT: &str = "/mutable";

/// Check if the backingfiles and mutable partitions already exist and are valid.
pub async fn partitions_exist() -> bool {
    Path::new("/dev/disk/by-label/backingfiles").exists()
        && Path::new("/dev/disk/by-label/mutable").exists()
}

/// Ensure xfsprogs is installed.
async fn ensure_xfs_tools() -> Result<()> {
    if sentryusb_shell::run("which", &["mkfs.xfs"]).await.is_err() {
        info!("Installing xfsprogs...");
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(600),
            "apt-get", &["-y", "install", "xfsprogs"],
        ).await.context("failed to install xfsprogs")?;
    }
    Ok(())
}

/// Determine the partition name prefix for a device (e.g. "p" for mmcblk, "" for sd).
fn partition_prefix(device: &str) -> &'static str {
    if device.contains("mmcblk") || device.contains("nvme") || device.contains("loop") {
        "p"
    } else {
        ""
    }
}

/// Create partitions on an external DATA_DRIVE. Returns true if any work was performed.
pub async fn setup_data_drive(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let data_drive = env.data_drive.as_deref()
        .context("DATA_DRIVE not set")?;

    let prefix = partition_prefix(data_drive);
    let p1 = format!("{}{}{}", data_drive, prefix, 1);
    let p2 = format!("{}{}{}", data_drive, prefix, 2);

    let bf_ok = check_label_matches(&p2, "backingfiles").await;
    let mut_ok = check_label_matches(&p1, "mutable").await;
    let bf_xfs = check_fstype(&p2, "xfs").await;
    let mut_ext4 = check_fstype(&p1, "ext4").await;

    let already_partitioned = bf_ok && mut_ok && bf_xfs && mut_ext4;
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let fstab_complete = fstab.contains("LABEL=backingfiles") && fstab.contains("LABEL=mutable");

    if already_partitioned && fstab_complete {
        return Ok(false);
    }

    emitter.begin_phase("partitions", "Disk partitioning");
    emitter.progress(&format!("DATA_DRIVE is set to {}", data_drive));

    if already_partitioned {
        emitter.progress("Existing backingfiles (xfs) and mutable (ext4) partitions found. Keeping them.");

        // Drop any auto-mount or stale mount holding the device first.
        // Without this, xfs_repair contends with whatever mounted the
        // partition (systemd auto-mount, autofs, udisks2, etc.), often
        // hits the 60s timeout, and then our subsequent `mount` call
        // fails with "Can't open blockdev" because the loser still
        // has /dev/sda2 open exclusively.
        emitter.progress(&format!("Releasing any active mounts on {}...", data_drive));
        cleanup_mounts().await;

        emitter.progress(&format!("Clearing XFS log on {}...", p2));
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(60), "xfs_repair", &["-L", &p2],
        ).await;

        // Let udev reprobe the device after xfs_repair so the kernel
        // releases the inode it briefly held during the log replay.
        let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=10"]).await;
    } else {
        emitter.progress(&format!("Unmounting partitions on {}...", data_drive));
        cleanup_mounts().await;

        emitter.progress(&format!("WARNING: This will delete EVERYTHING on {}", data_drive));
        sentryusb_shell::run("wipefs", &["-afq", data_drive]).await
            .context("wipefs failed")?;
        sentryusb_shell::run("parted", &[data_drive, "--script", "mktable", "gpt"]).await
            .context("parted mktable failed")?;

        emitter.progress("Creating partitions...");
        sentryusb_shell::run(
            "parted", &["-a", "optimal", "-m", data_drive, "mkpart", "primary", "ext4", "0%", "2GB"],
        ).await?;
        sentryusb_shell::run(
            "parted", &["-a", "optimal", "-m", data_drive, "mkpart", "primary", "ext4", "2GB", "100%"],
        ).await?;

        let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=10"]).await;

        emitter.progress(&format!("Formatting mutable partition (ext4) on {}...", p1));
        sentryusb_shell::run("mkfs.ext4", &["-F", "-L", "mutable", &p1]).await
            .context("mkfs.ext4 failed")?;

        emitter.progress(&format!("Formatting backingfiles partition (xfs) on {}...", p2));
        sentryusb_shell::run("mkfs.xfs", &["-f", "-m", "reflink=1", "-L", "backingfiles", &p2]).await
            .context("mkfs.xfs failed")?;

        emitter.progress("Partition formatting complete.");
    }

    update_fstab().await?;
    Ok(true)
}

/// Create partitions on the SD card (after the root partition). Returns true if work was done.
pub async fn setup_sd_card(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let boot_disk = env.boot_disk.as_deref()
        .context("Could not detect boot disk")?;

    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let fstab_complete = fstab.contains("LABEL=backingfiles") && fstab.contains("LABEL=mutable");

    if partitions_exist().await && fstab_complete {
        return Ok(false);
    }

    emitter.begin_phase("partitions", "Disk partitioning");

    ensure_xfs_tools().await?;

    if partitions_exist().await {
        emitter.progress("Using existing backingfiles and mutable partitions");
        update_fstab().await?;
        return Ok(true);
    }

    emitter.progress("Creating backingfiles and mutable partitions on SD card...");

    // Get last partition info
    let output = sentryusb_shell::run(
        "bash", &["-c", &format!(
            "sfdisk -q -l {} | tail +2 | sort -n -k 2 | tail -1 | awk '{{print $1}}'", boot_disk
        )],
    ).await?;
    let last_part_dev = output.trim().to_string();
    let last_part_num: u32 = last_part_dev.chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .parse()
        .context("could not parse partition number")?;

    let prefix = partition_prefix(boot_disk);
    let bf_dev = format!("{}{}{}", boot_disk, prefix, last_part_num + 1);
    let mut_dev = format!("{}{}{}", boot_disk, prefix, last_part_num + 2);

    // Calculate sectors
    let disk_sectors: u64 = sentryusb_shell::run(
        "blockdev", &["--getsz", boot_disk],
    ).await?.trim().parse().context("blockdev parse error")?;

    let last_disk_sector = disk_sectors - 1;
    // 300 MB for mutable
    let first_mutable_sector = last_disk_sector - 614400 + 1;

    let last_part_end: u64 = sentryusb_shell::run(
        "bash", &["-c", &format!(
            "sfdisk -o End -q -l {} | tail +2 | sort -n | tail -1", boot_disk
        )],
    ).await?.trim().parse().context("sfdisk End parse error")?;

    // Round up to 1MB boundary
    let first_bf_sector = ((last_part_end + 1 + 2047) / 2048) * 2048;
    let bf_num_sectors = first_mutable_sector - first_bf_sector;

    // Preserve disk identifier for fstab/cmdline.txt
    let orig_id = get_disk_identifier(boot_disk).await?;

    emitter.progress("Creating backingfiles partition...");
    sentryusb_shell::run(
        "bash", &["-c", &format!(
            "echo '{},{}' | sfdisk --force --no-reread {} -N {}",
            first_bf_sector, bf_num_sectors, boot_disk, last_part_num + 1
        )],
    ).await.context("sfdisk backingfiles failed")?;

    emitter.progress("Creating mutable partition...");
    sentryusb_shell::run(
        "bash", &["-c", &format!(
            "echo '{},' | sfdisk --force --no-reread {} -N {}",
            first_mutable_sector, boot_disk, last_part_num + 2
        )],
    ).await.context("sfdisk mutable failed")?;

    let _ = sentryusb_shell::run("partprobe", &[boot_disk]).await;
    let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=10"]).await;

    // Add partitions to kernel if needed
    if !Path::new(&bf_dev).exists() || !Path::new(&mut_dev).exists() {
        let _ = sentryusb_shell::run(
            "partx", &["--add", "--nr", &format!("{}:{}", last_part_num + 1, last_part_num + 2), boot_disk],
        ).await;
        let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=10"]).await;
    }

    if !Path::new(&bf_dev).exists() || !Path::new(&mut_dev).exists() {
        bail!("Failed to create partitions: {} or {} not found", bf_dev, mut_dev);
    }

    // Update disk identifier in fstab and cmdline.txt
    let new_id = get_disk_identifier(boot_disk).await?;
    if orig_id != new_id {
        emitter.progress("Updating disk identifier in fstab and cmdline.txt...");
        let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
        std::fs::write("/etc/fstab", fstab.replace(&orig_id, &new_id))?;

        if let Some(cmdline) = &env.cmdline_path {
            if Path::new(cmdline).exists() {
                let content = std::fs::read_to_string(cmdline).unwrap_or_default();
                std::fs::write(cmdline, content.replace(&orig_id, &new_id))?;
            }
        }
    }

    // Calculate mutable inodes: ~1 per 20000 sectors of backingfiles
    let mutable_inodes = bf_num_sectors / 20000;

    emitter.progress(&format!("Formatting backingfiles (xfs) on {}...", bf_dev));
    sentryusb_shell::run("mkfs.xfs", &["-f", "-m", "reflink=1", "-L", "backingfiles", &bf_dev]).await
        .context("mkfs.xfs failed")?;

    emitter.progress(&format!("Formatting mutable (ext4) on {}...", mut_dev));
    sentryusb_shell::run(
        "mkfs.ext4", &["-F", "-N", &mutable_inodes.to_string(), "-L", "mutable", &mut_dev],
    ).await.context("mkfs.ext4 failed")?;

    emitter.progress("Partition formatting complete.");
    update_fstab().await?;
    Ok(true)
}

async fn get_disk_identifier(disk: &str) -> Result<String> {
    let output = sentryusb_shell::run(
        "bash", &["-c", &format!(
            "fdisk -l {} | grep 'Disk identifier' | sed 's/Disk identifier: 0x//'", disk
        )],
    ).await?;
    Ok(output.trim().to_string())
}

async fn check_label_matches(device: &str, label: &str) -> bool {
    let symlink = format!("/dev/disk/by-label/{}", label);
    if let Ok(target) = std::fs::read_link(&symlink) {
        let target_str = target.to_string_lossy();
        let dev_name = Path::new(device).file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        target_str.ends_with(&dev_name)
    } else {
        false
    }
}

async fn check_fstype(device: &str, expected: &str) -> bool {
    sentryusb_shell::run("bash", &["-c", &format!(
        "blkid {} | grep -q 'TYPE=\"{}\"'", device, expected
    )]).await.is_ok()
}

async fn cleanup_mounts() {
    for mount in &["/mnt/cam", "/mnt/music", "/mnt/lightshow", "/mnt/boombox", "/backingfiles", "/mutable"] {
        let _ = sentryusb_shell::run("umount", &[mount]).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
}

/// Ensure /etc/fstab has entries for backingfiles and mutable.
async fn update_fstab() -> Result<()> {
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();

    let mut additions = String::new();

    if !fstab.contains("LABEL=backingfiles") {
        additions.push_str(&format!(
            "LABEL=backingfiles {} xfs auto,rw,noatime,nofail 0 2\n", BACKINGFILES_MOUNT
        ));
    }
    if !fstab.contains("LABEL=mutable") {
        additions.push_str(&format!(
            "LABEL=mutable {} ext4 auto,rw,nofail 0 2\n", MUTABLE_MOUNT
        ));
    }

    if !additions.is_empty() {
        let mut new_fstab = fstab;
        if !new_fstab.ends_with('\n') {
            new_fstab.push('\n');
        }
        new_fstab.push_str(&additions);
        std::fs::write("/etc/fstab", new_fstab)?;
        info!("Updated /etc/fstab with backingfiles and mutable entries");
    }

    // Ensure mount points exist
    let _ = std::fs::create_dir_all(BACKINGFILES_MOUNT);
    let _ = std::fs::create_dir_all(MUTABLE_MOUNT);

    Ok(())
}
