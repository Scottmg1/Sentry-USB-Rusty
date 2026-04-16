use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use chrono::{Datelike, Timelike};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::info;

use crate::router::AppState;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LOCK_CHIME_DIR: &str = "/mutable/LockChime";
const LOCK_CHIME_TARGET: &str = "/mutable/LockChime.wav";
const LOCK_CHIME_MAX_BYTES: usize = 1 * 1024 * 1024;
const LOCK_CHIME_MAX_SECONDS: f64 = 5.0;
const LOCK_CHIME_CONFIG_FILE: &str = "/mutable/LockChime/.random_config.json";
const LOCK_CHIME_ACTIVE_FILE: &str = "/mutable/LockChime/.active_name";
const CAM_DISK_IMAGE: &str = "/backingfiles/cam_disk.bin";
const CAM_MOUNT_POINT: &str = "/mnt/cam";
const GADGET_CONFIG_DIR: &str = "/sys/kernel/config/usb_gadget/sentryusb";

// ---------------------------------------------------------------------------
// Cam-disk mutex (serializes gadget disable/enable + mount/unmount)
// ---------------------------------------------------------------------------

static CAM_DISK_MU: std::sync::LazyLock<Mutex<()>> = std::sync::LazyLock::new(|| Mutex::new(()));

// ---------------------------------------------------------------------------
// Logging helper
// ---------------------------------------------------------------------------

fn lock_chime_log(msg: &str) {
    let log_path = "/mutable/archiveloop.log";
    let now = chrono::Local::now().format("%a %e %b %H:%M:%S %Z %Y");
    let line = format!("{}: [lock-chime] {}\n", now, msg);
    let _ = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log_path)
        .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
}

// ---------------------------------------------------------------------------
// Filename validation / sanitization
// ---------------------------------------------------------------------------

fn is_valid_lock_chime_file(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    let re = regex::Regex::new(r"^[a-zA-Z0-9 _.\-]+\.wav$").unwrap();
    re.is_match(name)
}

fn sanitize_lock_chime_name(name: &str) -> String {
    let re = regex::Regex::new(r"[^a-zA-Z0-9 _.\-]").unwrap();
    let mut result = re.replace_all(name, "").trim().to_string();
    if result.is_empty() {
        result = "sound".to_string();
    }
    if !result.to_lowercase().ends_with(".wav") {
        result.push_str(".wav");
    }
    result
}

// ---------------------------------------------------------------------------
// Atomic write (temp -> fsync -> rename -> fsync dir -> touch -> sync)
// ---------------------------------------------------------------------------

fn write_chime_file_atomic(dest_path: &str, data: &[u8]) -> Result<(), String> {
    let dir = Path::new(dest_path)
        .parent()
        .unwrap_or(Path::new("/"));
    let base = Path::new(dest_path)
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let tmp_path = dir.join(format!(".{}.tmp", base));

    // 1. Write to temp file
    std::fs::write(&tmp_path, data)
        .map_err(|e| format!("write temp: {}", e))?;

    // 2. Fsync the temp file
    if let Ok(f) = std::fs::File::open(&tmp_path) {
        let _ = f.sync_all();
    }

    // 3. Remove old target
    let _ = std::fs::remove_file(dest_path);

    // 4. Atomic rename
    if let Err(e) = std::fs::rename(&tmp_path, dest_path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!("rename: {}", e));
    }

    // 5. Fsync the directory
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }

    // 6. Touch timestamps
    // On Linux we can use filetime or just call touch; just open+close suffices
    // to update mtime on many filesystems. We also write a single byte and rewrite.
    // For simplicity, use std::fs::File::open + set_len to touch.
    // Actually, the simplest portable approach: rewrite with same data is too expensive.
    // Instead, we'll use libc::utimensat via the nix crate or just skip on non-Linux.
    // Since this runs on Linux (Pi), we can shell out to touch if needed, but
    // the rename itself updates mtime on most filesystems. Keep it simple.

    // 7. Full system sync
    let _ = std::process::Command::new("sync").status();

    Ok(())
}

// ---------------------------------------------------------------------------
// Gadget / mount helpers
// ---------------------------------------------------------------------------

fn is_gadget_active() -> bool {
    Path::new(GADGET_CONFIG_DIR).exists()
}

async fn gadget_disable() -> Result<(), String> {
    tokio::task::spawn_blocking(sentryusb_gadget::disable)
        .await
        .map_err(|e| format!("join: {}", e))?
        .map_err(|e| e.to_string())
}

async fn gadget_enable() -> Result<(), String> {
    tokio::task::spawn_blocking(sentryusb_gadget::enable)
        .await
        .map_err(|e| format!("join: {}", e))?
        .map_err(|e| e.to_string())
}

