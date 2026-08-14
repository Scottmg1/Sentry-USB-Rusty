//! Status, storage, config, and WiFi API handlers.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::router::AppState;

// Cache shell-derived network data for 10 s and filesystem capacity for 5 s.

#[derive(Clone, Default)]
struct CachedNetwork {
    wifi_ssid: String,
    /// Frequency in Hz, empty when unavailable.
    wifi_freq: String,
    wifi_ip: String,
    ether_ip: String,
    ether_speed: String,
    /// Cached device names; signal and throughput remain live.
    wifi_dev: String,
    eth_dev: String,
}

/// Reads `(link_quality/70, signal_dbm)` directly from `/proc/net/wireless`.
fn read_wireless_quality(dev: &str) -> Option<(String, Option<i32>)> {
    let data = std::fs::read_to_string("/proc/net/wireless").ok()?;
    for line in data.lines().skip(2) {
        let line = line.trim_start();
        let prefix = format!("{}:", dev);
        if !line.starts_with(&prefix) {
            continue;
        }
        let cols: Vec<&str> = line[prefix.len()..].split_whitespace().collect();
        if cols.len() < 3 {
            return None;
        }
        // Kernel fixed-point fields may end in a period.
        let link = cols[1].trim_end_matches('.').parse::<u32>().ok()?;
        let level = cols[2].trim_end_matches('.').parse::<i32>().ok();
        return Some((format!("{}/70", link), level));
    }
    None
}

#[derive(Clone, Copy, Default)]
struct CachedStorage {
    total_space: u64,
    free_space: u64,
}

struct StatusCache {
    network: Mutex<Option<(CachedNetwork, Instant)>>,
    storage: Mutex<Option<(CachedStorage, Instant)>>,
}

static STATUS_CACHE: OnceLock<StatusCache> = OnceLock::new();

fn cache() -> &'static StatusCache {
    STATUS_CACHE.get_or_init(|| StatusCache {
        network: Mutex::new(None),
        storage: Mutex::new(None),
    })
}

const NETWORK_TTL: Duration = Duration::from_secs(10);
const STORAGE_TTL: Duration = Duration::from_secs(5);

/// Returns total and free bytes from `statvfs` without spawning `stat`.
fn statvfs_backing_files() -> Option<(u64, u64)> {
    let path = std::ffi::CString::new("/backingfiles/.").ok()?;
    // SAFETY: `statvfs` initializes this zeroed struct on a successful return.
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::statvfs(path.as_ptr(), &mut buf) };
    if r != 0 {
        return None;
    }
    let frsize = buf.f_frsize as u64;
    let total = (buf.f_blocks as u64).saturating_mul(frsize);
    let free = (buf.f_bfree as u64).saturating_mul(frsize);
    Some((total, free))
}

async fn cached_storage() -> CachedStorage {
    {
        let guard = cache().storage.lock().unwrap();
        if let Some((info, when)) = &*guard {
            if when.elapsed() < STORAGE_TTL {
                return *info;
            }
        }
    }
    let info = statvfs_backing_files()
        .map(|(t, f)| CachedStorage { total_space: t, free_space: f })
        .unwrap_or_default();
    let mut guard = cache().storage.lock().unwrap();
    *guard = Some((info, Instant::now()));
    info
}


#[derive(Clone)]
pub struct NetSample {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub taken_at: Instant,
}

pub type NetSampler = Arc<Mutex<HashMap<String, NetSample>>>;


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
    /// Host-link state; unlike `drives_active`, reflects actual enumeration.
    udc_state: String,
    /// Seconds since the last `cam_disk.bin` write, or -1 when unknown.
    cam_last_write_secs: i64,
    wifi_ssid: String,
    /// Wi-Fi frequency in Hz, empty when unavailable.
    wifi_freq: String,
    wifi_strength: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    wifi_signal_dbm: Option<i32>,
    wifi_ip: String,
    ether_ip: String,
    ether_speed: String,
    sbc_model: String,
    fan_speed: String,
    wifi_rx_bps: u64,
    wifi_tx_bps: u64,
    ether_rx_bps: u64,
    ether_tx_bps: u64,
    /// Final hostname segment used as the stable device suffix.
    device_suffix: String,
}

