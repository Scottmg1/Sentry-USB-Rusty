//! Pre-setup sanity checks — port of `verify-configuration.sh`.
//!
//! Runs before any destructive operation and bails loudly (and early) if
//! the Pi isn't actually capable of running SentryUSB. Each check mirrors
//! a bash function in `verify-configuration.sh` so behavior is identical
//! across the Go and Rust setup paths.
//!
//! Checks, in order:
//!   1. [`check_supported_hardware`] — Pi Zero W / Zero 2 W / 3 / 4 / 5.
//!   2. [`check_udc`] — at least one UDC driver exposed under
//!      `/sys/class/udc/`. Implies `dtoverlay=dwc2` is in config.txt.
//!   3. [`check_xfs_support`] — `mkfs.xfs -m reflink=1` succeeds on a
//!      loopback image (installs `xfsprogs` if missing).
//!   4. [`check_required_config`] — critical config variables present
//!      (`CAM_SIZE`).
//!   5. [`check_available_space`] — SD card or USB drive has enough room
//!      for the backing files partition.
//!
//! On failure the returned `anyhow::Error` is logged and the runner
//! aborts before touching the filesystem.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::env::{PiModel, SetupEnv};
use crate::SetupEmitter;

/// Minimum usable space on the SD card (8 GiB) after root-partition shrink.
/// Older code required 32 GiB which blocked anything under a ~38 GB card
/// even though the actual footprint is ~8 GB.
const MIN_SD_SPACE_BYTES: u64 = 8 * (1 << 30);

/// Minimum total size of an external USB drive (59 GiB, rounded to match
/// the bash threshold).
const MIN_USB_SIZE_BYTES: u64 = 59 * (1 << 30);

/// Run all pre-setup sanity checks. Returns `Ok(())` when it's safe to
/// proceed; `Err` aborts the runner before any destructive operation.
pub async fn verify_setup(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    check_supported_hardware(env)?;
    check_udc()?;
    check_xfs_support(emitter).await?;
    check_required_config(env)?;
    check_available_space(env, emitter).await?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Individual checks
// -----------------------------------------------------------------------------

fn check_supported_hardware(env: &SetupEnv) -> Result<()> {
    // Not-a-Pi skips the check entirely — matches bash: non-Pi boards
    // (RockPi, Radxa) are handled by other setup paths and aren't our
    // problem here. Pi 2 is NOT supported (no USB gadget hardware).
    match env.pi_model {
        PiModel::Pi5 | PiModel::Pi4 | PiModel::Pi3 | PiModel::PiZero2 | PiModel::PiZeroW => {
            Ok(())
        }
        PiModel::Pi2 => bail!(
            "STOP: unsupported hardware: Raspberry Pi 2. \
             (only Pi Zero W, Pi Zero 2 W, Pi 3, Pi 4, and Pi 5 have the necessary hardware to run SentryUSB)"
        ),
        PiModel::Other => {
            // Could be a RockPi / Radxa Zero / genuinely unknown board.
            // Bash returns silently for non-Pi boards; we do the same.
            Ok(())
        }
    }
}

fn check_udc() -> Result<()> {
    let udc_dir = Path::new("/sys/class/udc");
    // Count symlinks under /sys/class/udc/. Bash uses `find -type l`.
    let count = match std::fs::read_dir(udc_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_symlink()).unwrap_or(false))
            .count(),
        Err(_) => 0,
    };
    if count == 0 {
        let model = std::fs::read_to_string("/sys/firmware/devicetree/base/model")
            .unwrap_or_default()
            .replace('\0', "");
        bail!(
            "STOP: this device ({}) does not have a UDC driver. \
             Check that dtoverlay=dwc2 is in the correct section of config.txt for your Pi model",
            model.trim()
        );
    }
    Ok(())
}

