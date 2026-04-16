//! Status, storage, config, and WiFi API handlers.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;

use crate::router::AppState;

// ---------------------------------------------------------------------------
// GET /api/status
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct PiStatus {
    cpu_temp: String,
    num_snapshots: String,
    snapshot_oldest: String,
    snapshot_newest: String,
    total_space: String,
    free_space: String,
    uptime: String,
    drives_active: String,
    wifi_ssid: String,
    wifi_freq: String,
    wifi_strength: String,
    wifi_ip: String,
    ether_ip: String,
    ether_speed: String,
    device_suffix: String,
    sbc_model: String,
}

pub async fn get_status(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut s = PiStatus {
        cpu_temp: String::new(),
        num_snapshots: "0".into(),
        snapshot_oldest: String::new(),
        snapshot_newest: String::new(),
        total_space: String::new(),
        free_space: String::new(),
        uptime: String::new(),
        drives_active: "no".into(),
        wifi_ssid: String::new(),
        wifi_freq: String::new(),
        wifi_strength: String::new(),
        wifi_ip: String::new(),
        ether_ip: String::new(),
        ether_speed: String::new(),
        device_suffix: String::new(),
        sbc_model: String::new(),
    };

    // Device suffix from machine-id
    if let Ok(mid) = std::fs::read_to_string("/etc/machine-id") {
        let mid = mid.trim();
        if mid.len() >= 4 {
            s.device_suffix = mid[mid.len() - 4..].to_uppercase();
        }
    }

    // SBC model
    s.sbc_model = get_sbc_model();

    // CPU temperature
    if let Ok(data) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        s.cpu_temp = data.trim().to_string();
    }

    // Uptime
    if let Ok(data) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs) = data.split_whitespace().next() {
            s.uptime = secs.to_string();
        }
    }

    // USB gadget status
    if std::path::Path::new("/sys/kernel/config/usb_gadget/sentryusb").exists() {
        s.drives_active = "yes".into();
    }

    // Snapshots
    let snapshots = find_snapshots();
    s.num_snapshots = snapshots.len().to_string();
    if !snapshots.is_empty() {
        if let Ok(meta) = std::fs::metadata(&snapshots[0]) {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    s.snapshot_oldest = d.as_secs().to_string();
                }
            }
        }
        if let Ok(meta) = std::fs::metadata(snapshots.last().unwrap()) {
            if let Ok(t) = meta.modified() {
                if let Ok(d) = t.duration_since(std::time::UNIX_EPOCH) {
                    s.snapshot_newest = d.as_secs().to_string();
                }
            }
        }
    }

    // Disk space
    if let Ok(out) = sentryusb_shell::run("stat", &["--file-system", "--format=%b %S %f", "/backingfiles/."]).await {
        let parts: Vec<&str> = out.trim().split_whitespace().collect();
        if parts.len() >= 3 {
            if let (Ok(blocks), Ok(bs), Ok(free)) = (
                parts[0].parse::<u64>(),
                parts[1].parse::<u64>(),
                parts[2].parse::<u64>(),
            ) {
                s.total_space = (blocks * bs).to_string();
                s.free_space = (free * bs).to_string();
            }
        }
    }

    // WiFi info
    let wifi_dev = find_net_device("wl*");
    if !wifi_dev.is_empty() {
        if let Ok(out) = sentryusb_shell::run("iwgetid", &["-r", &wifi_dev]).await {
            s.wifi_ssid = out.trim().to_string();
        }
        if let Ok(out) = sentryusb_shell::run("iwgetid", &["-r", "-f", &wifi_dev]).await {
            s.wifi_freq = out.trim().to_string();
        }
        if let Ok(out) = sentryusb_shell::run("iwconfig", &[&wifi_dev]).await {
            for line in out.lines() {
                if let Some(after) = line.split("Link Quality=").nth(1) {
                    if let Some(qual) = after.split_whitespace().next() {
                        s.wifi_strength = qual.to_string();
                    }
                }
            }
        }
        if let Ok(out) = sentryusb_shell::run("ip", &["-4", "addr", "show", &wifi_dev]).await {
            for line in out.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(addr) = trimmed.split_whitespace().nth(1) {
                        s.wifi_ip = addr.split('/').next().unwrap_or("").to_string();
                    }
                }
            }
        }
    }

    // Ethernet info
    let mut eth_dev = find_net_device("eth*");
    if eth_dev.is_empty() {
        eth_dev = find_net_device("en*");
    }
    if !eth_dev.is_empty() {
        if let Ok(out) = sentryusb_shell::run("ip", &["-4", "addr", "show", &eth_dev]).await {
            for line in out.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(addr) = trimmed.split_whitespace().nth(1) {
                        s.ether_ip = addr.split('/').next().unwrap_or("").to_string();
                    }
                }
            }
        }
        if let Ok(out) = sentryusb_shell::run("ethtool", &[&eth_dev]).await {
            for line in out.lines() {
                if line.contains("Speed:") {
                    if let Some(val) = line.split(':').nth(1) {
                        s.ether_speed = val.trim().to_string();
                    }
                }
            }
        }
    }

    (StatusCode::OK, Json(serde_json::to_value(s).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// GET /api/status/storage
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StorageBreakdown {
    cam_size: i64,
    music_size: i64,
    lightshow_size: i64,
    boombox_size: i64,
    wraps_size: i64,
    snapshots_size: i64,
    total_space: i64,
    free_space: i64,
}

pub async fn get_storage_breakdown(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut sb = StorageBreakdown {
        cam_size: disk_usage("/backingfiles/cam_disk.bin"),
        music_size: disk_usage("/backingfiles/music_disk.bin"),
        lightshow_size: disk_usage("/backingfiles/lightshow_disk.bin"),
        boombox_size: disk_usage("/backingfiles/boombox_disk.bin"),
        wraps_size: disk_usage("/backingfiles/wraps_disk.bin"),
        snapshots_size: 0,
        total_space: 0,
        free_space: 0,
    };

    if let Ok(out) = sentryusb_shell::run("stat", &["--file-system", "--format=%b %S %f", "/backingfiles/."]).await {
        let parts: Vec<&str> = out.trim().split_whitespace().collect();
        if parts.len() >= 3 {
            if let (Ok(blocks), Ok(bs), Ok(free)) = (
                parts[0].parse::<i64>(),
                parts[1].parse::<i64>(),
                parts[2].parse::<i64>(),
            ) {
                sb.total_space = blocks * bs;
                sb.free_space = free * bs;
            }
        }
    }

    // Derive snapshot usage by subtraction (reflink clones make du unreliable)
    let disk_images = sb.cam_size + sb.music_size + sb.lightshow_size + sb.boombox_size + sb.wraps_size;
    let used = sb.total_space - sb.free_space;
    sb.snapshots_size = (used - disk_images).max(0);

    (StatusCode::OK, Json(serde_json::to_value(sb).unwrap_or_default()))
}

// ---------------------------------------------------------------------------
// GET /api/config
// ---------------------------------------------------------------------------

pub async fn get_config(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let has = |p: &str| -> String {
        if std::path::Path::new(p).exists() { "yes".into() } else { "no".into() }
    };

    let mut uses_ble = "no".to_string();
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _)) = sentryusb_config::parse_file(config_path) {
        if active.contains_key("TESLA_BLE_VIN") {
            uses_ble = "yes".to_string();
        }
    }

    (StatusCode::OK, Json(serde_json::json!({
        "has_cam": has("/backingfiles/cam_disk.bin"),
        "has_music": has("/backingfiles/music_disk.bin"),
        "has_lightshow": has("/backingfiles/lightshow_disk.bin"),
        "has_boombox": has("/backingfiles/boombox_disk.bin"),
        "has_wraps": has("/backingfiles/wraps_disk.bin"),
        "uses_ble": uses_ble,
    })))
}