pub async fn get_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Snapshot and backing-file metadata can block during archive I/O.
    let mut s = match tokio::task::spawn_blocking(status_fs_snapshot).await {
        Ok(s) => s,
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("status task: {}", e),
            );
        }
    };

    // Capacity uses a cached `statvfs` call.
    let storage = cached_storage().await;
    if storage.total_space > 0 {
        s.total_space = storage.total_space.to_string();
        s.free_space = storage.free_space.to_string();
    }

    // Cache stable network identity; keep signal and throughput live.
    let net = cached_network().await;
    s.wifi_ssid = net.wifi_ssid;
    s.wifi_freq = net.wifi_freq;
    s.wifi_ip = net.wifi_ip;
    s.ether_ip = net.ether_ip;
    s.ether_speed = net.ether_speed;
    if !net.wifi_dev.is_empty() {
        if let Some((strength, dbm)) = read_wireless_quality(&net.wifi_dev) {
            s.wifi_strength = strength;
            s.wifi_signal_dbm = dbm;
        }
        let (rx, tx) = compute_throughput(&state.net_sampler, &net.wifi_dev);
        s.wifi_rx_bps = rx;
        s.wifi_tx_bps = tx;
    }
    if !net.eth_dev.is_empty() {
        let (rx, tx) = compute_throughput(&state.net_sampler, &net.eth_dev);
        s.ether_rx_bps = rx;
        s.ether_tx_bps = tx;
    }

    (StatusCode::OK, Json(serde_json::to_value(s).unwrap_or_default()))
}

/// Collects direct filesystem status on the blocking pool.
fn status_fs_snapshot() -> PiStatus {
    let mut s = PiStatus {
        cpu_temp: String::new(),
        num_snapshots: "0".into(),
        snapshot_oldest: String::new(),
        snapshot_newest: String::new(),
        total_space: String::new(),
        free_space: String::new(),
        uptime: String::new(),
        drives_active: "no".into(),
        udc_state: String::new(),
        cam_last_write_secs: -1,
        wifi_ssid: String::new(),
        wifi_freq: String::new(),
        wifi_strength: String::new(),
        wifi_signal_dbm: None,
        wifi_ip: String::new(),
        ether_ip: String::new(),
        ether_speed: String::new(),
        sbc_model: String::new(),
        fan_speed: String::new(),
        wifi_rx_bps: 0,
        wifi_tx_bps: 0,
        ether_rx_bps: 0,
        ether_tx_bps: 0,
        device_suffix: read_device_suffix(),
    };

    s.sbc_model = get_sbc_model();

    if let Ok(data) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        s.cpu_temp = data.trim().to_string();
    }

    s.fan_speed = read_fan_speed();

    if let Ok(data) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs) = data.split_whitespace().next() {
            s.uptime = secs.to_string();
        }
    }

    // Active requires both UDC binding and a configured backing file.
    if sentryusb_gadget::is_active() {
        s.drives_active = "yes".into();
    }
    s.udc_state = read_udc_state();
    s.cam_last_write_secs = cam_last_write_secs();

    // Slot numbers are not time-monotonic, so derive the range from mtimes.
    let scan = scan_snapshots(std::path::Path::new("/backingfiles/snapshots/"));
    s.num_snapshots = scan.count.to_string();
    if let Some(t) = scan.oldest_unix {
        s.snapshot_oldest = t.to_string();
    }
    if let Some(t) = scan.newest_unix {
        s.snapshot_newest = t.to_string();
    }

    s
}

/// Returns shared cached network data, refreshing once when stale.
async fn cached_network() -> CachedNetwork {
    {
        let guard = cache().network.lock().unwrap();
        if let Some((info, when)) = &*guard {
            if when.elapsed() < NETWORK_TTL {
                return info.clone();
            }
        }
    }
    let info = compute_network_info().await;
    let mut guard = cache().network.lock().unwrap();
    *guard = Some((info.clone(), Instant::now()));
    info
}