fn is_mount_point_active(mount_point: &str) -> bool {
    let Ok(data) = std::fs::read_to_string("/proc/mounts") else {
        return false;
    };
    for line in data.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() >= 2 && fields[1] == mount_point {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Cam disk sync operations
// ---------------------------------------------------------------------------

async fn copy_lock_chime_to_cam_mount() -> Result<(), String> {
    let data = match std::fs::read(LOCK_CHIME_TARGET) {
        Ok(d) => d,
        Err(_) => return Ok(()), // No staged LockChime.wav -- nothing to copy
    };

    if !Path::new(CAM_DISK_IMAGE).exists() {
        info!("lockchime: cam disk image not found, skipping cam sync");
        return Ok(());
    }

    if is_mount_point_active(CAM_MOUNT_POINT) {
        info!("lockchime: cam disk already mounted (archiveloop?), skipping to avoid conflict");
        return Ok(());
    }

    sentryusb_shell::run_with_timeout(Duration::from_secs(10), "mount", &[CAM_MOUNT_POINT])
        .await
        .map_err(|e| format!("mount cam disk: {}", e))?;

    let cam_target = PathBuf::from(CAM_MOUNT_POINT).join("LockChime.wav");
    let write_err = write_chime_file_atomic(cam_target.to_str().unwrap_or(""), &data);

    if let Err(e) = sentryusb_shell::run_with_timeout(Duration::from_secs(10), "umount", &[CAM_MOUNT_POINT]).await {
        info!("lockchime: umount cam disk failed: {}", e);
    }

    if let Err(e) = write_err {
        return Err(format!("write LockChime.wav to cam disk: {}", e));
    }

    info!("lockchime: synced LockChime.wav to cam disk ({} bytes)", data.len());
    Ok(())
}

async fn sync_lock_chime_to_cam_disk() -> Result<(), String> {
    let _guard = CAM_DISK_MU.lock().await;

    if !Path::new(CAM_DISK_IMAGE).exists() {
        info!("lockchime: cam disk image not found, skipping cam sync");
        return Ok(());
    }

    let gadget_was_active = is_gadget_active();

    if gadget_was_active {
        if let Err(e) = gadget_disable().await {
            info!("lockchime: gadget disable failed: {}", e);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    let copy_err = copy_lock_chime_to_cam_mount().await;

    if gadget_was_active {
        if let Err(e) = gadget_enable().await {
            info!("lockchime: gadget enable failed: {}", e);
            return Err(format!("re-enable gadget: {}", e));
        }
        info!("lockchime: USB gadget re-enabled -- Tesla will read the new lock sound");
    }

    copy_err
}

async fn clear_lock_chime_from_cam_disk() -> Result<(), String> {
    let _guard = CAM_DISK_MU.lock().await;

    if !Path::new(CAM_DISK_IMAGE).exists() {
        return Ok(());
    }

    let gadget_was_active = is_gadget_active();

    if gadget_was_active {
        if let Err(e) = gadget_disable().await {
            info!("lockchime: gadget disable failed: {}", e);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    if let Err(e) =
        sentryusb_shell::run_with_timeout(Duration::from_secs(10), "mount", &[CAM_MOUNT_POINT]).await
    {
        if gadget_was_active {
            let _ = gadget_enable().await;
        }
        return Err(format!("mount cam disk: {}", e));
    }

    let cam_target = PathBuf::from(CAM_MOUNT_POINT).join("LockChime.wav");
    let _ = std::fs::remove_file(&cam_target);
    let _ = std::process::Command::new("sync").status();

    if let Err(e) =
        sentryusb_shell::run_with_timeout(Duration::from_secs(10), "umount", &[CAM_MOUNT_POINT]).await
    {
        info!("lockchime: umount cam disk failed: {}", e);
    }

    if gadget_was_active {
        if let Err(e) = gadget_enable().await {
            return Err(format!("re-enable gadget: {}", e));
        }
    }

    info!("lockchime: cleared LockChime.wav from cam disk");
    Ok(())
}

// ---------------------------------------------------------------------------
// WAV processing
// ---------------------------------------------------------------------------

fn parse_wav_duration(data: &[u8]) -> Result<f64, String> {
    if data.len() < 44 {
        return Err("file too small to be a valid WAV".into());
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a WAV file -- must be .wav format".into());
    }

    let mut pos: usize = 12;
    let mut byte_rate: u32 = 0;
    let mut fmt_found = false;

    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);

        if chunk_id == b"fmt " {
            if pos + 8 + chunk_size as usize > data.len() {
                return Err("malformed WAV fmt chunk".into());
            }
            if chunk_size < 16 {
                return Err("unsupported WAV format".into());
            }
            byte_rate = u32::from_le_bytes([data[pos + 16], data[pos + 17], data[pos + 18], data[pos + 19]]);
            fmt_found = true;
        } else if chunk_id == b"data" && fmt_found && byte_rate > 0 {
            return Ok(chunk_size as f64 / byte_rate as f64);
        }

        pos += 8 + chunk_size as usize;
        if chunk_size % 2 != 0 {
            pos += 1; // WAV chunk padding byte
        }
    }

    if !fmt_found {
        return Err("not a WAV file -- must be .wav format".into());
    }
    Err("could not determine WAV duration".into())
}

struct WavInfo {
    audio_format: u16,
    num_channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    #[allow(dead_code)]
    duration: f64,
    #[allow(dead_code)]
    fmt_offset: usize,
    data_offset: usize,
    data_size: u32,
}

fn parse_wav_info(data: &[u8]) -> Result<WavInfo, String> {
    if data.len() < 44 {
        return Err("file too small to be a valid WAV".into());
    }
    if &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("not a WAV file -- must be .wav format".into());
    }

    let mut pos: usize = 12;
    let mut info = WavInfo {
        audio_format: 0,
        num_channels: 0,
        sample_rate: 0,
        bits_per_sample: 0,
        duration: 0.0,
        fmt_offset: 0,
        data_offset: 0,
        data_size: 0,
    };
    let mut fmt_found = false;

    while pos + 8 <= data.len() {
        let chunk_id = &data[pos..pos + 4];
        let chunk_size = u32::from_le_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]]);

        if chunk_id == b"fmt " {
            if pos + 8 + chunk_size as usize > data.len() || chunk_size < 16 {
                return Err("malformed WAV fmt chunk".into());
            }
            info.fmt_offset = pos;
            info.audio_format = u16::from_le_bytes([data[pos + 8], data[pos + 9]]);
            info.num_channels = u16::from_le_bytes([data[pos + 10], data[pos + 11]]);
            info.sample_rate = u32::from_le_bytes([data[pos + 12], data[pos + 13], data[pos + 14], data[pos + 15]]);
            info.bits_per_sample = u16::from_le_bytes([data[pos + 22], data[pos + 23]]);
            fmt_found = true;
        } else if chunk_id == b"data" && fmt_found {
            info.data_offset = pos + 8;
            info.data_size = chunk_size;
            if info.sample_rate > 0 && info.num_channels > 0 && info.bits_per_sample > 0 {
                let bytes_per_sample = info.num_channels as u32 * info.bits_per_sample as u32 / 8;
                let byte_rate = info.sample_rate * bytes_per_sample;
                if byte_rate > 0 {
                    info.duration = chunk_size as f64 / byte_rate as f64;
                }
            }
            return Ok(info);
        }

        pos += 8 + chunk_size as usize;
        if chunk_size % 2 != 0 {
            pos += 1;
        }
    }

    if !fmt_found {
        return Err("not a WAV file -- must be .wav format".into());
    }
    Err("could not determine WAV format".into())
}

