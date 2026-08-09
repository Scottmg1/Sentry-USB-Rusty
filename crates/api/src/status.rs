//! Status, storage, config, and WiFi API handlers.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::router::AppState;

// ---------------------------------------------------------------------------
// Status cache
// ---------------------------------------------------------------------------
//
// Dashboard polls /api/status every 2 s per open tab. Without caching,
// each call shells out 5-6 subprocesses (iwgetid, iwconfig, ip×2, ethtool,
// stat) to gather WiFi/Ethernet/disk info. On Pi Zero 2 W that's
// measurable CPU + page faults from fork+exec — wasted on data that
// barely changes.
//
// We cache the slow parts in-process. CPU temp, fan speed, uptime, and
// gadget state stay live (they're cheap /sys reads). The TTLs match
// how often each value realistically changes:
//   * Network info (SSID, IP, signal, ethtool):  10 s
//   * Disk space (total/free via statvfs):        5 s
//
// Per-tab CPU drops ~70 % and the polling no longer dominates idle Pi
// usage.

#[derive(Clone, Default)]
struct CachedNetwork {
    wifi_ssid: String,
    /// Frequency in Hz as a string (e.g. "5180000000" for 5.18 GHz).
    /// The iOS client formats this for display via `formatFreqGHz`; the
    /// web UI ignores it. Empty when not connected or `iw` isn't
    /// installed. Cached with the rest of the wifi info since channel
    /// only changes on reconnect/roam.
    wifi_freq: String,
    wifi_ip: String,
    ether_ip: String,
    ether_speed: String,
    /// Cached device names so we don't re-scan /sys/class/net every poll.
    /// Signal strength + throughput are read live in `get_status` —
    /// `wifi_strength` / `wifi_signal_dbm` come from /proc/net/wireless
    /// (a single file read, no shell-out), and the bps values are
    /// derived from the net_sampler. Everything else here changes only
    /// when the user reconnects or swaps cable, so it's safe to cache.
    wifi_dev: String,
    eth_dev: String,
}

/// Live signal read from /proc/net/wireless — no fork+exec.
///
/// Returns `(strength_as_X/70, signal_dbm)`. The `/70` denominator
/// matches what mainline mac80211 drivers (Broadcom Cypress on Pi 4/5
/// and Pi Zero 2 W, Realtek on most third-party chipsets) report as
/// the max link quality; other drivers may scale slightly differently,
/// in which case the WifiBars indicator is approximate but the dBm
/// value the UI also shows is always exact.
///
/// /proc/net/wireless format:
/// ```text
/// Inter-| sta-|   Quality        |   Discarded packets               | Missed | WE
///  face | tus | link level noise |  nwid  crypt   frag  retry   misc | beacon | 22
///  wlan0: 0000   58.  -52.  -256        0      0      0      0    137        0
/// ```
fn read_wireless_quality(dev: &str) -> Option<(String, Option<i32>)> {
    let data = std::fs::read_to_string("/proc/net/wireless").ok()?;
    for line in data.lines().skip(2) {
        let line = line.trim_start();
        // Match either "wlan0:" or "wlan0 :" — kernel emits the former.
        let prefix = format!("{}:", dev);
        if !line.starts_with(&prefix) {
            continue;
        }
        let cols: Vec<&str> = line[prefix.len()..].split_whitespace().collect();
        // [status, link, level, noise, ...]
        if cols.len() < 3 {
            return None;
        }
        // Values end with a `.` (e.g. "58." for fixed-point) — strip it.
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

/// statvfs syscall — single fast syscall vs forking `stat`. Returns
/// `(total_bytes, free_bytes)` or `None` on failure. The path is
/// `/backingfiles/.` to match the legacy `stat --file-system` target.
fn statvfs_backing_files() -> Option<(u64, u64)> {
    let path = std::ffi::CString::new("/backingfiles/.").ok()?;
    // SAFETY: zero-init is the documented init pattern for libc structs;
    // we check the return code before reading fields.
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

// ---------------------------------------------------------------------------
// Network throughput sampler
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct NetSample {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub taken_at: Instant,
}

pub type NetSampler = Arc<Mutex<HashMap<String, NetSample>>>;

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
    /// Host-link state from /sys/class/udc/<udc>/state ("configured" =
    /// the car is actually enumerated and talking; "suspended"/"not
    /// attached" = it is not, even when drives_active says "yes").
    /// drives_active only reflects the configfs binding — the Pi's
    /// *intent* to present — which stays "yes" through a dead link.
    udc_state: String,
    /// Seconds since the car last wrote to cam_disk.bin (mtime age),
    /// -1 when unknown. Same signal the telemetry heartbeat uses.
    cam_last_write_secs: i64,
    wifi_ssid: String,
    /// Frequency in Hz as a string (e.g. "5180000000" for 5.18 GHz).
    /// Empty when not on WiFi or `iw` isn't installed. iOS renders this
    /// as "5.2 GHz" in the dashboard Wi-Fi sub-line via `formatFreqGHz`;
    /// the web UI doesn't currently use it.
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
    /// Stable per-device suffix derived from the system hostname (the
    /// part after the final `-`, e.g. "sentryusb-A3F1" → "A3F1"). iOS
    /// uses this for the dashboard hero-bar identifier so devices paired
    /// over WiFi (no BLE metadata path) still show "Sentry USB-A3F1"
    /// instead of bare "Sentry USB". Empty if `/etc/hostname` is
    /// unreadable or has no dash.
    device_suffix: String,
}

pub async fn get_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // The sysfs/procfs reads are cheap, but the snapshot directory scan
    // and file metadata hit /backingfiles — seconds while an archive run
    // saturates the disk. This is the endpoint the web UI's connection
    // banner polls, so a pinned worker here reads as "Reconnecting".
    // Blocking pool for the whole sync-FS half.
    let mut s = match tokio::task::spawn_blocking(status_fs_snapshot).await {
        Ok(s) => s,
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("status task: {}", e),
            );
        }
    };

    // Disk space — cached statvfs syscall. Replaces a per-call
    // `stat --file-system` shell-out and serves the cached value for
    // STORAGE_TTL between fresh reads.
    let storage = cached_storage().await;
    if storage.total_space > 0 {
        s.total_space = storage.total_space.to_string();
        s.free_space = storage.free_space.to_string();
    }

    // Network info — IPs, SSID, and ether_speed are cached at
    // NETWORK_TTL (they change only on reconnect/cable swap).
    //
    // WiFi signal strength + dBm are read LIVE from /proc/net/wireless
    // every poll so the bars and dBm value update in near-real-time as
    // the user moves around — the cached version would lag by 10 s,
    // which feels broken for a "signal strength" indicator.
    //
    // Throughput (rx_bps/tx_bps) is also live, derived from the
    // net_sampler background loop.
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

