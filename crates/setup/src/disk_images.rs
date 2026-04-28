//! Disk image creation — replaces `create-backingfiles.sh`.
//!
//! Creates FAT32/exFAT disk images for cam, music, lightshow, and boombox
//! drives in /backingfiles/. Wraps & License Plates live as folders on the
//! cam drive — no dedicated partition.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::env::SetupEnv;
use crate::SetupEmitter;

const BACKINGFILES: &str = "/backingfiles";

/// Disk image spec.
struct DriveSpec {
    name: &'static str,
    label: &'static str,
    config_key: &'static str,
    default_fallback: &'static str,
}

const DRIVE_SPECS: &[DriveSpec] = &[
    DriveSpec { name: "cam", label: "CAM", config_key: "CAM_SIZE", default_fallback: "30G" },
    DriveSpec { name: "music", label: "MUSIC", config_key: "MUSIC_SIZE", default_fallback: "4G" },
    DriveSpec { name: "lightshow", label: "LIGHTSHOW", config_key: "LIGHTSHOW_SIZE", default_fallback: "1G" },
    DriveSpec { name: "boombox", label: "BOOMBOX", config_key: "BOOMBOX_SIZE", default_fallback: "100M" },
];

/// One-time cleanup for installs that previously had a dedicated wraps disk.
/// The 4 GB image is no longer used — Wraps & LicensePlate are now folders
/// on the cam drive. Reclaim the space on the next setup re-run.
fn purge_legacy_wraps_disk() {
    let _ = std::fs::remove_file(format!("{}/wraps_disk.bin", BACKINGFILES));
    let _ = std::fs::remove_file(format!("{}/wraps_disk.bin.opts", BACKINGFILES));
    let _ = std::fs::remove_dir("/mnt/wraps");
}

/// Parse a human-readable size like "30G", "4G", "100M" into KB.
pub fn dehumanize(s: &str) -> Result<u64> {
    let s = s.trim().to_uppercase()
        .replace("GB", "G")
        .replace("MB", "M")
        .replace("KB", "K");

    if s == "0" || s.is_empty() {
        return Ok(0);
    }

    if s.ends_with('G') {
        let n: f64 = s.trim_end_matches('G').parse()?;
        Ok((n * 1024.0 * 1024.0) as u64) // KB
    } else if s.ends_with('M') {
        let n: f64 = s.trim_end_matches('M').parse()?;
        Ok((n * 1024.0) as u64)
    } else if s.ends_with('K') {
        let n: f64 = s.trim_end_matches('K').parse()?;
        Ok(n as u64)
    } else {
        // Assume bytes
        let n: u64 = s.parse()?;
        Ok(n / 1024)
    }
}

/// Get available space in KB on /backingfiles, minus a safety margin.
async fn available_space_kb() -> Result<u64> {
    let output = sentryusb_shell::run(
        "df", &["--output=size", "--block-size=1K", &format!("{}/", BACKINGFILES)],
    ).await?;
    let total: u64 = output.lines().last().unwrap_or("0").trim().parse().unwrap_or(0);

    // Reserve 10% capped between 2GB and 10GB
    let ten_pct = total / 10;
    let min_pad = 2 * 1024 * 1024; // 2GB in KB
    let max_pad = 10 * 1024 * 1024; // 10GB in KB
    let padding = ten_pct.max(min_pad).min(max_pad);
    Ok(total.saturating_sub(padding))
}

/// Check if an existing image file matches the requested size (within 10MB).
fn image_matches(file: &str, requested_kb: u64) -> bool {
    if requested_kb == 0 {
        return !Path::new(file).exists();
    }
    if let Ok(meta) = std::fs::metadata(file) {
        let current_kb = meta.len() / 1024;
        let diff = (current_kb as i64 - requested_kb as i64).unsigned_abs();
        diff < 10240
    } else {
        false
    }
}