async fn check_xfs_support(emitter: &SetupEmitter) -> Result<()> {
    emitter.progress("Checking XFS support");

    // Install xfsprogs if the mkfs binary is missing.
    if sentryusb_shell::run("which", &["mkfs.xfs"]).await.is_err() {
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(180),
            "apt-get",
            &["-y", "install", "xfsprogs"],
        )
        .await
        .context("failed to install xfsprogs")?;
    }

    let img = "/tmp/xfs.img";
    let mnt = "/tmp/xfsmnt";

    // Cleanup any leftovers from a previous interrupted run. A stuck
    // mount at `mnt` (umount failed silently, or we crashed mid-check)
    // would otherwise make the fresh mount below fail with "mount point
    // busy" and we'd incorrectly report "STOP: xfs does not support
    // required features". Escalate: plain umount → lazy umount → bail.
    let _ = sentryusb_shell::run("umount", &[mnt]).await;
    if sentryusb_shell::run("findmnt", &[mnt]).await.is_ok() {
        let _ = sentryusb_shell::run("umount", &["-l", mnt]).await;
        if sentryusb_shell::run("findmnt", &[mnt]).await.is_ok() {
            bail!(
                "STOP: {} is still a mount point after umount + lazy umount — reboot and re-run setup",
                mnt
            );
        }
    }
    let _ = std::fs::remove_file(img);
    let _ = std::fs::remove_dir_all(mnt);

    // 1 GB loopback image.
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "truncate",
        &["-s", "1GB", img],
    )
    .await
    .context("truncate xfs test image")?;

    // reflink=1 is the feature Sentry USB actually needs (copy-on-write
    // snapshots of the cam image). If mkfs can make the fs but mount
    // fails, the kernel doesn't support the required features.
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "mkfs.xfs",
        &["-m", "reflink=1", "-f", img],
    )
    .await
    .context("mkfs.xfs failed — kernel likely lacks reflink support")?;

    std::fs::create_dir_all(mnt)?;
    if sentryusb_shell::run("mount", &[img, mnt]).await.is_err() {
        let _ = std::fs::remove_file(img);
        let _ = std::fs::remove_dir_all(mnt);
        bail!("STOP: xfs does not support required features");
    }

    // Success — clean up.
    let _ = sentryusb_shell::run("umount", &[mnt]).await;
    let _ = std::fs::remove_file(img);
    let _ = std::fs::remove_dir_all(mnt);

    emitter.progress("XFS supported");
    Ok(())
}

fn check_required_config(env: &SetupEnv) -> Result<()> {
    // Bash bails if CAM_SIZE isn't set at all. In Rust the config already
    // has a default of "0" (unset/zero triggers the SD fallback), so an
    // explicitly empty or literal-0 CAM_SIZE still runs the setup — but
    // a truly missing key is a user-config error we should surface.
    if !env.config.contains_key("CAM_SIZE") {
        bail!(
            "STOP: Define the variable CAM_SIZE in sentryusb.conf like this: \
             export CAM_SIZE=32"
        );
    }
    Ok(())
}

async fn check_available_space(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    match env.data_drive.as_deref() {
        None => {
            emitter.progress("DATA_DRIVE is not set. SD card will be used.");
            check_available_space_sd(env, emitter).await
        }
        Some(drive) if Path::new(drive).exists() => {
            emitter.progress(&format!(
                "DATA_DRIVE is set to {}. This will be used for /mutable and /backingfiles.",
                drive
            ));
            check_available_space_usb(drive, emitter).await
        }
        Some(drive) => bail!(
            "STOP: DATA_DRIVE is set to {}, which does not exist.",
            drive
        ),
    }
}

