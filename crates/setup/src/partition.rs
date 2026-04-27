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

    // True idempotency only: partitions exist with correct labels/fstypes
    // AND fstab already references them. This is the resume-after-reboot
    // case (or a clean re-run that needs no work). Anything else means
    // the drive is in an unknown state and we wipe it cleanly.
    if already_partitioned && fstab_complete {
        return Ok(false);
    }

    emitter.begin_phase("partitions", "Disk partitioning");
    emitter.progress(&format!("DATA_DRIVE is set to {}", data_drive));

    // No "keep existing" branch — if we got here, either there are no
    // partitions yet, OR there are partitions but they don't match the
    // expected configuration (missing fstab entries, leftover state from
    // a previous install). Either way the right answer is wipe + fresh
    // mkfs, so the user lands on a known-good drive every time setup
    // runs to completion. The wizard's destructive-change detection
    // ([SetupWizard.tsx:140]) already warned the user before they hit
    // Save, so we're not erasing anything they didn't sign off on.
    if already_partitioned {
        emitter.progress(&format!(
            "Existing partitions on {} look stale (fstab incomplete) — wiping for a clean install",
            data_drive
        ));
    }

    emitter.progress(&format!("Unmounting partitions on {}...", data_drive));
    cleanup_mounts().await;

    // Comprehensive teardown: covers the auto-mounters and loop devices
    // that cleanup_mounts (well-known paths only) misses. Without this,
    // parted writes the new GPT but the kernel refuses to switch to it
    // because something on the system (commonly udisks2 having
    // auto-mounted the prior install's partition at /media/pi/<label>)
    // still has a partition open.
    emitter.progress(&format!("Releasing kernel-side holders on {}...", data_drive));
    release_data_drive(data_drive, emitter).await;

    emitter.progress(&format!("WARNING: This will delete EVERYTHING on {}", data_drive));
    // Bound every block-device operation. A stalled / wedged USB
    // bridge can hang wipefs or parted indefinitely, leaving the
    // wizard stuck on "Creating partitions..." with no way to recover.
    // 2 minutes is long enough for any healthy drive (mkfs.ext4
    // lazy-init means even multi-TB drives finish in seconds) and
    // short enough that the user notices a problem.
    let op_timeout = Duration::from_secs(120);
    sentryusb_shell::run_with_timeout(op_timeout, "wipefs", &["-afq", data_drive]).await
        .context("wipefs failed (drive unresponsive?)")?;
    sentryusb_shell::run_with_timeout(op_timeout, "parted",
        &[data_drive, "--script", "mktable", "gpt"]).await
        .context("parted mktable failed")?;

    emitter.progress("Creating partitions...");
    sentryusb_shell::run_with_timeout(op_timeout, "parted",
        &["-a", "optimal", "-m", data_drive, "mkpart", "primary", "ext4", "0%", "2GB"]).await?;
    sentryusb_shell::run_with_timeout(op_timeout, "parted",
        &["-a", "optimal", "-m", data_drive, "mkpart", "primary", "ext4", "2GB", "100%"]).await?;

    let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=30"]).await;

    emitter.progress(&format!("Formatting mutable partition (ext4) on {}...", p1));
    sentryusb_shell::run_with_timeout(op_timeout, "mkfs.ext4",
        &["-F", "-L", "mutable", &p1]).await.context("mkfs.ext4 failed")?;

    emitter.progress(&format!("Formatting backingfiles partition (xfs) on {}...", p2));
    sentryusb_shell::run_with_timeout(op_timeout, "mkfs.xfs",
        &["-f", "-m", "reflink=1", "-L", "backingfiles", &p2]).await.context("mkfs.xfs failed")?;

    emitter.progress("Partition formatting complete.");

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
    let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=30"]).await;

    // Add partitions to kernel if needed
    if !Path::new(&bf_dev).exists() || !Path::new(&mut_dev).exists() {
        let _ = sentryusb_shell::run(
            "partx", &["--add", "--nr", &format!("{}:{}", last_part_num + 1, last_part_num + 2), boot_disk],
        ).await;
        let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=30"]).await;
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

/// Aggressively release every kernel-side reference to `drive` and its
/// partitions before we rewrite the partition table.
///
/// Required because `parted ... mktable` writes the new GPT to disk but
/// then asks the kernel to re-read it — and that ioctl fails with
/// "Partition(s) N on /dev/X have been written, but we have been unable
/// to inform the kernel of the change, probably because it/they are in
/// use" if anything still holds a reference. The user reported this on
/// a fresh boot where systemd/udisks2 had auto-mounted the previous
/// install's `mutable` partition at `/media/pi/mutable`, which the
/// well-known-paths cleanup never touched.
///
/// Steps mirror what desktop "Disks" apps do before reformatting:
///   1. Disable the USB gadget so configfs isn't holding cam_disk.bin
///      across this teardown.
///   2. swapoff any swap partitions on the drive.
///   3. Lazy-force-unmount every mountpoint anywhere on the system that
///      lives on a partition of this drive (covers /media/pi/<label>,
///      /run/media/<user>/<label>, custom locations, anything).
///   4. Detach any loop devices backed by partitions of this drive.
///   5. wipefs each existing partition (clears the FS signature so
///      autofs / udisks2 don't immediately re-probe and grab it back).
///   6. `partx -d` to drop kernel partition table entries.
///   7. udevadm settle so pending change events finish.
///   8. blockdev --flushbufs + --rereadpt to make the kernel re-examine
///      the disk; if this still fails, parted will too, and the error
///      surfaces with enough context for the user to act.
async fn release_data_drive(drive: &str, emitter: &SetupEmitter) {
    let _ = sentryusb_gadget::disable();

    // Snapshot every partition of this drive plus its mountpoint and
    // fstype. lsblk pairs are stable; -P quotes them so spaces in
    // mountpoints don't break parsing. Skip the parent device row.
    let lsblk_out = sentryusb_shell::run(
        "lsblk", &["-Pno", "NAME,MOUNTPOINT,FSTYPE", "-p", drive],
    ).await.unwrap_or_default();

    let mut parts: Vec<(String, String, String)> = Vec::new();
    for line in lsblk_out.lines() {
        let mut name = String::new();
        let mut mp = String::new();
        let mut fst = String::new();
        for field in line.split_whitespace() {
            if let Some(v) = field.strip_prefix("NAME=") {
                name = v.trim_matches('"').to_string();
            } else if let Some(v) = field.strip_prefix("MOUNTPOINT=") {
                mp = v.trim_matches('"').to_string();
            } else if let Some(v) = field.strip_prefix("FSTYPE=") {
                fst = v.trim_matches('"').to_string();
            }
        }
        if !name.is_empty() && name != drive {
            parts.push((name, mp, fst));
        }
    }

    // Step 2 — swapoff
    for (name, _mp, fst) in &parts {
        if fst == "swap" {
            emitter.progress(&format!("swapoff {}", name));
            let _ = sentryusb_shell::run("swapoff", &[name]).await;
        }
    }

    // Step 3 — lazy-force-unmount every active mountpoint. Lazy + force
    // covers cases where a process still has the directory open: the
    // mount is detached from the namespace immediately so parted can
    // proceed, and the open fd is reaped when the process exits.
    for (name, mp, _fst) in &parts {
        if !mp.is_empty() && mp != "[SWAP]" {
            emitter.progress(&format!("Unmounting {} from {}", name, mp));
            let _ = sentryusb_shell::run("umount", &["-lf", mp]).await;
        }
    }

    // Step 4 — detach loopbacks. Cheap to ignore failures; -j prints the
    // matching loop device(s) which we then `-d`.
    for (name, _mp, _fst) in &parts {
        let loops = sentryusb_shell::run("losetup", &["-j", name]).await.unwrap_or_default();
        for line in loops.lines() {
            if let Some(loop_dev) = line.split(':').next() {
                let _ = sentryusb_shell::run("losetup", &["-d", loop_dev]).await;
            }
        }
    }

    // Step 5 — wipe FS signatures on each partition. Stops auto-probers
    // (udisks2, blkid, autofs) from re-grabbing the partition between
    // our umount and parted's BLKRRPART.
    for (name, _mp, _fst) in &parts {
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(60), "wipefs", &["-afq", name],
        ).await;
    }

    // Step 6 — drop kernel partition table mappings.
    let _ = sentryusb_shell::run("partx", &["-d", drive]).await;

    // Step 7 — let pending udev events finish before we touch the disk.
    let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=10"]).await;

    // Step 8 — flush page cache and force a partition-table reread. If
    // rereadpt still fails here, parted will give a clearer error.
    let _ = sentryusb_shell::run("blockdev", &["--flushbufs", drive]).await;
    let _ = sentryusb_shell::run("blockdev", &["--rereadpt", drive]).await;

    tokio::time::sleep(Duration::from_secs(2)).await;
}