/// Validates a WAV is PCM 16-bit, resamples to 44.1kHz if needed, and converts
/// stereo/multi-channel to mono. Returns the (possibly modified) WAV data.
fn ensure_mono_wav(data: &[u8]) -> Result<Vec<u8>, String> {
    let info = parse_wav_info(data)?;

    if info.audio_format != 1 {
        return Err(format!("only PCM WAV is supported (got format {})", info.audio_format));
    }
    if info.bits_per_sample != 16 {
        return Err(format!("bit depth must be 16-bit (got {}-bit)", info.bits_per_sample));
    }

    let channels = info.num_channels as usize;
    let num_frames = info.data_size as usize / (channels * 2);
    let src_offset = info.data_offset;
    let needs_resample = info.sample_rate != 44100;
    let needs_mono = channels > 1;

    // Already mono 44100Hz 16-bit -- nothing to do
    if !needs_resample && !needs_mono {
        return Ok(data.to_vec());
    }

    // Step 1: Read all samples as interleaved i16
    let total_samples = num_frames * channels;
    let mut samples: Vec<i16> = Vec::with_capacity(total_samples);
    for i in 0..total_samples {
        let idx = src_offset + i * 2;
        if idx + 2 > data.len() {
            break;
        }
        samples.push(i16::from_le_bytes([data[idx], data[idx + 1]]));
    }

    // Step 2: Mix down to mono if multi-channel
    let mono: Vec<i16> = if needs_mono {
        let mut m = Vec::with_capacity(num_frames);
        for i in 0..num_frames {
            let mut sum: i32 = 0;
            for ch in 0..channels {
                if let Some(&s) = samples.get(i * channels + ch) {
                    sum += s as i32;
                }
            }
            m.push((sum / channels as i32) as i16);
        }
        info!("[lockchime] Mixed {}-channel WAV to mono", channels);
        m
    } else {
        samples
    };

    // Step 3: Resample to 44100Hz if needed (linear interpolation)
    let resampled: Vec<i16> = if needs_resample {
        let src_rate = info.sample_rate as f64;
        let dst_rate = 44100.0;
        let ratio = src_rate / dst_rate;
        let out_len = (mono.len() as f64 / ratio) as usize;
        let mut r = Vec::with_capacity(out_len);

        for i in 0..out_len {
            let src_pos = i as f64 * ratio;
            let idx = src_pos as usize;
            let frac = src_pos - idx as f64;

            if idx + 1 < mono.len() {
                let s0 = mono[idx] as f64;
                let s1 = mono[idx + 1] as f64;
                r.push((s0 + frac * (s1 - s0)) as i16);
            } else if idx < mono.len() {
                r.push(mono[idx]);
            }
        }
        info!(
            "[lockchime] Resampled WAV from {} Hz to 44100 Hz ({} -> {} samples)",
            info.sample_rate,
            mono.len(),
            out_len
        );
        r
    } else {
        mono
    };

    // Step 4: Build new WAV file: mono 44100Hz 16-bit
    let out_data_size = (resampled.len() * 2) as u32;
    let mut out = vec![0u8; 44 + out_data_size as usize];

    // RIFF header
    out[0..4].copy_from_slice(b"RIFF");
    out[4..8].copy_from_slice(&(36 + out_data_size).to_le_bytes());
    out[8..12].copy_from_slice(b"WAVE");

    // fmt chunk
    out[12..16].copy_from_slice(b"fmt ");
    out[16..20].copy_from_slice(&16u32.to_le_bytes()); // chunk size
    out[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    out[22..24].copy_from_slice(&1u16.to_le_bytes()); // mono
    out[24..28].copy_from_slice(&44100u32.to_le_bytes()); // sample rate
    out[28..32].copy_from_slice(&(44100u32 * 1 * 16 / 8).to_le_bytes()); // byte rate
    out[32..34].copy_from_slice(&(1u16 * 16 / 8).to_le_bytes()); // block align
    out[34..36].copy_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data chunk
    out[36..40].copy_from_slice(b"data");
    out[40..44].copy_from_slice(&out_data_size.to_le_bytes());
    for (i, &s) in resampled.iter().enumerate() {
        let offset = 44 + i * 2;
        out[offset..offset + 2].copy_from_slice(&(s as u16).to_le_bytes());
    }

    info!(
        "[lockchime] Normalized WAV to mono 44100Hz 16-bit ({} -> {} bytes)",
        data.len(),
        out.len()
    );
    Ok(out)
}

// ---------------------------------------------------------------------------
// Random config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RandomConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    mode: String,
    #[serde(default)]
    interval: String,
    #[serde(default)]
    hour: i32,
    #[serde(default)]
    day: i32,
}