/// Create a single drive image file with a partition table and filesystem.
async fn create_drive(
    name: &str,
    label: &str,
    size_kb: u64,
    use_exfat: bool,
    emitter: &SetupEmitter,
) -> Result<()> {
    let filename = format!("{}/{}_disk.bin", BACKINGFILES, name);
    let mountpoint = format!("/mnt/{}", name);

    if size_kb == 0 {
        let _ = std::fs::remove_file(&filename);
        let _ = std::fs::remove_file(format!("{}.opts", filename));
        let _ = std::fs::remove_dir(&mountpoint);
        return Ok(());
    }

    emitter.progress(&format!("Allocating {}K for {}...", size_kb, filename));
    let _ = std::fs::remove_file(&filename);
    sentryusb_shell::run("truncate", &["--size", &format!("{}K", size_kb), &filename]).await
        .context("truncate failed")?;

    // Create partition table
    let sfdisk_type = if use_exfat { "type=7" } else { "type=c" };
    sentryusb_shell::run(
        "bash", &["-c", &format!("echo '{}' | sfdisk '{}'", sfdisk_type, filename)],
    ).await.context("sfdisk failed on disk image")?;

    // Find partition offset
    let offset = get_partition_offset(&filename).await?;

    // Set up loop device
    let loopdev = sentryusb_shell::run(
        "losetup", &["-f", "--show", "-o", &offset.to_string(), &filename],
    ).await.context("losetup failed")?.trim().to_string();

    let _ = sentryusb_shell::run("udevadm", &["settle", "--timeout=5"]).await;

    // Format
    emitter.progress(&format!("Creating filesystem with label '{}'", label));
    let format_result = if use_exfat {
        sentryusb_shell::run("mkfs.exfat", &[&loopdev, "-L", label]).await
    } else {
        sentryusb_shell::run("mkfs.vfat", &[&loopdev, "-F", "32", "-n", label]).await
    };

    let _ = sentryusb_shell::run("losetup", &["-d", &loopdev]).await;
    format_result.context("filesystem creation failed")?;

    let _ = std::fs::create_dir_all(&mountpoint);
    emitter.progress(&format!("Drive image {} ready.", filename));
    Ok(())
}

/// Get the byte offset of the first partition in a disk image.
async fn get_partition_offset(filename: &str) -> Result<u64> {
    let bytes_out = sentryusb_shell::run(
        "bash", &["-c", &format!("sfdisk -l -o Size -q --bytes '{}' | tail -1", filename)],
    ).await?;
    let size_in_bytes: u64 = bytes_out.trim().parse().context("parse size")?;

    let sectors_out = sentryusb_shell::run(
        "bash", &["-c", &format!("sfdisk -l -o Sectors -q '{}' | tail -1", filename)],
    ).await?;
    let size_in_sectors: u64 = sectors_out.trim().parse().context("parse sectors")?;

    let sector_size = size_in_bytes / size_in_sectors;

    let start_out = sentryusb_shell::run(
        "bash", &["-c", &format!("sfdisk -l -o Start -q '{}' | tail -1", filename)],
    ).await?;
    let start_sector: u64 = start_out.trim().parse().context("parse start")?;

    Ok(start_sector * sector_size)
}

/// Release all loop devices and unmount all drive image mount points.
async fn release_all_images() {
    let _ = sentryusb_shell::run("bash", &["-c", "killall archiveloop 2>/dev/null"]).await;
    // Use the usb_gadget crate to disable
    let _ = sentryusb_gadget::disable();
    // /mnt/wraps stays in the list to drain any leftover mount from a
    // pre-migration install before purge_legacy_wraps_disk runs.
    for mount in &["/mnt/cam", "/mnt/music", "/mnt/lightshow", "/mnt/boombox", "/mnt/wraps"] {
        let _ = sentryusb_shell::run("umount", &["-d", mount]).await;
    }
    let _ = sentryusb_shell::run(
        "bash", &["-c", "umount -d /backingfiles/snapshots/snap*/mnt 2>/dev/null"],
    ).await;
}

/// Ensure exfat tools are available if needed.
async fn ensure_exfat_tools(use_exfat: bool, emitter: &SetupEmitter) -> Result<bool> {
    if !use_exfat {
        return Ok(false);
    }

    // Check kernel support
    let has_kernel = sentryusb_shell::run(
        "bash", &["-c", "grep -q exfat /proc/filesystems || modprobe -n exfat"],
    ).await.is_ok();

    if !has_kernel {
        // Surface to the wizard log — a silent fallback would let the
        // user think they got an exFAT cam disk when they actually
        // got FAT32 (and FAT32's 4 GB per-file cap silently truncates
        // long Tesla clips).
        emitter.progress("WARNING: kernel does not support ExFAT — falling back to FAT32");
        return Ok(false);
    }

    // Install exfatprogs if needed
    if sentryusb_shell::run("which", &["mkfs.exfat"]).await.is_err() {
        if sentryusb_shell::run_with_timeout(
            Duration::from_secs(600), "apt-get", &["-y", "install", "exfatprogs"],
        ).await.is_err() {
            emitter.progress("WARNING: could not install exfatprogs — falling back to FAT32");
            return Ok(false);
        }
    }

    Ok(true)
}

