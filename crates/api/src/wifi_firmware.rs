//! Wi-Fi firmware updater for the Broadcom/Cypress CYW43455 combo radio.
//!
//! Raspberry Pi packages firmware 7.45.265 (built Aug 2023). Infineon ships a
//! newer 7.45.286 (built Oct 2024) in its own repository, which Raspberry Pi
//! cannot redistribute because it sits under a licence Infineon explicitly did
//! not upstream to linux-firmware.git.
//!
//! Why we offer it: on a Pi 5 the older firmware can wedge mid-archive. The
//! kernel logs a burst of `CMD53 sg block write failed -84` plus
//! `max tx seq number error`, after which the radio stays associated with a
//! strong signal and normal receive speed while transmit collapses — and
//! Bluetooth dies with it, because the two share one antenna through the
//! chip's coexistence arbiter. Worse, reloading the *old* firmware only gets
//! transmit back to a fraction of normal: measured on a Pi 5 archiving over
//! Wi-Fi, 7.45.265 stayed at ~40 Mbit/s across two reloads on a device that
//! had been sustaining ~190 Mbit/s, while loading 7.45.286 restored full speed
//! immediately.
//!
//! The install is deliberately survivable. Reloading the radio drops Wi-Fi for
//! ~20 s, which kills the very HTTP connection that started the install, so
//! the work runs detached and every step is persisted to `/mutable` where the
//! UI can pick it back up once it reconnects. If the radio does not come back,
//! the previous blob is restored and the radio reloaded again, so a bad image
//! can never strand the device off the network.
//!
//! The new image is also pinned with `dpkg-divert`, because the file belongs to
//! the `firmware-brcm80211` package and any later `apt upgrade` of it would
//! otherwise quietly restore 7.45.265 and reintroduce the fault. The diversion
//! sends the packaged copy to a `.distrib` sidecar and leaves ours at the path
//! the driver loads; removing the diversion puts the packaged file back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::router::AppState;

/// Infineon's newest published CYW43455 build.
const TARGET_VERSION: &str = "7.45.286";
/// Pinned to a release tag, never a moving branch.
const FW_URL: &str = "https://raw.githubusercontent.com/Infineon/ifx-linux-firmware/release-v6.1.145-2026_0108/firmware/cyfmac43455-sdio.bin";
const FW_SHA256: &str = "eaff8d2b6d2501bb5c477ba343900c7487af915898eac13bc91b33b1285dadce";
const FW_SIZE: u64 = 616_233;

/// The symlink the driver follows. The real file it lands on varies by distro,
/// so it is always resolved rather than assumed.
const FW_LINK: &str = "/usr/lib/firmware/brcm/brcmfmac43455-sdio.bin";
const BACKUP_DIR: &str = "/mutable/wifi-firmware";
const STATE_FILE: &str = "/mutable/wifi-firmware/state.json";

/// Guards a second install starting mid-flight — the first is holding the
/// radio down and the two would fight over the same file.
static INSTALL_RUNNING: AtomicBool = AtomicBool::new(false);

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstallState {
    /// idle | running | success | failed | rolled_back
    pub state: String,
    pub step: String,
    pub progress: u8,
    pub message: String,
    pub updated_at: i64,
}

impl Default for InstallState {
    fn default() -> Self {
        InstallState {
            state: "idle".into(),
            step: String::new(),
            progress: 0,
            message: String::new(),
            updated_at: 0,
        }
    }
}

