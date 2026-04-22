//! OTA update: check for updates, run update, version info.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;
use crate::status::get_sbc_model;

static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

/// Salt for the telemetry fingerprint hash. Must match Go `telemetrySalt`.
const TELEMETRY_SALT: &str = "SENTRYUSB_2026_PROD";

/// SHA-256 hash of a stable hardware identifier + salt. Uses the SBC serial
/// number (survives reflash) with fallback to machine-id. Cached.
/// Mirrors Go `getFingerprint` (server/api/update.go:42-82).
pub(crate) fn get_fingerprint() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| {
        use ring::digest::{SHA256, digest};
        let mut id = String::new();
        for p in [
            "/sys/firmware/devicetree/base/serial-number",
            "/proc/device-tree/serial-number",
        ] {
            if let Ok(raw) = std::fs::read_to_string(p) {
                let trimmed = raw.trim_matches(|c: char| c == '\0' || c.is_whitespace());
                if !trimmed.is_empty() {
                    id = trimmed.to_string();
                    break;
                }
            }
        }
        if id.is_empty() {
            for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
                if let Ok(raw) = std::fs::read_to_string(p) {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        id = trimmed.to_string();
                        break;
                    }
                }
            }
        }
        if id.is_empty() {
            tracing::warn!("[telemetry] no fingerprint source available");
            return String::new();
        }
        let h = digest(&SHA256, format!("{}{}", id, TELEMETRY_SALT).as_bytes());
        hex::encode(h.as_ref())
    })
    .as_str()
}

/// GET /api/system/check-internet
pub async fn check_internet(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let connected = sentryusb_shell::run("ping", &["-c", "1", "-W", "3", "8.8.8.8"]).await.is_ok();
    (StatusCode::OK, Json(serde_json::json!({"connected": connected})))
}