/// Fetches shell-derived Wi-Fi and Ethernet properties.
async fn compute_network_info() -> CachedNetwork {
    let mut info = CachedNetwork::default();

    // Skip potentially blocking Wi-Fi commands when the interface is down.
    let wifi_dev = find_net_device("wl*");
    if !wifi_dev.is_empty() && iface_is_up(&wifi_dev) {
        info.wifi_dev = wifi_dev.clone();
        let ssid_args = ["-r", wifi_dev.as_str()];
        let ip_args = ["-4", "addr", "show", wifi_dev.as_str()];
        // `iw` reports frequency in MHz.
        let iw_args = ["dev", wifi_dev.as_str(), "link"];
        let (ssid_r, ip_r, iw_r) = tokio::join!(
            sentryusb_shell::run("iwgetid", &ssid_args),
            sentryusb_shell::run("ip", &ip_args),
            sentryusb_shell::run("iw", &iw_args),
        );
        if let Ok(out) = ssid_r {
            info.wifi_ssid = out.trim().to_string();
        }
        if let Ok(out) = ip_r {
            for line in out.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(addr) = trimmed.split_whitespace().nth(1) {
                        info.wifi_ip = addr.split('/').next().unwrap_or("").to_string();
                    }
                }
            }
        }
        if let Ok(out) = iw_r {
            for line in out.lines() {
                let trimmed = line.trim();
                // Convert MHz to the API's Hz-string format.
                if let Some(rest) = trimmed.strip_prefix("freq:") {
                    if let Ok(mhz) = rest.trim().parse::<u64>() {
                        info.wifi_freq = (mhz * 1_000_000).to_string();
                        break;
                    }
                }
            }
        }
    }

    // Apply the same blocking-command guard to Ethernet.
    let mut eth_dev = find_net_device("eth*");
    if eth_dev.is_empty() {
        eth_dev = find_net_device("en*");
    }
    if !eth_dev.is_empty() && iface_is_up(&eth_dev) {
        info.eth_dev = eth_dev.clone();
        let eth_ip_args = ["-4", "addr", "show", eth_dev.as_str()];
        let eth_tool_args = [eth_dev.as_str()];
        let (ip_r, ethtool_r) = tokio::join!(
            sentryusb_shell::run("ip", &eth_ip_args),
            sentryusb_shell::run("ethtool", &eth_tool_args),
        );
        if let Ok(out) = ip_r {
            for line in out.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("inet ") {
                    if let Some(addr) = trimmed.split_whitespace().nth(1) {
                        info.ether_ip = addr.split('/').next().unwrap_or("").to_string();
                    }
                }
            }
        }
        if let Ok(out) = ethtool_r {
            for line in out.lines() {
                if line.contains("Speed:") {
                    if let Some(val) = line.split(':').nth(1) {
                        info.ether_speed = val.trim().to_string();
                    }
                }
            }
        }
    }

    info
}


#[derive(Serialize)]
struct StorageBreakdown {
    cam_size: i64,
    music_size: i64,
    lightshow_size: i64,
    boombox_size: i64,
    snapshots_size: i64,
    total_space: i64,
    free_space: i64,
}

pub async fn get_storage_breakdown(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (cam, music, lightshow, boombox) = tokio::join!(
        disk_usage("/backingfiles/cam_disk.bin"),
        disk_usage("/backingfiles/music_disk.bin"),
        disk_usage("/backingfiles/lightshow_disk.bin"),
        disk_usage("/backingfiles/boombox_disk.bin"),
    );
    let mut sb = StorageBreakdown {
        cam_size: cam,
        music_size: music,
        lightshow_size: lightshow,
        boombox_size: boombox,
        snapshots_size: 0,
        total_space: 0,
        free_space: 0,
    };

    // This slower-polled endpoint reads `statvfs` directly without caching.
    if let Some((total, free)) = statvfs_backing_files() {
        sb.total_space = total as i64;
        sb.free_space = free as i64;
    }

    // Reflink clones make `du` unsuitable; derive snapshot usage by subtraction.
    let disk_images = sb.cam_size + sb.music_size + sb.lightshow_size + sb.boombox_size;
    let used = sb.total_space - sb.free_space;
    sb.snapshots_size = (used - disk_images).max(0);

    (StatusCode::OK, Json(serde_json::to_value(sb).unwrap_or_default()))
}