// ---------------------------------------------------------------------------
// GET /api/wifi
// ---------------------------------------------------------------------------

pub async fn get_wifi_config(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut ssid = String::new();
    let mut connected = false;
    let mut source = String::new();

    // 1. Try nmcli
    if let Ok(out) = sentryusb_shell::run("nmcli", &["-t", "-f", "active,ssid", "dev", "wifi"]).await {
        for line in out.lines() {
            if line.starts_with("yes:") {
                ssid = line.strip_prefix("yes:").unwrap_or("").to_string();
                connected = true;
                source = "networkmanager".into();
                break;
            }
        }
    }

    // 2. Fallback: iwgetid
    if ssid.is_empty() {
        if let Ok(out) = sentryusb_shell::run("iwgetid", &["-r"]).await {
            let s = out.trim();
            if !s.is_empty() {
                ssid = s.to_string();
                connected = true;
                source = "iwgetid".into();
            }
        }
    }

    // 3. Fallback: wpa_supplicant.conf
    if ssid.is_empty() {
        for p in &[
            "/etc/wpa_supplicant/wpa_supplicant.conf",
            "/boot/firmware/wpa_supplicant.conf",
            "/boot/wpa_supplicant.conf",
        ] {
            if let Ok(data) = std::fs::read_to_string(p) {
                for line in data.lines() {
                    let trimmed = line.trim();
                    if let Some(val) = trimmed.strip_prefix("ssid=") {
                        let val = val.trim_matches('"');
                        if !val.is_empty() {
                            ssid = val.to_string();
                            source = "wpa_supplicant".into();
                            break;
                        }
                    }
                }
                if !ssid.is_empty() {
                    break;
                }
            }
        }
    }

    // 4. Config SSID
    let mut config_ssid = String::new();
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("SSID") {
            config_ssid = v.clone();
        }
    }
    // Filter placeholder values
    let lower = config_ssid.to_lowercase();
    if matches!(lower.as_str(), "your_ssid" | "yourssid" | "your_wifi" | "ssid" | "your_network" | "") {
        config_ssid.clear();
    }

    // WLAN country
    let mut wlan_country = String::new();
    if let Ok(out) = sentryusb_shell::run("iw", &["reg", "get"]).await {
        for line in out.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("country") {
                let parts: Vec<&str> = trimmed.splitn(3, ' ').collect();
                if parts.len() >= 2 {
                    wlan_country = parts[1].trim_end_matches(':').to_string();
                }
                break;
            }
        }
    }

    (StatusCode::OK, Json(serde_json::json!({
        "current": {
            "ssid": ssid,
            "connected": connected,
            "source": source,
        },
        "config_ssid": config_ssid,
        "wlan_country": wlan_country,
    })))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_snapshots() -> Vec<String> {
    let mut snaps = Vec::new();
    let base = std::path::Path::new("/backingfiles/snapshots/");
    if base.exists() {
        if let Ok(entries) = walkdir(base) {
            for path in entries {
                if path.ends_with("snap.bin") {
                    snaps.push(path);
                }
            }
        }
    }
    snaps.sort();
    snaps
}