fn read_state() -> InstallState {
    std::fs::read_to_string(STATE_FILE)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist progress where it survives the Wi-Fi drop, and mirror it to any
/// still-connected WebSocket client so the progress bar moves live.
fn set_state(hub: &sentryusb_ws::Hub, state: &str, step: &str, progress: u8, message: &str) {
    let s = InstallState {
        state: state.into(),
        step: step.into(),
        progress,
        message: message.into(),
        updated_at: chrono::Utc::now().timestamp(),
    };
    let _ = std::fs::create_dir_all(BACKUP_DIR);
    if let Ok(json) = serde_json::to_string(&s) {
        let tmp = format!("{}.tmp", STATE_FILE);
        if std::fs::write(&tmp, &json).is_ok() {
            let _ = std::fs::rename(&tmp, STATE_FILE);
        }
    }
    hub.broadcast("wifi_firmware_status", &s);
    info!("[wifi-fw] {} {}% {}", step, progress, message);
}

// ── board / firmware detection ────────────────────────────────────────────

fn board_model() -> String {
    for p in [
        "/proc/device-tree/model",
        "/sys/firmware/devicetree/base/model",
    ] {
        if let Ok(s) = std::fs::read_to_string(p) {
            let s = s.trim_end_matches('\0').trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    String::new()
}

/// Only the Pi 5 is offered this. The Pi 3B+/4/CM4 carry the same CYW43455,
/// but the failure and the fix have only been characterised on a Pi 5, so they
/// are left alone until that changes.
fn is_pi5() -> bool {
    board_model().to_lowercase().contains("raspberry pi 5")
}

/// Resolve the symlink chain to the file the driver actually loads.
fn resolve_fw_path() -> Option<PathBuf> {
    std::fs::canonicalize(FW_LINK).ok().filter(|p| p.is_file())
}

/// dpkg parks the packaged file here once the path is diverted, so its
/// presence is a reliable "our image is pinned" signal.
fn distrib_path(fw_path: &Path) -> PathBuf {
    let mut p = fw_path.as_os_str().to_os_string();
    p.push(".distrib");
    PathBuf::from(p)
}

fn is_pinned(fw_path: &Path) -> bool {
    distrib_path(fw_path).exists()
}

/// Divert the packaged path so `apt upgrade` of firmware-brcm80211 can never
/// overwrite the firmware we install. `--rename` moves the current file aside
/// to `.distrib`, which is why the backup is taken before this runs.
async fn pin_firmware(fw_path: &Path) -> bool {
    if is_pinned(fw_path) {
        return true;
    }
    let path = fw_path.to_string_lossy().to_string();
    match sentryusb_shell::run("dpkg-divert", &["--local", "--rename", "--add", &path]).await {
        Ok(_) => {
            info!("[wifi-fw] pinned {} against package upgrades", path);
            true
        }
        Err(e) => {
            // Non-Debian hosts have no dpkg-divert; the install still works,
            // it just isn't protected from a future package upgrade.
            warn!("[wifi-fw] could not pin firmware ({e}) — an apt upgrade may revert it");
            false
        }
    }
}

/// Undo the diversion, putting the distribution's own file back at the path.
async fn unpin_firmware(fw_path: &Path) {
    if !is_pinned(fw_path) {
        return;
    }
    // `--rename --remove` refuses to clobber, so our image has to go first.
    let _ = std::fs::remove_file(fw_path);
    let path = fw_path.to_string_lossy().to_string();
    if let Err(e) =
        sentryusb_shell::run("dpkg-divert", &["--local", "--rename", "--remove", &path]).await
    {
        warn!("[wifi-fw] could not remove the firmware diversion: {e}");
    }
}

/// Pull the `Version: 7.45.xxx` banner out of a firmware image. The blob keeps
/// it as plain text, so a bounded scan beats parsing the container format.
fn version_in_blob(path: &Path) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    let needle = b"Version: ";
    let i = data.windows(needle.len()).position(|w| w == needle)?;
    let rest = &data[i + needle.len()..];
    let end = rest.iter().position(|c| !c.is_ascii_graphic())?;
    std::str::from_utf8(&rest[..end])
        .ok()
        .map(|s| s.to_string())
}

/// The version the radio is running right now, from the driver's boot banner.
async fn running_version() -> Option<String> {
    let out = sentryusb_shell::run("dmesg", &[]).await.ok()?;
    let line = out
        .lines()
        .filter(|l| l.contains("preinit_dcmds: Firmware"))
        .next_back()?;
    let idx = line.find("version ")?;
    line[idx + "version ".len()..]
        .split_whitespace()
        .next()
        .map(|s| s.to_string())
}

/// Look for the signature of the wedge in the kernel log, so the warning can
/// say "this happened to you" rather than "this might happen to you".
async fn symptom_detected() -> Option<String> {
    let out = sentryusb_shell::run("dmesg", &[]).await.ok()?;
    let hits = out
        .lines()
        .filter(|l| {
            l.contains("brcmfmac")
                && (l.contains("CMD53")
                    || l.contains("brcmf_sdio_txfail")
                    || l.contains("max tx seq number error")
                    || l.contains("RXHEADER FAILED"))
        })
        .count();
    if hits == 0 {
        return None;
    }
    Some(format!(
        "{} Wi-Fi bus error{} in this boot's kernel log",
        hits,
        if hits == 1 { "" } else { "s" }
    ))
}

// ── status ────────────────────────────────────────────────────────────────

/// GET /api/system/wifi-firmware
pub async fn get_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let model = board_model();
    let pi5 = is_pi5();
    let fw_path = resolve_fw_path();
    let installed = fw_path.as_deref().and_then(version_in_blob);
    let running = running_version().await;
    let symptom = symptom_detected().await;

    let on_target =
        installed.as_deref() == Some(TARGET_VERSION) || running.as_deref() == Some(TARGET_VERSION);
    // Eligible only where the work can actually be done: a Pi 5 whose firmware
    // file resolved, not already on the newer build.
    let eligible = pi5 && fw_path.is_some() && !on_target;

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "eligible": eligible,
            "supported_board": pi5,
            "model": model,
            "running_version": running,
            "installed_version": installed,
            "target_version": TARGET_VERSION,
            "up_to_date": on_target,
            "symptom_detected": symptom.is_some(),
            "symptom_detail": symptom,
            "can_rollback": Path::new(&format!("{}/stock.bin", BACKUP_DIR)).exists(),
            "pinned": fw_path.as_deref().map(is_pinned).unwrap_or(false),
            "install": read_state(),
        })),
    )
}