/// Sync half of `get_status`: the PiStatus skeleton plus every direct
/// filesystem read (sysfs/procfs, gadget state, snapshot scan). Runs on
/// the blocking pool.
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

    // SBC model
    s.sbc_model = get_sbc_model();

    // CPU temperature
    if let Ok(data) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        s.cpu_temp = data.trim().to_string();
    }

    // Fan speed (Raspberry Pi cooling fan RPM from hwmon device)
    s.fan_speed = read_fan_speed();

    // Uptime
    if let Ok(data) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs) = data.split_whitespace().next() {
            s.uptime = secs.to_string();
        }
    }

    // USB gadget status: report active only when UDC is bound AND lun.0 has a
    // backing file. A bare directory-exists check reports "yes" through a
    // partial teardown where the car has already lost the device — that drove
    // a UI bug where the dashboard stayed green after a failed toggle.
    if sentryusb_gadget::is_active() {
        s.drives_active = "yes".into();
    }
    s.udc_state = read_udc_state();
    s.cam_last_write_secs = cam_last_write_secs();

    // Snapshots. Oldest/newest come from the scan's mtime min/max —
    // slot numbers are not time-monotonic (a reflash can leave a stale
    // high-numbered snapshot above a restarted sequence), and reading
    // the endpoints of the name-sorted list rendered the dashboard's
    // date range backwards on such devices.
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

/// Refresh-on-stale wrapper around the heavy WiFi + Ethernet shell-outs.
/// Returns a (cheap-to-clone) snapshot — concurrent callers share the
/// same fetch when within TTL, and only one re-fetches when stale.
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