fn load_random_config() -> RandomConfig {
    let data = match std::fs::read_to_string(LOCK_CHIME_CONFIG_FILE) {
        Ok(d) => d,
        Err(_) => return RandomConfig::default(),
    };
    serde_json::from_str(&data).unwrap_or_default()
}

fn save_random_config_to_disk(cfg: &RandomConfig) -> Result<(), String> {
    // Validate
    if !cfg.mode.is_empty()
        && cfg.mode != "on_connect"
        && cfg.mode != "scheduled"
        && cfg.mode != "smart"
    {
        return Err("invalid mode: must be on_connect, scheduled, or smart".into());
    }
    let mut cfg = cfg.clone();
    if cfg.mode == "scheduled" || cfg.mode == "smart" {
        if cfg.interval.is_empty() {
            cfg.interval = "daily".into();
        }
        if cfg.interval != "hourly" && cfg.interval != "daily" && cfg.interval != "weekly" {
            return Err("invalid interval: must be hourly, daily, or weekly".into());
        }
        if cfg.hour < 0 || cfg.hour > 23 {
            cfg.hour = 0;
        }
        if cfg.day < 0 || cfg.day > 6 {
            cfg.day = 0;
        }
    }

    let _ = std::fs::create_dir_all(LOCK_CHIME_DIR);
    let json = serde_json::to_string_pretty(&cfg).map_err(|e| e.to_string())?;
    std::fs::write(LOCK_CHIME_CONFIG_FILE, json).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// WAV file listing
// ---------------------------------------------------------------------------

fn list_wav_files() -> Vec<String> {
    let entries = match std::fs::read_dir(LOCK_CHIME_DIR) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut wavs = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(true) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.to_lowercase().ends_with(".wav") {
            wavs.push(name);
        }
    }
    wavs
}

// ---------------------------------------------------------------------------
// Pick and activate random chime
// ---------------------------------------------------------------------------

fn pick_and_activate_random() -> String {
    let wavs = list_wav_files();
    if wavs.is_empty() {
        return String::new();
    }

    // Read current active name so we can avoid picking it again
    let current_active = std::fs::read_to_string(LOCK_CHIME_ACTIVE_FILE)
        .unwrap_or_default()
        .trim()
        .to_string();

    // Filter out current chime if more than one option
    let candidates: Vec<&String> = if !current_active.is_empty() && wavs.len() > 1 {
        let filtered: Vec<&String> = wavs.iter().filter(|w| **w != current_active).collect();
        if filtered.is_empty() {
            wavs.iter().collect()
        } else {
            filtered
        }
    } else {
        wavs.iter().collect()
    };

    let chosen = candidates[rand::rng().random_range(0..candidates.len())].clone();
    let src_path = PathBuf::from(LOCK_CHIME_DIR).join(&chosen);

    let data = match std::fs::read(&src_path) {
        Ok(d) => d,
        Err(e) => {
            info!("lockchime: failed to read {}: {}", chosen, e);
            return String::new();
        }
    };

    if let Err(e) = write_chime_file_atomic(LOCK_CHIME_TARGET, &data) {
        info!("lockchime: failed to write target: {}", e);
        return String::new();
    }
    let _ = std::fs::write(LOCK_CHIME_ACTIVE_FILE, &chosen);
    info!("lockchime: random mode activated {:?}", chosen);

    chosen
}

// ---------------------------------------------------------------------------
// BLE VIN helper
// ---------------------------------------------------------------------------