async fn check_available_space_sd(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    emitter.progress("Verifying that there is sufficient space available on the MicroSD card...");

    // Fast path: partitions already exist from a previous run.
    let backingfiles_dev = "/dev/disk/by-label/backingfiles";
    let mutable_dev = "/dev/disk/by-label/mutable";
    if Path::new(backingfiles_dev).exists() && Path::new(mutable_dev).exists() {
        let size_output = sentryusb_shell::run(
            "blockdev",
            &["--getsize64", backingfiles_dev],
        )
        .await
        .context("blockdev --getsize64 backingfiles")?;
        let size: u64 = size_output.trim().parse().unwrap_or(0);
        if size < MIN_SD_SPACE_BYTES {
            bail!(
                "STOP: Existing backingfiles partition is too small ({}GB, need at least {}GB)",
                size / 1024 / 1024 / 1024,
                MIN_SD_SPACE_BYTES / 1024 / 1024 / 1024
            );
        }
        emitter.progress("There is sufficient space available.");
        return Ok(());
    }

    // Fresh partition: `sfdisk -F <disk>` reports free space. The first
    // line of the "free space" report has "XXX bytes" which we parse.
    let boot_disk = env
        .boot_disk
        .as_deref()
        .context("check_available_space_sd: BOOT_DISK is not set")?;

    let sfdisk_out =
        sentryusb_shell::run("sfdisk", &["-F", boot_disk])
            .await
            .context("sfdisk -F")?;

    // First "N bytes" match wins — matches bash `grep -o '[0-9]* bytes' | head -1`.
    let available_space = sfdisk_out
        .lines()
        .find_map(parse_bytes_from_line)
        .unwrap_or(0);

    if available_space < MIN_SD_SPACE_BYTES {
        let parted = sentryusb_shell::run("parted", &[boot_disk, "print"])
            .await
            .unwrap_or_default();
        bail!(
            "STOP: The MicroSD card is too small: {}GB available, need at least {}GB.\n{}",
            available_space / 1024 / 1024 / 1024,
            MIN_SD_SPACE_BYTES / 1024 / 1024 / 1024,
            parted
        );
    }

    emitter.progress("There is sufficient space available.");
    Ok(())
}

async fn check_available_space_usb(drive: &str, emitter: &SetupEmitter) -> Result<()> {
    emitter.progress("Verifying that there is sufficient space available on the USB drive ...");

    // 30-second timeout — a sleeping / I/O-error USB drive can hang lsblk
    // indefinitely otherwise. Match bash's explicit `timeout 30` wrapping.
    let lsblk_out = sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "lsblk",
        &["-pno", "TYPE", drive],
    )
    .await
    .with_context(|| {
        format!(
            "Could not read {} (drive may be unresponsive or disconnected). \
             Try unplugging and reconnecting it.",
            drive
        )
    })?;

    let drive_type = lsblk_out.lines().next().unwrap_or("").trim();
    if drive_type != "disk" {
        bail!(
            "STOP: The specified drive ({}) is not a disk (TYPE={}). \
             Please specify path to the disk.",
            drive,
            drive_type
        );
    }

    let size_out = sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "blockdev",
        &["--getsize64", drive],
    )
    .await
    .with_context(|| {
        format!(
            "Could not read size of {} (drive may be unresponsive). \
             Try unplugging and reconnecting it.",
            drive
        )
    })?;

    let drive_size: u64 = size_out.trim().parse().unwrap_or(0);
    if drive_size < MIN_USB_SIZE_BYTES {
        let parted = sentryusb_shell::run("parted", &[drive, "print"])
            .await
            .unwrap_or_default();
        bail!(
            "STOP: The USB drive is too small: {}GB available. Expected at least 64GB\n{}",
            drive_size / 1024 / 1024 / 1024,
            parted
        );
    }

    emitter.progress("There is sufficient space available.");
    Ok(())
}

// -----------------------------------------------------------------------------
// Parsing helpers
// -----------------------------------------------------------------------------

/// Parse the first "N bytes" occurrence on a line — e.g.
/// `Unpartitioned space /dev/mmcblk0: 10737418240 bytes, 10.7 GiB`.
fn parse_bytes_from_line(line: &str) -> Option<u64> {
    // Scan for a run of digits immediately followed by " bytes".
    let bytes_idx = line.find(" bytes")?;
    let prefix = &line[..bytes_idx];
    let digits: String = prefix
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bytes_picks_trailing_number_before_bytes() {
        // Real sfdisk output shape.
        let line = "Unpartitioned space /dev/mmcblk0: 10737418240 bytes, 10.7 GiB";
        assert_eq!(parse_bytes_from_line(line), Some(10_737_418_240));
    }

    #[test]
    fn parse_bytes_none_when_absent() {
        // " bytes" matches but no digits immediately before → None.
        assert_eq!(parse_bytes_from_line("no bytes here"), None);
        assert_eq!(
            parse_bytes_from_line("/dev/mmcblk0 30GB"),
            None,
            "no `bytes` substring → no match"
        );
    }

    #[test]
    fn parse_bytes_handles_leading_text() {
        assert_eq!(
            parse_bytes_from_line("size: 123456789 bytes total"),
            Some(123_456_789)
        );
    }
}
