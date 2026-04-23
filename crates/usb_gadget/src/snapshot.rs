//! Snapshot management — make and release snapshots of the cam disk.
//!
//! Snapshots use XFS reflink copies (copy-on-write) to create instant,
//! space-efficient copies of the cam disk for archiving without interrupting
//! USB gadget operation.

use std::path::Path;

use anyhow::{bail, Result};
use tracing::info;

const SNAPSHOTS_DIR: &str = "/backingfiles/snapshots";
const CAM_DISK: &str = "/backingfiles/cam_disk.bin";

/// Create a new snapshot of the cam disk.
pub async fn make_snapshot() -> Result<String> {
    let _ = std::fs::create_dir_all(SNAPSHOTS_DIR);

    if !Path::new(CAM_DISK).exists() {
        bail!("cam disk image not found");
    }

    // Find the next snapshot number
    let mut max_num = 0u32;
    if let Ok(entries) = std::fs::read_dir(SNAPSHOTS_DIR) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(num_str) = name.strip_prefix("snap-") {
                if let Ok(num) = num_str.parse::<u32>() {
                    if num > max_num {
                        max_num = num;
                    }
                }
            }
        }
    }
    let snap_num = max_num + 1;
    let snap_name = format!("snap-{:06}", snap_num);
    let snap_dir = format!("{}/{}", SNAPSHOTS_DIR, snap_name);
    std::fs::create_dir_all(&snap_dir)?;

    // Create reflink copy (XFS copy-on-write).
    //
    // `--reflink=auto` is the default intent: use reflink when the FS
    // supports it (XFS with reflink=1, the setup-wizard default),
    // otherwise fall back to a regular full copy. The setup wizard's
    // XFS verify step catches unsupported backing filesystems up front,
    // so in practice reflink always wins here — but `auto` ensures that
    // if a user manually reformatted `/backingfiles` to ext4, snapshots
    // still work (slower + temporarily 2× disk usage, but not a hard
    // failure). Earlier `--reflink=always` hard-errored on ext4, which
    // was the auditor's concern.
    //
    // Timeout bumped to 10 minutes because the fallback path has to
    // write the full cam image; at 32 GB and ~100 MB/s sustained that's
    // ~5 minutes, leaving a generous margin for slower storage.
    let snap_file = format!("{}/snap.bin", snap_dir);
    let result = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(600),
        "cp",
        &["--reflink=auto", CAM_DISK, &snap_file],
    ).await;

    match result {
        Ok(_) => {
            info!("Created snapshot: {} -> {}", snap_name, snap_file);
            Ok(snap_name)
        }
        Err(e) => {
            // Clean up on failure
            let _ = std::fs::remove_dir_all(&snap_dir);
            bail!("Failed to create snapshot: {}", e)
        }
    }
}

/// Release (delete) a snapshot.
pub async fn release_snapshot(snap_name: &str) -> Result<()> {
    // Validate name to prevent path traversal
    if snap_name.contains("..") || snap_name.contains('/') {
        bail!("invalid snapshot name");
    }

    let snap_dir = format!("{}/{}", SNAPSHOTS_DIR, snap_name);
    if !Path::new(&snap_dir).exists() {
        bail!("snapshot not found: {}", snap_name);
    }

    // Unmount if mounted
    let mnt_dir = format!("{}/mnt", snap_dir);
    if Path::new(&mnt_dir).exists() {
        let _ = sentryusb_shell::run("umount", &[&mnt_dir]).await;
    }

    std::fs::remove_dir_all(&snap_dir)?;
    info!("Released snapshot: {}", snap_name);
    Ok(())
}

/// List all snapshots.
pub fn list_snapshots() -> Vec<String> {
    let mut snaps = Vec::new();
    if let Ok(entries) = std::fs::read_dir(SNAPSHOTS_DIR) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("snap-") && entry.path().is_dir() {
                snaps.push(name);
            }
        }
    }
    snaps.sort();
    snaps
}