fn read_ble_vin() -> String {
    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::parse_file(config_path) {
        Ok((active, _)) => active.get("TESLA_BLE_VIN").cloned().unwrap_or_default(),
        Err(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// BLE shift state query
// ---------------------------------------------------------------------------

async fn query_ble_shift_state() -> Result<String, String> {
    let vin = read_ble_vin();
    if vin.is_empty() {
        return Err("no VIN configured -- set TESLA_BLE_VIN in your config".into());
    }

    if !Path::new("/root/.ble/paired").exists() {
        return Err("BLE not paired -- pair your Pi in Settings first".into());
    }

    if !Path::new("/root/.ble/key_private.pem").exists() {
        return Err("BLE private key missing at /root/.ble/key_private.pem".into());
    }

    // Stop BLE GATT daemon for exclusive hci0 access
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(5),
        "systemctl",
        &["stop", "sentryusb-ble"],
    )
    .await;

    let vin_upper = vin.to_uppercase();
    let result = sentryusb_shell::run_with_timeout(
        Duration::from_secs(15),
        "/root/bin/tesla-control",
        &[
            "-ble",
            "-vin",
            &vin_upper,
            "-key-file",
            "/root/.ble/key_private.pem",
            "state",
            "drive",
        ],
    )
    .await;

    // Always restart BLE daemon
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(5),
        "systemctl",
        &["start", "sentryusb-ble"],
    )
    .await;

    let out = match result {
        Ok(o) => o,
        Err(e) => {
            let err_msg = e.to_string();
            info!("lockchime: BLE drive state query failed: {}", err_msg);
            return Err(format!("tesla-control failed: {}", err_msg));
        }
    };

    // Parse JSON: {"driveState":{"shiftState":{"p":{}},...}}
    let parsed: serde_json::Value =
        serde_json::from_str(&out).map_err(|e| {
            info!("lockchime: failed to parse drive state JSON: {}\nRaw output: {}", e, out);
            format!("unexpected response from tesla-control -- raw: {}", out.trim())
        })?;

    let shift_state = &parsed["driveState"]["shiftState"];
    if shift_state.is_null() || (shift_state.is_object() && shift_state.as_object().unwrap().is_empty()) {
        return Err("vehicle returned empty drive state -- car may be asleep".into());
    }

    if let Some(obj) = shift_state.as_object() {
        for key in obj.keys() {
            let state = key.to_uppercase();
            info!("lockchime: vehicle shift state: {}", state);
            return Ok(state);
        }
    }

    Err("shiftState was empty in response".into())
}

// ---------------------------------------------------------------------------
// Scheduler (background task)
// ---------------------------------------------------------------------------

static SCHEDULER_ONCE: Once = Once::new();

/// Starts the background scheduler for scheduled/smart random mode.
/// Safe to call multiple times -- only starts once.
pub fn start_lock_chime_scheduler() {
    SCHEDULER_ONCE.call_once(|| {
        tokio::spawn(lock_chime_scheduler_loop());
    });
}

async fn lock_chime_scheduler_loop() {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    let mut last_scheduled_key = String::new();
    let mut smart_pending = false;
    let mut last_smart_retry: Option<chrono::DateTime<chrono::Local>> = None;

    loop {
        interval.tick().await;

        let cfg = load_random_config();
        if !cfg.enabled {
            last_scheduled_key.clear();
            continue;
        }

        if cfg.mode == "scheduled" || cfg.mode == "smart" {
            let now = chrono::Local::now();

            let run_key = match cfg.interval.as_str() {
                "hourly" => {
                    if now.format("%M").to_string() != "00" {
                        continue;
                    }
                    now.format("%Y-%m-%d-%H").to_string()
                }
                "daily" => {
                    if now.hour() as i32 != cfg.hour || now.format("%M").to_string() != "00" {
                        continue;
                    }
                    now.format("%Y-%m-%d").to_string()
                }
                "weekly" => {
                    if now.weekday().num_days_from_sunday() as i32 != cfg.day
                        || now.hour() as i32 != cfg.hour
                        || now.format("%M").to_string() != "00"
                    {
                        continue;
                    }
                    now.format("%Y-W%W").to_string()
                }
                _ => continue,
            };

            if cfg.mode == "smart" {
                let is_new_window = run_key != last_scheduled_key;
                let is_retry = smart_pending
                    && last_smart_retry.is_some()
                    && now.signed_duration_since(last_smart_retry.unwrap())
                        >= chrono::Duration::minutes(15);

                if !is_new_window && !is_retry {
                    continue;
                }

                match query_ble_shift_state().await {
                    Err(e) => {
                        info!("lockchime: smart mode -- BLE query failed: {}, will retry in 15 min", e);
                        lock_chime_log(&format!(
                            "smart mode -- BLE query failed: {}, will retry in 15 min",
                            e
                        ));
                        smart_pending = true;
                        last_smart_retry = Some(now);
                        last_scheduled_key = run_key;
                        continue;
                    }
                    Ok(ref state) if state != "P" => {
                        info!("lockchime: smart mode -- vehicle in {}, will retry in 15 min", state);
                        lock_chime_log(&format!(
                            "smart mode -- vehicle in {}, will retry in 15 min",
                            state
                        ));
                        smart_pending = true;
                        last_smart_retry = Some(now);
                        last_scheduled_key = run_key;
                        continue;
                    }
                    Ok(_) => {
                        info!("lockchime: smart mode -- vehicle in Park, proceeding with chime change");
                        lock_chime_log("smart mode -- vehicle in Park, proceeding with chime change");
                        smart_pending = false;
                    }
                }
            } else {
                // Scheduled mode: simple dedup
                if run_key == last_scheduled_key {
                    continue;
                }
            }

            let chosen = pick_and_activate_random();
            if !chosen.is_empty() {
                lock_chime_log(&format!("{} mode -- changed lock chime to {:?}", cfg.mode, chosen));
                match sync_lock_chime_to_cam_disk().await {
                    Ok(()) => {
                        lock_chime_log(&format!(
                            "{} mode -- cam disk sync OK, Tesla will use new sound",
                            cfg.mode
                        ));
                    }
                    Err(e) => {
                        info!("lockchime: {} cam sync failed: {}", cfg.mode, e);
                        lock_chime_log(&format!("{} mode -- cam disk sync FAILED: {}", cfg.mode, e));
                    }
                }
            }
            last_scheduled_key = run_key;
        }
    }
}

// ---------------------------------------------------------------------------
// Deduplicate filename helper
// ---------------------------------------------------------------------------

fn deduplicate_filename(dir: &str, base_name: &str) -> Option<(PathBuf, String)> {
    let dest_path = PathBuf::from(dir).join(base_name);
    if !dest_path.exists() {
        return Some((dest_path, base_name.to_string()));
    }

    let ext = Path::new(base_name)
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let stem = base_name.strip_suffix(&ext).unwrap_or(base_name);

    for i in 1..=100 {
        let candidate_name = format!("{}_{}{}", stem, i, ext);
        let candidate_path = PathBuf::from(dir).join(&candidate_name);
        if !candidate_path.exists() {
            return Some((candidate_path, candidate_name));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// HTTP Handlers
// ---------------------------------------------------------------------------

/// GET /api/lockchime/list
pub async fn list(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let _ = std::fs::create_dir_all(LOCK_CHIME_DIR);

    let entries = match std::fs::read_dir(LOCK_CHIME_DIR) {
        Ok(e) => e,
        Err(_) => return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to list sounds"),
    };

    let active_name = std::fs::read_to_string(LOCK_CHIME_ACTIVE_FILE)
        .unwrap_or_default()
        .trim()
        .to_string();

    let mut sounds = Vec::new();
    for entry in entries.flatten() {
        if entry.file_type().map(|ft| ft.is_dir()).unwrap_or(true) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.to_lowercase().ends_with(".wav") {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
        sounds.push(serde_json::json!({
            "name": name,
            "size": size,
            "active": name == active_name,
        }));
    }

    let active_set = Path::new(LOCK_CHIME_TARGET).exists();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "sounds": sounds,
            "active_name": active_name,
            "active_set": active_set,
        })),
    )
}

/// POST /api/lockchime/upload
pub async fn upload(
    State(_s): State<AppState>,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    // The body arrives as raw multipart bytes. Parse with a simple boundary search.
    let raw = body.to_vec();

    if raw.len() > LOCK_CHIME_MAX_BYTES + 4096 {
        return crate::json_error(StatusCode::BAD_REQUEST, "Upload too large (max 1 MB)");
    }

    // Parse multipart manually: find boundary from first line
    let (filename, file_data) = match parse_multipart_wav(&raw) {
        Ok(r) => r,
        Err(e) => return crate::json_error(StatusCode::BAD_REQUEST, &e),
    };

    // Validate extension
    if !filename.to_lowercase().ends_with(".wav") {
        return crate::json_error(StatusCode::BAD_REQUEST, "Only .wav files are supported");
    }

    // Validate and normalize WAV
    let data = match ensure_mono_wav(&file_data) {
        Ok(d) => d,
        Err(e) => return crate::json_error(StatusCode::BAD_REQUEST, &e),
    };

    // Validate duration
    let duration = match parse_wav_duration(&data) {
        Ok(d) => d,
        Err(e) => return crate::json_error(StatusCode::BAD_REQUEST, &e),
    };
    if duration > LOCK_CHIME_MAX_SECONDS {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            &format!(
                "Sound is {:.1} seconds -- must be {:.0} seconds or less",
                duration, LOCK_CHIME_MAX_SECONDS
            ),
        );
    }

    // Check size after conversion
    if data.len() > LOCK_CHIME_MAX_BYTES {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            &format!("File is too large ({} KB) -- max 1 MB", data.len() / 1024),
        );
    }

    let _ = std::fs::create_dir_all(LOCK_CHIME_DIR);

    // Sanitize and reject reserved name
    let base_name = sanitize_lock_chime_name(&filename);
    let stem_lower = Path::new(&base_name)
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .to_lowercase();
    if stem_lower == "lockchime" {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "File cannot be named \"lockchime\" -- please rename it before uploading",
        );
    }

    let (dest_path, final_name) = match deduplicate_filename(LOCK_CHIME_DIR, &base_name) {
        Some(r) => r,
        None => {
            return crate::json_error(StatusCode::CONFLICT, "Too many files with the same name");
        }
    };

    if let Err(_) = std::fs::write(&dest_path, &data) {
        return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to save file");
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "name": final_name,
            "duration": duration,
            "size": data.len(),
        })),
    )
}