/// Ensure dosfstools is available.
async fn ensure_vfat_tools() -> Result<()> {
    if sentryusb_shell::run("which", &["mkfs.vfat"]).await.is_err() {
        sentryusb_shell::run_with_timeout(
            Duration::from_secs(600), "apt-get", &["-y", "install", "dosfstools"],
        ).await?;
    }
    Ok(())
}

/// Create all disk images based on config settings. Returns true if any work was performed.
pub async fn create_disk_images(env: &SetupEnv, emitter: &SetupEmitter) -> Result<bool> {
    let use_exfat_cfg = env.get_bool("USE_EXFAT", false);

    // Calculate requested sizes first (before any heavy work) so we can
    // short-circuit when everything already matches.
    let mut sizes: Vec<(String, String, u64, String)> = Vec::new();
    for spec in DRIVE_SPECS {
        let raw = env.get(spec.config_key, "0");
        let size_kb = if raw.contains('%') {
            dehumanize(spec.default_fallback)?
        } else {
            dehumanize(&raw)?
        };
        let filename = format!("{}/{}_disk.bin", BACKINGFILES, spec.name);
        sizes.push((spec.name.to_string(), spec.label.to_string(), size_kb, filename));
    }

    // Reclaim the 4 GB the dedicated wraps disk used to occupy. Runs before
    // the all-match early exit so a pre-migration install gets cleaned up
    // even when the user hasn't changed any sizes.
    let legacy_wraps_path = format!("{}/wraps_disk.bin", BACKINGFILES);
    let legacy_wraps = Path::new(&legacy_wraps_path).exists();
    if legacy_wraps {
        emitter.progress("Removing legacy wraps disk image — using cam drive folders now...");
        let _ = sentryusb_shell::run("umount", &["-d", "/mnt/wraps"]).await;
        purge_legacy_wraps_disk();
    }

    let all_match = sizes.iter().all(|(_, _, sz, f)| image_matches(f, *sz));
    if all_match && !legacy_wraps {
        return Ok(false);
    }

    emitter.begin_phase("disk_images", "Disk images");
    emitter.progress("Creating disk images...");

    let use_exfat = ensure_exfat_tools(use_exfat_cfg, emitter).await?;
    ensure_vfat_tools().await?;

    // Space check. teslausb auto-shrinks because it has no UI to ask
    // the user; we have a UI, so we reject explicitly with a clear
    // breakdown. The wizard pre-flight surfaces the same calculation
    // before submit (see verify::verify_disk_space). Never auto-delete
    // snapshots as a side effect of a settings change.
    let total_requested: u64 = sizes.iter().map(|(_, _, sz, _)| sz).sum();
    let available = available_space_kb().await.unwrap_or(0);
    if total_requested > available {
        let need_gb = (total_requested - available) / 1024 / 1024;
        let req_gb = total_requested / 1024 / 1024;
        let avail_gb = available / 1024 / 1024;
        bail!(
            "Disk images need {} GB but backingfiles has only {} GB free \
             (after safety reserve). Free at least {} GB by deleting \
             snapshots from the snapshot management page, then re-run setup.",
            req_gb, avail_gb, need_gb,
        );
    }

    // Release everything that might be using the images
    release_all_images().await;

    // Create/update each drive
    let cam_changed = !image_matches(&sizes[0].3, sizes[0].2);
    for (name, label, size_kb, filename) in &sizes {
        if image_matches(filename, *size_kb) {
            continue;
        }
        emitter.progress(&format!("Recreating {} drive ({}K)...", name, size_kb));
        create_drive(name, label, *size_kb, use_exfat, emitter).await?;
    }

    // Clean up stale /mutable/TeslaCam symlinks when cam drive was
    // changed/removed — those symlinks point into the old cam_disk and
    // are dangling after the recreate. Snapshots are intentionally NOT
    // touched: they live independently on backingfiles and represent
    // the user's archived footage history. Wiping them on a CAM_SIZE
    // change is the same "I changed a setting, why did I lose data"
    // failure mode the partition wipe used to cause.
    if sizes[0].2 == 0 || cam_changed {
        if Path::new("/mutable/TeslaCam").is_dir() {
            for dir in &["RecentClips", "SavedClips", "SentryClips", "TeslaTrackMode"] {
                let _ = std::fs::remove_dir_all(format!("/mutable/TeslaCam/{}", dir));
            }
            let _ = std::fs::remove_file("/mutable/sentry_files_archived");
        }
    }

    emitter.progress("Disk image creation complete.");
    Ok(true)
}