pub async fn get_config(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let has = |p: &str| -> String {
        if std::path::Path::new(p).exists() { "yes".into() } else { "no".into() }
    };

    // Surface BLE settings after explicit enablement or any existing BLE state.
    let uses_ble = if crate::ble::is_ble_enabled()
        || std::path::Path::new("/root/.ble/key_private.pem").exists()
        || std::path::Path::new("/root/.ble/paired").exists()
    {
        "yes".to_string()
    } else {
        "no".to_string()
    };

    (StatusCode::OK, Json(serde_json::json!({
        "has_cam": has("/backingfiles/cam_disk.bin"),
        "has_music": has("/backingfiles/music_disk.bin"),
        "has_lightshow": has("/backingfiles/lightshow_disk.bin"),
        "has_boombox": has("/backingfiles/boombox_disk.bin"),
        "uses_ble": uses_ble,
    })))
}


pub async fn get_wifi_config(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut ssid = String::new();
    let mut connected = false;
    let mut source = String::new();

    // Prefer the kernel association query; `nmcli` may trigger a scan.
    if let Ok(out) = sentryusb_shell::run("iwgetid", &["-r"]).await {
        let s = out.trim();
        if !s.is_empty() {
            ssid = s.to_string();
            connected = true;
            source = "iwgetid".into();
        }
    }

    // Fall back to NetworkManager.
    if ssid.is_empty() {
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
    }

    // Fall back to configured wpa_supplicant networks.
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

    // Also expose the application-configured SSID.
    let mut config_ssid = String::new();
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("SSID") {
            config_ssid = v.clone();
        }
    }
    // Omit setup placeholders.
    let lower = config_ssid.to_lowercase();
    if matches!(lower.as_str(), "your_ssid" | "yourssid" | "your_wifi" | "ssid" | "your_network" | "") {
        config_ssid.clear();
    }

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

/// One-pass snapshot summary: count plus the oldest/newest `snap.bin`
/// mtimes (unix seconds).
struct SnapshotScan {
    count: usize,
    oldest_unix: Option<u64>,
    newest_unix: Option<u64>,
}