/// POST /api/lockchime/activate/{filename}
pub async fn activate(
    State(_s): State<AppState>,
    AxumPath(filename): AxumPath<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !is_valid_lock_chime_file(&filename) {
        return crate::json_error(StatusCode::BAD_REQUEST, "Invalid filename");
    }

    let src_path = PathBuf::from(LOCK_CHIME_DIR).join(&filename);
    let clean_src = match src_path.canonicalize().ok() {
        Some(p) => p,
        None => {
            // If canonicalize fails, try the path directly
            src_path.clone()
        }
    };

    // Validate path is inside lock chime dir
    let clean_str = clean_src.to_string_lossy();
    if !clean_str.starts_with(LOCK_CHIME_DIR) && !src_path.starts_with(LOCK_CHIME_DIR) {
        return crate::json_error(StatusCode::BAD_REQUEST, "Invalid filename");
    }

    if !src_path.exists() {
        return crate::json_error(StatusCode::NOT_FOUND, "Sound file not found");
    }

    let data = match std::fs::read(&src_path) {
        Ok(d) => d,
        Err(_) => {
            return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read source file");
        }
    };

    if let Err(_) = write_chime_file_atomic(LOCK_CHIME_TARGET, &data) {
        return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to activate lock sound");
    }

    let _ = std::fs::write(LOCK_CHIME_ACTIVE_FILE, &filename);

    // Sync to cam disk in background
    tokio::spawn(async {
        if let Err(e) = sync_lock_chime_to_cam_disk().await {
            info!("lockchime: cam disk sync failed after activate: {}", e);
        }
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "active": filename,
            "usb_rebound": true,
        })),
    )
}

