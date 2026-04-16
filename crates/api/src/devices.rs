//! Block device listing. Returns removable/external block devices suitable
//! for use as DATA_DRIVE. Critical: the OS drive is always excluded so the
//! user cannot accidentally wipe the boot/root disk. Port of server/api/devices.go.

use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;

use crate::router::AppState;

#[derive(Serialize)]
struct BlockDev {
    path: String,
    name: String,
    size_gb: String,
    model: String,
}

/// GET /api/system/block-devices
pub async fn list_block_devices(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let devices = enumerate_candidate_devices().await;
    (StatusCode::OK, Json(serde_json::to_value(&devices).unwrap_or_else(|_| serde_json::json!([]))))
}

async fn enumerate_candidate_devices() -> Vec<BlockDev> {
    let mut out = Vec::new();

    let entries = match std::fs::read_dir("/sys/block") {
        Ok(e) => e,
        Err(_) => return out,
    };

    let excluded = excluded_devices();

    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("loop") || name.starts_with("ram") || name.starts_with("zram") {
            continue;
        }
        if excluded.contains(&name) {
            continue;
        }

        let dev_path = format!("/dev/{}", name);

        let size_gb = match std::fs::read_to_string(format!("/sys/block/{}/size", name)) {
            Ok(s) => {
                let sectors: u64 = s.trim().parse().unwrap_or(0);
                let gb = (sectors as f64 * 512.0) / (1024.0 * 1024.0 * 1024.0);
                if gb < 0.5 {
                    continue;
                }
                format!("{:.1}", gb)
            }
            Err(_) => String::new(),
        };

        let model = std::fs::read_to_string(format!("/sys/block/{}/device/model", name))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let mut label = name.clone();
        if !model.is_empty() {
            label = format!("{} ({})", label, model);
        }
        if !size_gb.is_empty() {
            label = format!("{} - {} GB", label, size_gb);
        }

        out.push(BlockDev {
            path: dev_path,
            name: label,
            size_gb,
            model,
        });
    }
    out
}

/// Build the set of /sys/block device names whose drives host system mount
/// points. Always excludes `mmcblk0` (the onboard SD slot).
fn excluded_devices() -> HashSet<String> {
    let mut excluded: HashSet<String> = HashSet::new();
    excluded.insert("mmcblk0".to_string());

    let mounts = match std::fs::read_to_string("/proc/mounts") {
        Ok(s) => s,
        Err(_) => return excluded,
    };

    const SYSTEM_MOUNTS: &[&str] = &["/", "/boot", "/boot/firmware"];

    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let source = match fields.next() {
            Some(s) if s.starts_with("/dev/") => s,
            _ => continue,
        };
        let mountpoint = match fields.next() {
            Some(m) => m,
            None => continue,
        };
        if !SYSTEM_MOUNTS.contains(&mountpoint) {
            continue;
        }

        // Resolve symlinks (e.g. /dev/disk/by-partuuid/xxx -> /dev/sda1)
        let resolved = std::fs::canonicalize(source)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| source.to_string());
        let dev = resolved.strip_prefix("/dev/").unwrap_or(&resolved);

        // Strip partition suffix to get the parent disk name.
        if dev.contains("mmcblk") || dev.contains("nvme") || dev.contains("loop") {
            // mmcblk/nvme/loop style: partition suffix is "pN"
            if let Some(idx) = dev.rfind('p') {
                if idx > 0 {
                    excluded.insert(dev[..idx].to_string());
                }
            }
        } else {
            // sd-style: partition suffix is just trailing digits
            let trimmed: String = dev
                .chars()
                .rev()
                .skip_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if !trimmed.is_empty() {
                excluded.insert(trimmed);
            }
        }
        // Also exclude the partition device name itself.
        excluded.insert(dev.to_string());
    }

    excluded
}