fn walkdir(dir: &std::path::Path) -> std::io::Result<Vec<String>> {
    let mut result = Vec::new();
    if !dir.is_dir() {
        return Ok(result);
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            result.extend(walkdir(&path)?);
        } else if let Some(s) = path.to_str() {
            result.push(s.to_string());
        }
    }
    Ok(result)
}

fn find_net_device(pattern: &str) -> String {
    let prefix = pattern.trim_end_matches('*');
    if let Ok(entries) = std::fs::read_dir("/sys/class/net/") {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(prefix) {
                    return name.to_string();
                }
            }
        }
    }
    String::new()
}

fn disk_usage(path: &str) -> i64 {
    // On Linux, use stat to get st_blocks * 512 for actual disk usage
    // (handles sparse files and reflink copies correctly)
    if let Ok(out) = std::process::Command::new("stat")
        .args(["--format=%b", path])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Ok(blocks) = s.trim().parse::<i64>() {
                return blocks * 512;
            }
        }
    }
    0
}

/// Get SBC model from device tree.
pub fn get_sbc_model() -> String {
    for p in &["/proc/device-tree/model", "/sys/firmware/devicetree/base/model"] {
        if let Ok(data) = std::fs::read(p) {
            return String::from_utf8_lossy(&data)
                .trim_end_matches('\0')
                .trim()
                .to_string();
        }
    }
    "unknown".to_string()
}