/// Run `xfs_repair` against an XFS partition before we mount it on a
/// resume. We can't trust the previous run to have unmounted cleanly
/// (cttseraser FUSE crash, kernel panic, power loss…), so the FS may
/// have an uncommitted log entry or genuine metadata damage that
/// makes `mount` reject it with "Structure needs cleaning."
///
/// Strategy: try the safe pass first (no `-L`), fall back to log
/// zeroing only if a log replay is needed, then run one final pass to
/// confirm the structure is clean. All output is surfaced to the
/// wizard log so a stuck install reports something actionable
/// instead of silently retrying. Errors are non-fatal — the
/// downstream `mount` call decides whether the FS is usable.
pub(crate) async fn repair_xfs(dev: &str, emitter: &SetupEmitter) {
    // 5 min — xfs_repair on a damaged 1 TB SSD with millions of files
    // can legitimately run for a few minutes. The previous 60s ceiling
    // was killing repairs mid-flight, leaving the FS half-fixed and
    // making `mount` fail with "Structure needs cleaning" right
    // afterward.
    let timeout = Duration::from_secs(300);

    // Drop the kernel's buffer cache for this device before running
    // xfs_repair. If a previous mount attempt populated the page
    // cache with broken metadata (a corrupted FS, a half-formatted
    // partition, or a USB bridge that briefly returned stale bytes),
    // xfs_repair would otherwise see the *cached* version of the
    // device, not what's actually on disk. After a fresh mkfs.xfs on
    // a USB-attached drive (Samsung T7 etc.) without this flush, the
    // verify pass sees the previous run's corruption and we false-
    // positive into "partition unrecoverable".
    let _ = sentryusb_shell::run("blockdev", &["--flushbufs", dev]).await;
    let _ = sentryusb_shell::run("sync", &[]).await;

    emitter.progress(&format!("Checking XFS structure on {}...", dev));
    match sentryusb_shell::run_with_timeout(timeout, "xfs_repair", &[dev]).await {
        Ok(_) => {
            emitter.progress(&format!("XFS clean on {}", dev));
            // One more no-op pass so we know the verify is stable.
            let _ = sentryusb_shell::run_with_timeout(timeout, "xfs_repair", &[dev]).await;
            return;
        }
        Err(e) => {
            // Log replay failure surfaces as exit 2 + "needs to replay
            // a dirty log" on stderr. Other failures (genuine metadata
            // corruption) generally need `-L` to make any progress
            // anyway — at worst we drop a few uncommitted writes.
            let msg = e.to_string();
            emitter.progress(&format!(
                "xfs_repair pass on {} reported issues — falling back to log zeroing: {}",
                dev,
                truncate_for_log(&msg)
            ));
        }
    }

    emitter.progress(&format!("Clearing XFS log on {} (xfs_repair -L)...", dev));
    match sentryusb_shell::run_with_timeout(timeout, "xfs_repair", &["-L", dev]).await {
        Ok(_) => emitter.progress(&format!("XFS log cleared on {}", dev)),
        Err(e) => emitter.progress(&format!(
            "xfs_repair -L on {} returned an error (mount may still fail): {}",
            dev,
            truncate_for_log(&e.to_string())
        )),
    }

    // Verify pass after log zeroing. A clean exit here is what
    // tells us the subsequent mount has a chance.
    if let Err(e) = sentryusb_shell::run_with_timeout(timeout, "xfs_repair", &[dev]).await {
        emitter.progress(&format!(
            "xfs_repair verify on {} still reports errors — partition may be unrecoverable: {}",
            dev,
            truncate_for_log(&e.to_string())
        ));
    }

    // Final flush so the subsequent mount reads xfs_repair's writes
    // from disk rather than the page cache's pre-repair version.
    let _ = sentryusb_shell::run("sync", &[]).await;
    let _ = sentryusb_shell::run("blockdev", &["--flushbufs", dev]).await;
}

/// Trim long stderr blobs so the wizard log stays readable.
fn truncate_for_log(s: &str) -> String {
    const MAX: usize = 400;
    let s = s.trim();
    if s.len() <= MAX {
        s.to_string()
    } else {
        format!("{}…", &s[..MAX])
    }
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