/// POST /api/system/update
///
/// Downloads the latest binary from the Sentry-USB-Rusty GitHub releases and
/// replaces the running binary.  A service restart is needed to apply.
pub async fn run_update(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if UPDATE_RUNNING.swap(true, Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Update already in progress");
    }

    let hub = s.hub.clone();
    tokio::spawn(async move {
        hub.broadcast("update", &serde_json::json!({"status": "running"}));

        let result = self_update().await;

        UPDATE_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(msg) => hub.broadcast("update", &serde_json::json!({"status": "complete", "output": msg})),
            Err(e) => hub.broadcast("update", &serde_json::json!({"status": "error", "error": e.to_string()})),
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

const UPDATE_REPO: &str = "Scottmg1/Sentry-USB-Rusty";

async fn self_update() -> anyhow::Result<String> {
    let arch = sentryusb_shell::run("uname", &["-m"]).await?;
    let suffix = match arch.trim() {
        "aarch64" => "linux-arm64",
        "armv7l" => "linux-armv7",
        "armv6l" => "linux-armv6",
        "x86_64" => "linux-amd64",
        other => anyhow::bail!("unsupported architecture: {}", other),
    };

    let url = format!(
        "https://github.com/{}/releases/latest/download/sentryusb-{}",
        UPDATE_REPO, suffix
    );

    // Remount root read-write
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    let tmp = "/tmp/sentryusb-update";
    sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(120),
        "curl", &["-fsSL", &url, "-o", tmp],
    ).await?;

    sentryusb_shell::run("chmod", &["+x", tmp]).await?;
    sentryusb_shell::run("mv", &[tmp, "/opt/sentryusb/sentryusb"]).await?;

    // Fetch version tag
    let tag_cmd = format!(
        "curl -fsSL --max-time 10 https://api.github.com/repos/{}/releases/latest 2>/dev/null \
         | grep '\"tag_name\"' | head -1 | sed 's/.*\"tag_name\": *\"\\([^\"]*\\)\".*/\\1/'",
        UPDATE_REPO
    );
    let tag = sentryusb_shell::run("bash", &["-c", &tag_cmd]).await.unwrap_or_default();
    let tag = tag.trim();

    if !tag.is_empty() {
        let _ = std::fs::write("/opt/sentryusb/version", tag);
    }

    Ok(format!("Updated to {}.  Restart the service to apply.", if tag.is_empty() { "latest" } else { tag }))
}

/// GET /api/system/version
pub async fn get_version(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let version = env!("CARGO_PKG_VERSION");
    let sbc_model = get_sbc_model();

    // Read installed version tag if available (installer writes it here).
    let installed = std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .unwrap_or_else(|_| version.to_string());

    (StatusCode::OK, Json(serde_json::json!({
        "version": installed.trim(),
        "binary_version": version,
        "sbc_model": sbc_model,
    })))
}

/// Parse semver string like "v1.2.3" or "v1.2.3-beta.1" → (major, minor, patch, prerelease).
/// Matches Go `parseSemver` exactly so the two implementations agree on edge cases.
pub(crate) fn parse_semver(v: &str) -> Option<(u32, u32, u32, String)> {
    let v = v.trim().trim_start_matches('v');
    let (base, pre) = match v.find('-') {
        Some(i) => (&v[..i], v[i + 1..].to_string()),
        None => (v, String::new()),
    };
    let parts: Vec<&str> = base.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let mut nums = [0u32; 3];
    for (i, p) in parts.iter().take(3).enumerate() {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        nums[i] = p.parse().ok()?;
    }
    Some((nums[0], nums[1], nums[2], pre))
}

/// True if `candidate` is newer than `current`. Prerelease-aware:
/// stable beats prerelease at the same base version.
pub(crate) fn is_version_newer(candidate: &str, current: &str) -> bool {
    let c = parse_semver(candidate);
    let u = parse_semver(current);
    let (c, u) = match (c, u) {
        (Some(c), Some(u)) => (c, u),
        _ => return candidate.trim() != current.trim(),
    };
    if c.0 != u.0 {
        return c.0 > u.0;
    }
    if c.1 != u.1 {
        return c.1 > u.1;
    }
    if c.2 != u.2 {
        return c.2 > u.2;
    }
    match (u.3.is_empty(), c.3.is_empty()) {
        (true, true) => false,
        (false, true) => true,   // user on prerelease, candidate stable → newer
        (true, false) => false,  // user on stable, candidate prerelease → older
        (false, false) => c.3 > u.3,
    }
}

fn read_current_version() -> String {
    std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// POST /api/system/check-update
pub async fn check_for_update(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let cmd = format!(
        "curl -fsSL --max-time 10 https://api.github.com/repos/{}/releases/latest 2>/dev/null \
         | grep -o '\"tag_name\": *\"[^\"]*\"' | head -1",
        UPDATE_REPO
    );
    match sentryusb_shell::run("bash", &["-c", &cmd]).await {
        Ok(output) => {
            let latest = output.trim()
                .trim_start_matches("\"tag_name\":")
                .trim()
                .trim_matches('"')
                .to_string();
            let current = read_current_version();
            let available = !latest.is_empty() && is_version_newer(&latest, &current);

            // Fire-and-forget telemetry so support server can track install base.
            let cur_clone = current.clone();
            let lat_clone = latest.clone();
            tokio::spawn(async move {
                send_telemetry(&cur_clone, available, &lat_clone).await;
            });

            (StatusCode::OK, Json(serde_json::json!({
                "available": available,
                "latest": latest,
                "current": current,
            })))
        }
        Err(_) => (StatusCode::OK, Json(serde_json::json!({"available": false, "error": "could not check"}))),
    }
}

/// POST {fingerprint, current_version, update_available, new_version, arch, model}
/// to the telemetry endpoint. Best-effort — errors are logged, never surfaced.
pub async fn send_telemetry(current: &str, update_available: bool, new_version: &str) {
    let fp = get_fingerprint();
    if fp.is_empty() {
        return;
    }
    let arch = sentryusb_shell::run("uname", &["-m"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());
    let payload = serde_json::json!({
        "fingerprint": fp,
        "current_version": current,
        "update_available": update_available,
        "new_version": new_version,
        "arch": arch,
        "model": get_sbc_model(),
    });
    let url = format!("https://api.sentry-six.com/sentryusb/telemetry");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    match client.post(&url).json(&payload).send().await {
        Ok(r) => tracing::info!("[telemetry] sent (status {})", r.status()),
        Err(e) => tracing::warn!("[telemetry] failed: {}", e),
    }
}

/// Called once at startup to announce this device + current version.
pub fn spawn_startup_telemetry() {
    tokio::spawn(async move {
        let current = read_current_version();
        send_telemetry(&current, false, "").await;
    });
}

/// GET /api/system/update-status
pub async fn get_update_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let running = UPDATE_RUNNING.load(Ordering::Relaxed);
    (StatusCode::OK, Json(serde_json::json!({
        "status": if running { "running" } else { "idle" },
    })))
}