/// The original WiFi + Ethernet shell-out block, factored out so the
/// cache layer can call it without recursion.
async fn compute_network_info() -> CachedNetwork {
    let mut info = CachedNetwork::default();

    // WiFi info — skip shell queries when interface is down (saves 5-10s
    // on ethernet-only systems where wlan0 exists but is unconfigured).
    // `iwconfig` is no longer needed here — signal strength + dBm are
    // read live from /proc/net/wireless on every status poll, so this
    // cache only needs the SSID and IP (both rare-change values).
    let wifi_dev = find_net_device("wl*");
    if !wifi_dev.is_empty() && iface_is_up(&wifi_dev) {
        info.wifi_dev = wifi_dev.clone();
        let ssid_args = ["-r", wifi_dev.as_str()];
        let ip_args = ["-4", "addr", "show", wifi_dev.as_str()];
        // `iw dev <iface> link` line "freq: 5180" gives us the channel
        // frequency in MHz with no extra cost beyond a single fork. The
        // result is cached for NETWORK_TTL like the other wifi fields.
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
                // Format: `freq: 5180` (MHz). Convert to Hz string so iOS
                // `Formatters.formatFreqGHz` can divide by 1e9 and render
                // "5.2 GHz" without further parsing.
                if let Some(rest) = trimmed.strip_prefix("freq:") {
                    if let Ok(mhz) = rest.trim().parse::<u64>() {
                        info.wifi_freq = (mhz * 1_000_000).to_string();
                        break;
                    }
                }
            }
        }
    }

    // Ethernet info — same operstate guard
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

// ---------------------------------------------------------------------------
// GET /api/status/storage
// ---------------------------------------------------------------------------

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

    // statvfs syscall instead of forking `stat` — matches the
    // refactor in get_status. /api/status/storage is polled at 10 s,
    // so we just always read fresh here rather than cache.
    if let Some((total, free)) = statvfs_backing_files() {
        sb.total_space = total as i64;
        sb.free_space = free as i64;
    }

    // Derive snapshot usage by subtraction (reflink clones make du unreliable)
    let disk_images = sb.cam_size + sb.music_size + sb.lightshow_size + sb.boombox_size;
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

    // `uses_ble` controls whether the BLE pair card renders in Device
    // settings. Historically it was VIN-gated, meaning users who didn't
    // pick BLE during initial setup couldn't even see the card to opt
    // in later. Now it returns "yes" whenever BLE *could* be used:
    //   * user has explicitly enabled BLE in settings, OR
    //   * a VIN is already set (legacy install), OR
    //   * a BLE keypair has been generated (opted in at some point), OR
    //   * pairing artifacts exist (paired marker present).
    // Fresh installs that never touched BLE still return "no", so the
    // card stays hidden until the user enables BLE — preserving the
    // clean default for non-Tesla / non-BLE users. (tesla-control is
    // gone; the keypair is the native "opted in" signal.)
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

// ---------------------------------------------------------------------------
// GET /api/wifi
// ---------------------------------------------------------------------------