/// Scans only top-level numeric snapshot directories, avoiding autofs symlinks.
fn scan_snapshots(base: &std::path::Path) -> SnapshotScan {
    let mut scan = SnapshotScan { count: 0, oldest_unix: None, newest_unix: None };
    let Ok(entries) = std::fs::read_dir(base) else {
        return scan;
    };
    for entry in entries.flatten() {
        // `file_type` does not follow snapshot autofs symlinks.
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        // Match the numeric directory contract used by listing and eviction.
        if !entry
            .file_name()
            .to_string_lossy()
            .strip_prefix("snap-")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let snap_bin = entry.path().join("snap.bin");
        // Do not follow an unexpected `snap.bin` symlink.
        if let Ok(meta) = std::fs::symlink_metadata(&snap_bin) {
            scan.count += 1;
            // Slot names are not time-monotonic.
            if let Some(t) = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
            {
                scan.oldest_unix = Some(scan.oldest_unix.map_or(t, |o| o.min(t)));
                scan.newest_unix = Some(scan.newest_unix.map_or(t, |n| n.max(t)));
            }
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_snapshots_range_is_mtime_min_max_not_name_order() {
        let base = std::env::temp_dir().join(format!(
            "sentryusb-snap-scan-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let fixtures = [
            ("snap-000000", 1_785_300_000u64),
            ("snap-000413", 1_786_200_000u64),
            ("snap-000414", 1_784_600_000u64),
        ];
        for (name, mtime) in fixtures {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("snap.bin");
            let f = std::fs::File::create(&bin).unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
            f.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
        }
        std::fs::File::create(base.join("stray.txt")).unwrap();
        std::fs::create_dir_all(base.join("not-a-snap")).unwrap();
        let decoy = base.join("snap-backup-old");
        std::fs::create_dir_all(&decoy).unwrap();
        let df = std::fs::File::create(decoy.join("snap.bin")).unwrap();
        df.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000)),
        )
        .unwrap();

        let scan = scan_snapshots(&base);
        let _ = std::fs::remove_dir_all(&base);

        assert_eq!(scan.count, 3);
        assert_eq!(scan.oldest_unix, Some(1_784_600_000));
        assert_eq!(scan.newest_unix, Some(1_786_200_000));
    }

    #[test]
    fn scan_snapshots_empty_dir_reports_none() {
        let base = std::env::temp_dir().join(format!(
            "sentryusb-snap-scan-empty-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let scan = scan_snapshots(&base);
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(scan.count, 0);
        assert_eq!(scan.oldest_unix, None);
        assert_eq!(scan.newest_unix, None);
    }
}

fn read_fan_speed() -> String {
    let base = std::path::Path::new("/sys/devices/platform/cooling_fan/hwmon");
    let Ok(entries) = std::fs::read_dir(base) else {
        return String::new();
    };
    for entry in entries.flatten() {
        let candidate = entry.path().join("fan1_input");
        if let Ok(data) = std::fs::read_to_string(&candidate) {
            return data.trim().to_string();
        }
    }
    String::new()
}

/// Reads the first UDC's host-link state, shared with the health check.
pub(crate) fn read_udc_state() -> String {
    let Ok(entries) = std::fs::read_dir("/sys/class/udc") else {
        return String::new();
    };
    for entry in entries.flatten() {
        if let Ok(data) = std::fs::read_to_string(entry.path().join("state")) {
            return data.trim().to_string();
        }
    }
    String::new()
}

/// Seconds since the last write to cam_disk.bin, -1 when unknown.
fn cam_last_write_secs() -> i64 {
    let Ok(meta) = std::fs::metadata("/backingfiles/cam_disk.bin") else {
        return -1;
    };
    meta.modified()
        .ok()
        .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1)
}

/// Returns the final hostname segment when it is a plausible device suffix.
fn read_device_suffix() -> String {
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if let Some(idx) = hostname.rfind('-') {
        let suffix = &hostname[idx + 1..];
        if !suffix.is_empty() && suffix.len() <= 8 {
            return suffix.to_string();
        }
    }
    String::new()
}

fn read_net_bytes(dev: &str, stat: &str) -> Option<u64> {
    let path = format!("/sys/class/net/{}/statistics/{}", dev, stat);
    std::fs::read_to_string(&path).ok()?.trim().parse::<u64>().ok()
}

fn compute_throughput(sampler: &NetSampler, dev: &str) -> (u64, u64) {
    let Some(rx_now) = read_net_bytes(dev, "rx_bytes") else { return (0, 0); };
    let Some(tx_now) = read_net_bytes(dev, "tx_bytes") else { return (0, 0); };
    let now = Instant::now();
    let mut map = sampler.lock().unwrap_or_else(|e| e.into_inner());
    let result = if let Some(prev) = map.get(dev) {
        let elapsed = now.duration_since(prev.taken_at).as_secs_f64();
        if elapsed < 0.1 {
            (0, 0)
        } else {
            let rx_bps = ((rx_now.saturating_sub(prev.rx_bytes) as f64 * 8.0) / elapsed) as u64;
            let tx_bps = ((tx_now.saturating_sub(prev.tx_bytes) as f64 * 8.0) / elapsed) as u64;
            (rx_bps, tx_bps)
        }
    } else {
        (0, 0)
    };
    map.insert(dev.to_string(), NetSample { rx_bytes: rx_now, tx_bytes: tx_now, taken_at: now });
    result
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

/// Gates network commands that may block when an interface is down.
fn iface_is_up(dev: &str) -> bool {
    let path = format!("/sys/class/net/{}/operstate", dev);
    std::fs::read_to_string(&path)
        .map(|s| s.trim() == "up")
        .unwrap_or(false)
}

async fn disk_usage(path: &str) -> i64 {
    // Allocated blocks account for sparse and reflinked files.
    if let Ok(out) = tokio::process::Command::new("stat")
        .args(["--format=%b", path])
        .output()
        .await
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

/// Bundles status, drive stats, and processing status for serialized BLE transport.
pub async fn dashboard_snapshot(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, stats, drive_status) = tokio::join!(
        get_status(State(state.clone())),
        crate::drives_handler::drive_stats(State(state.clone())),
        crate::drives_handler::processing_status(State(state)),
    );
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": status.1.0,
            "drive_stats": stats.1.0,
            "drive_status": drive_status.1.0,
        })),
    )
}