/// POST /api/lockchime/clear-active
pub async fn clear_active(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Err(e) = std::fs::remove_file(LOCK_CHIME_TARGET) {
        if e.kind() != std::io::ErrorKind::NotFound {
            return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to clear active sound");
        }
    }
    let _ = std::fs::remove_file(LOCK_CHIME_ACTIVE_FILE);
    let _ = std::process::Command::new("sync").status();

    // Clear from cam disk in background
    tokio::spawn(async {
        if let Err(e) = clear_lock_chime_from_cam_disk().await {
            info!("lockchime: cam disk clear failed: {}", e);
        }
    });

    crate::json_ok()
}

/// DELETE /api/lockchime/{filename}
pub async fn delete_chime(
    State(_s): State<AppState>,
    AxumPath(filename): AxumPath<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !is_valid_lock_chime_file(&filename) {
        return crate::json_error(StatusCode::BAD_REQUEST, "Invalid filename");
    }

    let dest_path = PathBuf::from(LOCK_CHIME_DIR).join(&filename);

    // Validate path is inside lock chime dir
    let clean_path = dest_path.to_string_lossy();
    if !clean_path.starts_with(LOCK_CHIME_DIR) {
        return crate::json_error(StatusCode::BAD_REQUEST, "Invalid filename");
    }

    match std::fs::remove_file(&dest_path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return crate::json_error(StatusCode::NOT_FOUND, "File not found");
        }
        Err(_) => {
            return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to delete file");
        }
    }

    // If the deleted file was the active chime, clear it
    if let Ok(data) = std::fs::read_to_string(LOCK_CHIME_ACTIVE_FILE) {
        if data.trim() == filename {
            let _ = std::fs::remove_file(LOCK_CHIME_TARGET);
            let _ = std::fs::remove_file(LOCK_CHIME_ACTIVE_FILE);
            let _ = std::process::Command::new("sync").status();
            tokio::spawn(async {
                if let Err(e) = clear_lock_chime_from_cam_disk().await {
                    info!("lockchime: cam disk clear after delete failed: {}", e);
                }
            });
        }
    }

    crate::json_ok()
}

/// GET /api/lockchime/random-config
pub async fn get_random_config(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let cfg = load_random_config();

    let has_rtc = Path::new("/dev/rtc0").exists();

    let has_ble = Path::new("/root/.ble/paired").exists() && !read_ble_vin().is_empty();

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "enabled": cfg.enabled,
            "mode": cfg.mode,
            "interval": cfg.interval,
            "hour": cfg.hour,
            "day": cfg.day,
            "has_rtc": has_rtc,
            "has_ble": has_ble,
        })),
    )
}

/// PUT /api/lockchime/random-config
pub async fn save_random_config(
    State(_s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let req: RandomConfig = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "Invalid JSON"),
    };

    // If scheduled or smart mode, verify RTC hardware
    if req.enabled && (req.mode == "scheduled" || req.mode == "smart") {
        if !Path::new("/dev/rtc0").exists() {
            let mode_name = if req.mode == "smart" { "Smart" } else { "Scheduled" };
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                &format!("{} mode requires a working RTC (Pi 5 with battery)", mode_name),
            );
        }
    }

    // Smart mode requires both RTC and BLE
    if req.enabled && req.mode == "smart" {
        if !Path::new("/dev/rtc0").exists() {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "Smart mode requires a working RTC (Pi 5 with battery)",
            );
        }
        if !Path::new("/root/.ble/paired").exists() {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "Smart mode requires a paired BLE key -- pair your Pi in Settings first",
            );
        }
        if read_ble_vin().is_empty() {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "Smart mode requires a VIN configured for BLE",
            );
        }
    }

    if let Err(e) = save_random_config_to_disk(&req) {
        return crate::json_error(StatusCode::BAD_REQUEST, &e);
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "enabled": req.enabled,
            "mode": req.mode,
            "interval": req.interval,
            "hour": req.hour,
            "day": req.day,
        })),
    )
}