pub async fn get_wifi_config(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut ssid = String::new();
    let mut connected = false;
    let mut source = String::new();

    // 1. Try iwgetid — reads the kernel's association state in
    // milliseconds. `nmcli dev wifi` triggers a WiFi SCAN (3+ s
    // observed) and is only needed when iwgetid comes up empty.
    if let Ok(out) = sentryusb_shell::run("iwgetid", &["-r"]).await {
        let s = out.trim();
        if !s.is_empty() {
            ssid = s.to_string();
            connected = true;
            source = "iwgetid".into();
        }
    }

    // 2. Fallback: nmcli
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

/// One-pass snapshot summary: count plus the oldest/newest `snap.bin`
/// mtimes (unix seconds).
struct SnapshotScan {
    count: usize,
    oldest_unix: Option<u64>,
    newest_unix: Option<u64>,
}

/// Scan the snapshot backing files at the top of `base`
/// (`/backingfiles/snapshots/` in production).
///
/// The original implementation called a generic recursive `walkdir` and
/// filtered for paths ending in `snap.bin`. That descended through every
/// snapshot's `mnt -> /tmp/snapshots/snap-NNN` symlink, which is backed by
/// an autofs mount (timeout=300s) that re-mounts the per-snapshot vfat loop
/// device on first access. Each fresh /api/status call after the autofs
/// timeout therefore triggered up to 130 vfat mounts *and* walked the
/// entire dashcam tree inside each one — observed 15,000+ openat syscalls
/// per request and 5-15s TTFB.
///
/// Snapshots always have `snap.bin` directly at the top level
/// (`/backingfiles/snapshots/snap-NNNNNN/snap.bin`). We only need to scan
/// that one directory level — no recursion, no symlink follow.
fn scan_snapshots(base: &std::path::Path) -> SnapshotScan {
    let mut scan = SnapshotScan { count: 0, oldest_unix: None, newest_unix: None };
    let Ok(entries) = std::fs::read_dir(base) else {
        return scan;
    };
    for entry in entries.flatten() {
        // Only consider entries that are themselves directories on the
        // host filesystem. `file_type()` uses the dirent's d_type and
        // does NOT follow symlinks, so the `mnt` autofs symlink inside
        // each snapshot is never resolved here.
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        // Numeric `snap-NNNNNN` only, matching what the eviction and
        // listing paths consider a snapshot; a tooling/scratch directory
        // that happens to contain a `snap.bin` must not inflate the
        // count or skew the dashboard's date range.
        if !entry
            .file_name()
            .to_string_lossy()
            .strip_prefix("snap-")
            .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let snap_bin = entry.path().join("snap.bin");
        // Use symlink_metadata to avoid traversing into anything weird;
        // snap.bin is always a regular file on the parent XFS.
        if let Ok(meta) = std::fs::symlink_metadata(&snap_bin) {
            scan.count += 1;
            // Fold mtimes into min/max — slot names are NOT
            // time-monotonic, so name-order endpoints would lie.
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

    /// Snapshot slot numbers are NOT time-monotonic in the field: a
    /// reflash can leave a stale high-numbered snapshot (real Pi:
    /// snap-000414 from Jul 9 above snap-000413 from Aug 8, numbering
    /// restarted at 000000 beneath it), which made the dashboard render
    /// its date range backwards ("7/29 → 7/21"). The scan must report
    /// the min/max of the actual mtimes, not the mtimes of the lexical
    /// first/last names.
    #[test]
    fn scan_snapshots_range_is_mtime_min_max_not_name_order() {
        let base = std::env::temp_dir().join(format!(
            "sentryusb-snap-scan-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        // Codex's regression fixture: names 000000/000413/000414 with
        // mtimes Jul 29 / Aug 8 / Jul 21 (as unix secs, order scrambled
        // relative to names).
        let fixtures = [
            ("snap-000000", 1_785_300_000u64), // newer than 414, older than 413
            ("snap-000413", 1_786_200_000u64), // true newest
            ("snap-000414", 1_784_600_000u64), // true oldest, highest name
        ];
        for (name, mtime) in fixtures {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            let bin = dir.join("snap.bin");
            let f = std::fs::File::create(&bin).unwrap();
            let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
            f.set_times(std::fs::FileTimes::new().set_modified(t)).unwrap();
        }
        // A non-snapshot stray file and an empty dir must not count.
        std::fs::File::create(base.join("stray.txt")).unwrap();
        std::fs::create_dir_all(base.join("not-a-snap")).unwrap();
        // A tooling/scratch dir that happens to hold a `snap.bin` must
        // not inflate the count or skew the range: its mtime here is
        // older than every real snapshot, so a leak would show up as a
        // wrong "oldest" date.
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

/// Host-link state of the first UDC ("configured", "suspended",
/// "not attached", ...). Empty when there is no UDC (non-gadget dev box).
/// Shared with the health check so the pill and the health warning can
/// never disagree about what the link state means.
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

/// Last segment after the final `-` of the system hostname — e.g.
/// "sentryusb-A3F1" → "A3F1". Used by iOS to render the device-specific
/// identifier on the dashboard even when paired over WiFi (the BLE path
/// has its own device-info channel, but WiFi-only pairing has no way to
/// learn the suffix without it being in /status). Empty when the
/// hostname has no dash or `/etc/hostname` can't be read; an 8-char cap
/// guards against weird hostnames where the post-dash segment is a
/// fully-qualified domain piece rather than a stable identifier.
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

/// Returns true when the kernel reports the interface in `operstate == "up"`.
///
/// We use this to gate the shell queries below: `iwgetid`/`iwconfig`/`ip` can
/// each block for several seconds when an interface is present-but-DOWN
/// (e.g. `wlan0` exists but no NetworkManager / no Skip-WiFi configured),
/// adding up to 5-15s on `GET /api/status`. Companion apps that probe this
/// endpoint with a short HTTP timeout then fall back to BLE-only mode even
/// though the Pi is reachable over ethernet.
fn iface_is_up(dev: &str) -> bool {
    let path = format!("/sys/class/net/{}/operstate", dev);
    std::fs::read_to_string(&path)
        .map(|s| s.trim() == "up")
        .unwrap_or(false)
}

async fn disk_usage(path: &str) -> i64 {
    // On Linux, use stat to get st_blocks * 512 for actual disk usage
    // (handles sparse files and reflink copies correctly).
    // Async to avoid blocking the tokio worker thread on /api/status
    // polls — hit ~every 15 s by the SC companion app per device.
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

/// GET /api/dashboard-snapshot
///
/// The dashboard's three per-tick fetches (/status, /drives/stats,
/// /drives/status) bundled into one response. Over BLE each request is a
/// serialized round trip through the GATT proxy, so three calls per tick
/// tripled the latency and kept the pipe busy; the app calls this instead
/// when the transport is BLE (falling back to the three calls on 404 for
/// older Pi builds).
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