// ── install ───────────────────────────────────────────────────────────────

/// POST /api/system/wifi-firmware/install
pub async fn install(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if !is_pi5() {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "This update only applies to the Raspberry Pi 5.",
        );
    }
    let Some(fw_path) = resolve_fw_path() else {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "Could not locate the Wi-Fi firmware file on this system.",
        );
    };
    if INSTALL_RUNNING.swap(true, Ordering::SeqCst) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "A firmware install is already running.",
        );
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        if let Err(e) = run_install(&hub, &fw_path).await {
            warn!("[wifi-fw] install failed: {e}");
            set_state(&hub, "failed", "failed", 100, &e.to_string());
        }
        INSTALL_RUNNING.store(false, Ordering::SeqCst);
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "started": true,
            "note": "Wi-Fi drops for about 20 seconds while the radio reloads."
        })),
    )
}

async fn run_install(hub: &sentryusb_ws::Hub, fw_path: &Path) -> anyhow::Result<()> {
    set_state(hub, "running", "download", 5, "Downloading firmware…");

    let client = reqwest::Client::builder()
        .user_agent(concat!(
            "sentryusb-wifi-firmware/",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_secs(120))
        .build()?;
    let bytes = client
        .get(FW_URL)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    set_state(hub, "running", "verify", 30, "Verifying download…");
    if bytes.len() as u64 != FW_SIZE {
        anyhow::bail!(
            "unexpected firmware size {} (expected {})",
            bytes.len(),
            FW_SIZE
        );
    }
    let sum = hex::encode(ring::digest::digest(&ring::digest::SHA256, &bytes).as_ref());
    if sum != FW_SHA256 {
        anyhow::bail!("firmware checksum mismatch — refusing to install");
    }

    set_state(hub, "running", "backup", 45, "Backing up current firmware…");
    let _ = sentryusb_shell::run("mount", &["-o", "remount,rw", "/"]).await;
    std::fs::create_dir_all(BACKUP_DIR)?;
    let stock = format!("{}/stock.bin", BACKUP_DIR);
    // Only ever capture the *original* image, so repeated runs can't overwrite
    // the rollback target with the new firmware.
    if !Path::new(&stock).exists() {
        std::fs::copy(fw_path, &stock)?;
    }

    set_state(hub, "running", "install", 55, "Installing firmware…");
    // Pin before writing: the diversion renames the packaged file out of the
    // way, so writing first would only get that write moved aside.
    let pinned = pin_firmware(fw_path).await;
    std::fs::write(fw_path, &bytes)?;
    let _ = sentryusb_shell::run("sync", &[]).await;
    if !pinned {
        warn!("[wifi-fw] firmware installed but not pinned — a package upgrade may revert it");
    }

    set_state(hub, "running", "reload", 65, "Reloading the Wi-Fi radio…");
    reload_radio().await?;

    set_state(
        hub,
        "running",
        "wait",
        80,
        "Waiting for Wi-Fi to come back…",
    );
    if wait_for_wifi().await {
        let ver = running_version()
            .await
            .unwrap_or_else(|| TARGET_VERSION.to_string());
        // The reload restores the driver default (power save on).
        let _ = sentryusb_shell::run("iw", &["dev", "wlan0", "set", "power_save", "off"]).await;
        set_state(
            hub,
            "success",
            "done",
            100,
            &format!("Wi-Fi firmware {} is now running.", ver),
        );
        return Ok(());
    }

    // The radio did not come back — restore the old image and reload again.
    set_state(
        hub,
        "running",
        "rollback",
        90,
        "Wi-Fi did not return — restoring previous firmware…",
    );
    unpin_firmware(fw_path).await;
    std::fs::copy(&stock, fw_path)?;
    let _ = sentryusb_shell::run("sync", &[]).await;
    let _ = reload_radio().await;
    let recovered = wait_for_wifi().await;
    set_state(
        hub,
        "rolled_back",
        "rollback",
        100,
        if recovered {
            "The new firmware did not work. The previous version was restored and Wi-Fi is back."
        } else {
            "The new firmware did not work. The previous version was restored — please reboot the Pi."
        },
    );
    Ok(())
}

/// Ask brcmfmac to re-probe the chip, which reloads the firmware image from
/// disk. The phy index increments on every reprobe (phy0 → phy1 → …), so it is
/// resolved fresh each time rather than remembered.
async fn reload_radio() -> anyhow::Result<()> {
    let phy = std::fs::read_to_string("/sys/class/net/wlan0/phy80211/name")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if phy.is_empty() {
        anyhow::bail!("could not determine the Wi-Fi phy to reload");
    }
    let node = format!("/sys/kernel/debug/ieee80211/{}/reset", phy);
    if !Path::new(&node).exists() {
        anyhow::bail!("this kernel does not expose the Wi-Fi reset control");
    }
    std::fs::write(&node, b"1")?;
    Ok(())
}

/// Associated *and* actually passing traffic — association alone can come back
/// while the link is unusable, which is the whole failure we're fixing.
async fn wait_for_wifi() -> bool {
    tokio::time::sleep(Duration::from_secs(6)).await;
    for _ in 0..45 {
        let linked = sentryusb_shell::run("iw", &["dev", "wlan0", "link"])
            .await
            .map(|o| o.contains("Connected to"))
            .unwrap_or(false);
        if linked {
            let gw = sentryusb_shell::run(
                "bash",
                &[
                    "-c",
                    "ping -c1 -W2 -q $(ip route show default | awk 'NR==1{print $3}')",
                ],
            )
            .await;
            if gw.is_ok() {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    false
}

/// POST /api/system/wifi-firmware/rollback
pub async fn rollback(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let Some(fw_path) = resolve_fw_path() else {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "Could not locate the Wi-Fi firmware file.",
        );
    };
    let stock = format!("{}/stock.bin", BACKUP_DIR);
    if !Path::new(&stock).exists() {
        return crate::json_error(
            StatusCode::BAD_REQUEST,
            "No previous firmware is saved on this device.",
        );
    }
    if INSTALL_RUNNING.swap(true, Ordering::SeqCst) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "A firmware operation is already running.",
        );
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        set_state(
            &hub,
            "running",
            "rollback",
            40,
            "Restoring the previous firmware…",
        );
        let _ = sentryusb_shell::run("mount", &["-o", "remount,rw", "/"]).await;
        // Hand the path back to dpkg before restoring, so future upgrades of
        // firmware-brcm80211 manage this file normally again.
        unpin_firmware(&fw_path).await;
        let ok = std::fs::copy(&stock, &fw_path).is_ok();
        let _ = sentryusb_shell::run("sync", &[]).await;
        if ok {
            set_state(&hub, "running", "reload", 70, "Reloading the Wi-Fi radio…");
            let _ = reload_radio().await;
            let back = wait_for_wifi().await;
            set_state(
                &hub,
                "success",
                "done",
                100,
                if back {
                    "Previous Wi-Fi firmware restored."
                } else {
                    "Previous Wi-Fi firmware restored — please reboot the Pi."
                },
            );
        } else {
            set_state(
                &hub,
                "failed",
                "rollback",
                100,
                "Could not restore the previous firmware.",
            );
        }
        INSTALL_RUNNING.store(false, Ordering::SeqCst);
    });

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({"started": true})),
    )
}
