//! Free space management for the backing filesystem.
//!
//! Monitors disk usage and releases old snapshots when space runs low.

use std::path::Path;

use anyhow::Result;
use tracing::{info, warn};

const BACKINGFILES: &str = "/backingfiles";
const MIN_FREE_PCT: f64 = 5.0; // Minimum free space percentage before cleanup

/// Check if free space is below the threshold and release old snapshots if needed.
pub async fn manage_free_space() -> Result<()> {
    let (total, free) = get_space(BACKINGFILES)?;
    if total == 0 {
        return Ok(());
    }

    let free_pct = (free as f64 / total as f64) * 100.0;
    info!("Disk space: {:.1}% free ({} / {} bytes)", free_pct, free, total);

    if free_pct >= MIN_FREE_PCT {
        return Ok(());
    }

    info!("Free space below {}%, releasing old snapshots...", MIN_FREE_PCT);

    let snapshots = super::snapshot::list_snapshots();
    if snapshots.is_empty() {
        warn!("No snapshots to release, disk is full");
        return Ok(());
    }

    // Release oldest snapshots first until we're above the threshold
    for snap in &snapshots {
        if let Err(e) = super::snapshot::release_snapshot(snap).await {
            warn!("Failed to release {}: {}", snap, e);
            continue;
        }

        let (_, new_free) = get_space(BACKINGFILES)?;
        let new_pct = (new_free as f64 / total as f64) * 100.0;
        info!("After releasing {}: {:.1}% free", snap, new_pct);

        if new_pct >= MIN_FREE_PCT {
            break;
        }
    }

    Ok(())
}

/// Get total and free bytes for a filesystem.
fn get_space(path: &str) -> Result<(u64, u64)> {
    let output = std::process::Command::new("stat")
        .args(["--file-system", "--format=%b %S %f", path])
        .output()?;

    if !output.status.success() {
        anyhow::bail!("stat failed for {}", path);
    }

    let s = String::from_utf8_lossy(&output.stdout);
    let parts: Vec<&str> = s.trim().split_whitespace().collect();
    if parts.len() >= 3 {
        let blocks: u64 = parts[0].parse().unwrap_or(0);
        let block_size: u64 = parts[1].parse().unwrap_or(0);
        let free_blocks: u64 = parts[2].parse().unwrap_or(0);
        return Ok((blocks * block_size, free_blocks * block_size));
    }

    Ok((0, 0))
}