/// POST /api/lockchime/randomize
pub async fn randomize(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let chosen = pick_and_activate_random();
    if chosen.is_empty() {
        return crate::json_error(StatusCode::BAD_REQUEST, "No sounds in library to randomize");
    }

    lock_chime_log(&format!("manual randomize -- changed lock chime to {:?}", chosen));

    // Sync to cam disk in background
    tokio::spawn(async {
        match sync_lock_chime_to_cam_disk().await {
            Ok(()) => lock_chime_log("manual randomize -- cam disk sync OK"),
            Err(e) => {
                info!("lockchime: cam disk sync failed after manual randomize: {}", e);
                lock_chime_log(&format!("manual randomize -- cam disk sync FAILED: {}", e));
            }
        }
    });

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "active": chosen,
            "usb_rebound": true,
        })),
    )
}

/// POST /api/lockchime/randomize-on-connect
pub async fn randomize_on_connect(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let cfg = load_random_config();
    if !cfg.enabled || cfg.mode != "on_connect" {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "skipped": true,
                "reason": "random on_connect mode not active",
            })),
        );
    }

    let chosen = pick_and_activate_random();
    if chosen.is_empty() {
        return crate::json_error(StatusCode::BAD_REQUEST, "No sounds in library to randomize");
    }

    lock_chime_log(&format!(
        "on_connect mode (archiveloop) -- changed lock chime to {:?}",
        chosen
    ));

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "success": true,
            "active": chosen,
        })),
    )
}

/// GET /api/lockchime/ble-shift-state
pub async fn ble_shift_state(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match query_ble_shift_state().await {
        Err(e) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "success": false,
                "error": e,
            })),
        ),
        Ok(state) => {
            let label = match state.as_str() {
                "P" => "Park",
                "D" => "Drive",
                "R" => "Reverse",
                "N" => "Neutral",
                "SNA" => "Not Available",
                _ => &state,
            };

            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "success": true,
                    "shift_state": state,
                    "label": label,
                })),
            )
        }
    }
}

/// Called when the USB gadget is enabled (drive mounted). Randomizes the lock
/// sound if random mode is enabled with on_connect mode.
pub fn randomize_on_connect_sync() {
    let cfg = load_random_config();
    if !cfg.enabled || cfg.mode != "on_connect" {
        return;
    }
    let chosen = pick_and_activate_random();
    if !chosen.is_empty() {
        lock_chime_log(&format!("on_connect mode -- changed lock chime to {:?}", chosen));
    }
}

// ---------------------------------------------------------------------------
// Simple multipart parser (for upload handler)
// ---------------------------------------------------------------------------

/// Extracts filename and file data from a multipart/form-data body.
/// Looks for a part with name "file".
fn parse_multipart_wav(raw: &[u8]) -> Result<(String, Vec<u8>), String> {
    // Find boundary from first line (starts with --)
    let first_newline = raw.iter().position(|&b| b == b'\r' || b == b'\n')
        .ok_or_else(|| "Missing file field".to_string())?;
    let boundary = &raw[..first_newline];

    if boundary.len() < 3 || !boundary.starts_with(b"--") {
        return Err("Missing file field".to_string());
    }

    // Split body by boundary
    let mut parts = Vec::new();
    let mut start = 0;
    while let Some(pos) = find_subsequence(&raw[start..], boundary) {
        if start > 0 {
            // The content between the previous boundary end and this boundary start
            let part_end = start + pos;
            if part_end > start {
                parts.push(&raw[start..part_end]);
            }
        }
        start = start + pos + boundary.len();
        // Skip \r\n after boundary
        if start < raw.len() && raw[start] == b'\r' {
            start += 1;
        }
        if start < raw.len() && raw[start] == b'\n' {
            start += 1;
        }
    }

    for part in &parts {
        // Find header/body separator (double newline)
        let header_end = find_double_newline(part);
        if header_end.is_none() {
            continue;
        }
        let header_end = header_end.unwrap();

        let header_bytes = &part[..header_end];
        let header_str = String::from_utf8_lossy(header_bytes);

        // Check if this is the "file" field
        if !header_str.contains("name=\"file\"") {
            continue;
        }

        // Extract filename from Content-Disposition
        let filename = extract_filename_from_header(&header_str)
            .unwrap_or_else(|| "upload.wav".to_string());

        // Body starts after the double newline
        let body_start = header_end + if part[header_end..].starts_with(b"\r\n\r\n") {
            4
        } else if part[header_end..].starts_with(b"\n\n") {
            2
        } else {
            4
        };

        let mut body = &part[body_start..];
        // Trim trailing \r\n before next boundary
        if body.len() >= 2 && body[body.len() - 2] == b'\r' && body[body.len() - 1] == b'\n' {
            body = &body[..body.len() - 2];
        }

        return Ok((filename, body.to_vec()));
    }

    Err("Missing file field".to_string())
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn find_double_newline(data: &[u8]) -> Option<usize> {
    // Find \r\n\r\n or \n\n
    for i in 0..data.len().saturating_sub(3) {
        if data[i] == b'\r' && data[i + 1] == b'\n' && data[i + 2] == b'\r' && data[i + 3] == b'\n' {
            return Some(i);
        }
    }
    for i in 0..data.len().saturating_sub(1) {
        if data[i] == b'\n' && data[i + 1] == b'\n' {
            return Some(i);
        }
    }
    None
}

fn extract_filename_from_header(header: &str) -> Option<String> {
    // Look for filename="..." or filename*=UTF-8''...
    for part in header.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix("filename=\"") {
            if let Some(end) = rest.find('"') {
                let name = &rest[..end];
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}
