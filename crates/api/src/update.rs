//! OTA update: check for updates, run update, version info.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;

use crate::router::AppState;
use crate::status::get_sbc_model;

/// Cache file written by `check_for_update`, read by `get_update_status` so
/// the Settings page can render last-check results on load without forcing
/// a network round-trip. Path matches Go's `getUpdateStatus`.
const UPDATE_CHECK_CACHE: &str = "/tmp/sentryusb-update-check.json";

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
        hub.broadcast("update_status", &serde_json::json!({"status": "running"}));

        let result = self_update().await;

        UPDATE_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(msg) => hub.broadcast("update_status", &serde_json::json!({"status": "complete", "output": msg})),
            Err(e) => hub.broadcast("update_status", &serde_json::json!({"status": "error", "error": e.to_string()})),
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
///
/// Fetches the GitHub "latest release" JSON via reqwest and parses it
/// properly. The previous implementation shelled to `curl | grep | head`
/// which hid curl failures (pipeline exit code is `head`'s, always 0
/// on empty input) — a 403 rate limit or DNS blip would silently
/// return `available: false` and the UI would tell the user they were
/// up to date when they weren't.
///
/// The response shape carries both the simple fields (`available`,
/// `latest`, `current`) kept for backward compatibility with earlier
/// Rust clients **and** the richer fields the current web UI reads
/// (`update_available`, `latest_version`, `release_url`,
/// `release_notes`). Settings.tsx checks for `data.update_available`
/// / `data.latest_version`; without them the UI defaults to "up to
/// date" regardless of the actual result. This was the root cause of
/// the user-reported "update never appears" bug even when the backend
/// correctly found a newer release.
pub async fn check_for_update(
    State(_s): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let current = read_current_version();
    let can_update = !current.is_empty() && current != "dev";

    // Include prereleases if requested via query param OR if the user's
    // update_channel preference is set to "prerelease". Mirrors Go's
    // checkForUpdate (server/api/update.go:501-506).
    let mut include_prerelease = params.get("include_prerelease").map(String::as_str) == Some("true");
    if !include_prerelease {
        let prefs = crate::preferences::load_prefs();
        if prefs.get("update_channel").and_then(|v| v.as_str()) == Some("prerelease") {
            include_prerelease = true;
        }
    }

    let releases = match fetch_releases().await {
        Ok(rs) => rs,
        Err(msg) => {
            // Fire a basic telemetry heartbeat so the support server still sees
            // the device when GitHub is unreachable, matching Go's defer block.
            let cur_clone = current.clone();
            tokio::spawn(async move { send_telemetry(&cur_clone, false, "").await });

            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "available": false,
                    "update_available": false,
                    "error": msg,
                })),
            );
        }
    };

    let (latest_stable, latest_prerelease) = find_latest_releases(&releases);

    // current_version + checked_at top-level matches Go's response.
    let mut result = serde_json::json!({
        "current_version": current,
        "checked_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });

    let mut new_stable_version = String::new();

    if let Some(stable) = latest_stable {
        let stable_available = can_update && is_version_newer(&stable.tag_name, &current);
        result["update_available"] = serde_json::Value::Bool(stable_available);
        result["latest_version"] = serde_json::Value::String(stable.tag_name.clone());
        result["release_url"] = serde_json::Value::String(stable.html_url.clone());
        result["release_notes"] = serde_json::Value::String(stable.body.clone());
        result["stable"] = serde_json::json!({
            "version": stable.tag_name,
            "release_url": stable.html_url,
            "release_notes": stable.body,
            "available": stable_available,
        });
        if stable_available {
            new_stable_version = stable.tag_name.clone();
        }
    } else {
        result["update_available"] = serde_json::Value::Bool(false);
    }

    if include_prerelease {
        if let Some(pre) = latest_prerelease {
            let pre_available = can_update && is_version_newer(&pre.tag_name, &current);
            result["prerelease"] = serde_json::json!({
                "version": pre.tag_name,
                "release_url": pre.html_url,
                "release_notes": pre.body,
                "available": pre_available,
            });
        }
    }

    // Cache the result so the Settings page load can render last-check info
    // without re-hitting GitHub. Mirrors Go's writeFile at update.go:578.
    if let Ok(data) = serde_json::to_vec(&result) {
        let _ = std::fs::write(UPDATE_CHECK_CACHE, data);
    }

    // Telemetry — only report stable updates, never prereleases.
    let cur_clone = current.clone();
    let new_ver_clone = new_stable_version.clone();
    tokio::spawn(async move {
        send_telemetry(&cur_clone, !new_ver_clone.is_empty(), &new_ver_clone).await;
    });

    (StatusCode::OK, Json(result))
}

/// Minimal release info parsed from a GitHub release object.
#[derive(Clone)]
struct ReleaseInfo {
    tag_name: String,
    html_url: String,
    body: String,
    prerelease: bool,
}

/// Fetch the most recent releases (stable + prerelease) from GitHub.
async fn fetch_releases() -> Result<Vec<ReleaseInfo>, String> {
    let url = format!("https://api.github.com/repos/{}/releases?per_page=20", UPDATE_REPO);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("sentryusb-updater/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client init failed: {}", e))?;

    let resp = client.get(&url).send().await.map_err(|e| {
        if e.is_timeout() {
            "GitHub API request timed out".to_string()
        } else if e.is_connect() {
            format!("could not reach GitHub: {}", e)
        } else {
            format!("GitHub API request failed: {}", e)
        }
    })?;

    let status = resp.status();
    if !status.is_success() {
        return Err(if status.as_u16() == 403 || status.as_u16() == 429 {
            "GitHub API rate limit hit — wait about an hour and try again".to_string()
        } else {
            format!("GitHub API returned HTTP {}", status)
        });
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API returned unparseable JSON: {}", e))?;

    let arr = body
        .as_array()
        .ok_or_else(|| "GitHub API response was not an array".to_string())?;

    Ok(arr
        .iter()
        .map(|v| ReleaseInfo {
            tag_name: v.get("tag_name").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            html_url: v.get("html_url").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            body: v.get("body").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            prerelease: v.get("prerelease").and_then(|s| s.as_bool()).unwrap_or(false),
        })
        .filter(|r| !r.tag_name.is_empty())
        .collect())
}

/// Pick the first stable and the first prerelease from the list. Mirrors
/// Go's `findLatestReleases` — assumes the GitHub API returns releases in
/// publish-newest-first order.
fn find_latest_releases(releases: &[ReleaseInfo]) -> (Option<&ReleaseInfo>, Option<&ReleaseInfo>) {
    let mut stable: Option<&ReleaseInfo> = None;
    let mut prerelease: Option<&ReleaseInfo> = None;
    for r in releases {
        if r.prerelease {
            if prerelease.is_none() {
                prerelease = Some(r);
            }
        } else if stable.is_none() {
            stable = Some(r);
        }
        if stable.is_some() && prerelease.is_some() {
            break;
        }
    }
    (stable, prerelease)
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
///
/// The HTTP call races DNS at boot (same failure mode as the migrate
/// task — `nss-lookup.target` helps but the resolver's first query can
/// still time out on a cold router DNS cache). Telemetry is best-effort
/// so we can't afford to burn a lot of time on retries, but failing on
/// every boot until the resolver warms up and then staying failed until
/// the next reboot is needlessly noisy in the logs. Retry up to 3
/// times with 5s/10s backoff, only on transient errors — permanent
/// failures (HTTP 5xx, SSL verify) fail fast so we don't hammer the
/// backend.
pub fn spawn_startup_telemetry() {
    tokio::spawn(async move {
        let current = read_current_version();
        for attempt in 1..=3 {
            if try_send_telemetry(&current, false, "").await {
                return;
            }
            if attempt < 3 {
                let wait = std::time::Duration::from_secs(5 * attempt as u64);
                tokio::time::sleep(wait).await;
            }
        }
    });
}

/// Single-shot telemetry attempt. Returns `true` on success or on a
/// "no point retrying" failure (non-transient HTTP error, missing
/// fingerprint) — i.e. true means "stop trying". Returns `false` only
/// when a retry is likely to help (DNS, connect, timeout).
async fn try_send_telemetry(current: &str, update_available: bool, new_version: &str) -> bool {
    let fp = get_fingerprint();
    if fp.is_empty() {
        return true; // no fingerprint → no reason to retry
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
    let url = "https://api.sentry-six.com/sentryusb/telemetry";
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return true, // client setup broken → won't fix with retry
    };
    match client.post(url).json(&payload).send().await {
        Ok(r) => {
            tracing::info!("[telemetry] sent (status {})", r.status());
            true
        }
        Err(e) => {
            // `is_timeout` / `is_connect` / the DNS-shaped error strings
            // are the ones worth retrying. Anything else (TLS mismatch,
            // bad URL) won't resolve itself on a second attempt.
            let transient = e.is_timeout() || e.is_connect() || {
                let s = e.to_string();
                s.contains("dns error")
                    || s.contains("Could not resolve host")
                    || s.contains("Temporary failure in name resolution")
            };
            if transient {
                tracing::warn!("[telemetry] transient failure, will retry: {}", e);
                false
            } else {
                tracing::warn!("[telemetry] failed: {}", e);
                true
            }
        }
    }
}

/// GET /api/system/update-status
///
/// Returns the cached result of the last `check_for_update` call so the
/// Settings page can render last-known release info without forcing a
/// fresh GitHub round-trip on every page load. Mirrors Go's
/// `getUpdateStatus` (server/api/update.go:594).
///
/// Live install progress is delivered via the `update_status` WebSocket
/// channel (see `run_update`), not this endpoint.
pub async fn get_update_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match std::fs::read_to_string(UPDATE_CHECK_CACHE) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(_) => (
                StatusCode::OK,
                Json(serde_json::json!({"update_available": false})),
            ),
        },
        Err(_) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "update_available": false,
                "checked_at": "",
            })),
        ),
    }
}
