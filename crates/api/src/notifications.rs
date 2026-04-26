//! Push notification pairing + mobile app proxy — port of Go
//! `server/api/notifications.go`.
//!
//! Owns the Pi's long-lived `(device_id, device_secret)` credentials used
//! to authenticate against the Sentry Connect backend. Credentials live
//! at `/root/.sentryusb/notification-credentials.json` and are read back
//! by `envsetup.sh` so the bash `send-push-message` wrapper can forward
//! them to the Rust API's `/api/notifications/send` via `MOBILE_PUSH_*`
//! env vars.
//!
//! Pairing-code flow:
//!   1. iOS app hits `POST /api/notifications/generate-code`.
//!   2. Server mints a 6-char alphanumeric code (no ambiguous chars),
//!      registers it with the notification backend, and returns it plus
//!      an expiry timestamp.
//!   3. User types the code into the iOS app; the app hits the backend
//!      directly to finalize pairing.
//!
//! Paired-device management endpoints are thin proxies — the Pi's only
//! role is to authenticate with its device_secret; the backend owns the
//! per-device state.

use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::router::AppState;

const CREDENTIALS_PATH: &str = "/root/.sentryusb/notification-credentials.json";

/// Alphanumeric charset excluding ambiguous glyphs (0/O, 1/I/l).
/// Must match Go `pairingCodeCharset` exactly so codes generated on a
/// mixed Rust/Go fleet are cross-verifiable.
const PAIRING_CHARSET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const PAIRING_CODE_LEN: usize = 6;
const PAIRING_EXPIRY: Duration = Duration::from_secs(5 * 60);
const MAX_ACTIVE_CODES: usize = 3;

/// Default notification backend. Override with `SENTRY_NOTIFICATION_URL`
/// (env var or `sentryusb.conf` entry) — matches Sentry-USB PR #31 / 771bca6.
const DEFAULT_NOTIFICATION_BASE_URL: &str = "https://notifications.sentry-six.com";

fn notification_base_url() -> String {
    // 1. Env var first — covers dev overrides + any future systemd
    //    EnvironmentFile= setup.
    if let Ok(v) = std::env::var("SENTRY_NOTIFICATION_URL") {
        let trimmed = v.trim().trim_end_matches('/');
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    // 2. Parse `sentryusb.conf` directly. systemd starts the binary
    //    without sourcing the config (no shell wrapper), so the env
    //    var won't be set on a normal install — without this fallback,
    //    the user's SENTRY_NOTIFICATION_URL is silently ignored and
    //    every pairing/test/list call hits notifications.sentry-six.com
    //    regardless of what the conf says. Mirrors Go's configOrDefault
    //    (server/api/apiconfig.go).
    let config_path = sentryusb_config::find_config_path();
    if let Ok((active, _)) = sentryusb_config::parse_file(config_path) {
        if let Some(v) = active.get("SENTRY_NOTIFICATION_URL") {
            let trimmed = v.trim().trim_end_matches('/');
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    // 3. Hardcoded default.
    DEFAULT_NOTIFICATION_BASE_URL.to_string()
}

// -----------------------------------------------------------------------------
// Device credentials (long-lived)
// -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NotificationCredentials {
    device_id: String,
    device_secret: String,
}

static CACHED_CREDS: OnceLock<NotificationCredentials> = OnceLock::new();

/// Load or generate the Pi's notification credentials. Cached after the
/// first call for the lifetime of the process — credentials do not
/// change without explicit re-pairing, so refreshing mid-process would
/// complicate nothing and cost us durability.
fn get_or_create_credentials() -> Option<&'static NotificationCredentials> {
    if let Some(existing) = CACHED_CREDS.get() {
        return Some(existing);
    }

    // Try existing file first.
    if let Ok(data) = std::fs::read_to_string(CREDENTIALS_PATH) {
        if let Ok(c) = serde_json::from_str::<NotificationCredentials>(&data) {
            if !c.device_id.is_empty() && !c.device_secret.is_empty() {
                let _ = CACHED_CREDS.set(c);
                return CACHED_CREDS.get();
            }
        }
    }

    // Generate new credentials. 32 bytes → 64 hex chars for device_id;
    // 64 bytes → 128 hex chars for device_secret. Matches Go's
    // `generateSecureToken(32)` / `generateSecureToken(64)`.
    let device_id = random_hex(32);
    let device_secret = random_hex(64);
    let new = NotificationCredentials { device_id, device_secret };

    // Remount rootfs rw so the write lands on real disk, not an overlay
    // that gets wiped on reboot. Best-effort — `remountfs_rw` is a
    // runtime helper installed by setup.
    let _ = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status();

    if let Some(dir) = Path::new(CREDENTIALS_PATH).parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            warn!("[notifications] failed to mkdir {}: {}", dir.display(), e);
        }
        // 0700 on the parent so device_secret isn't world-readable.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }

    match serde_json::to_vec_pretty(&new) {
        Ok(data) => {
            if let Err(e) = write_private(CREDENTIALS_PATH, &data) {
                warn!("[notifications] failed to save credentials: {}", e);
                return None;
            }
            let short = new.device_id.chars().take(8).collect::<String>();
            info!("[notifications] Generated new device credentials: {}", short);
            let _ = CACHED_CREDS.set(new);
            CACHED_CREDS.get()
        }
        Err(e) => {
            warn!("[notifications] failed to serialize new credentials: {}", e);
            None
        }
    }
}

/// Write file at `path` with 0600 permissions via tmp + rename.
fn write_private(path: &str, data: &[u8]) -> std::io::Result<()> {
    let tmp = format!("{}.tmp", path);
    std::fs::write(&tmp, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn random_hex(byte_len: usize) -> String {
    let mut buf = vec![0u8; byte_len];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

/// Auto-enable `MOBILE_PUSH_ENABLED=true` in the config when the user
/// first pairs a device. Runs in a background task so it doesn't block
/// the pairing response — the code is already registered with the
/// backend by that point, and this only affects the next notification
/// dispatch.
async fn auto_enable_mobile_push_in_config() {
    // Best-effort: parse the active-only map, flip the flag, write back.
    // sentryusb_config::parse_file returns (active, commented); we only
    // write the active set.
    tokio::task::spawn_blocking(|| {
        let config_path = sentryusb_config::find_config_path();
        let (mut active, _) = match sentryusb_config::parse_file(config_path) {
            Ok(v) => v,
            Err(_) => return,
        };
        if active.get("MOBILE_PUSH_ENABLED").map(|v| v.as_str()) == Some("true") {
            return;
        }
        active.insert("MOBILE_PUSH_ENABLED".to_string(), "true".to_string());
        let _ = std::process::Command::new("bash")
            .args(["-c", "/root/bin/remountfs_rw"])
            .status();
        match sentryusb_config::write_file(config_path, &active) {
            Ok(()) => info!("[notifications] Auto-enabled MOBILE_PUSH_ENABLED in config"),
            Err(e) => warn!("[notifications] Failed to enable MOBILE_PUSH_ENABLED in config: {}", e),
        }
    })
    .await
    .ok();
}

// -----------------------------------------------------------------------------
// Pairing codes (short-lived)
// -----------------------------------------------------------------------------

#[derive(Clone)]
struct PairingCode {
    code: String,
    expires_at: SystemTime,
}

static ACTIVE_CODES: Mutex<Vec<PairingCode>> = Mutex::new(Vec::new());

fn generate_pairing_code_string() -> String {
    let mut rng = rand::rng();
    let mut out = String::with_capacity(PAIRING_CODE_LEN);
    for _ in 0..PAIRING_CODE_LEN {
        let idx = (rng.next_u32() as usize) % PAIRING_CHARSET.len();
        out.push(PAIRING_CHARSET[idx] as char);
    }
    out
}

fn clean_expired_codes(codes: &mut Vec<PairingCode>) {
    let now = SystemTime::now();
    codes.retain(|c| c.expires_at > now);
}

fn to_rfc3339(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// POST /api/notifications/generate-code
pub async fn generate_pairing_code(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let creds = match get_or_create_credentials() {
        Some(c) => c,
        None => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to initialize notification credentials",
            );
        }
    };

    // Mint the code under the lock, register with backend, then commit
    // or roll back. Matches Go's ordering: the user only sees a code if
    // the backend has acknowledged it.
    let (code, expires_at) = {
        let mut codes = ACTIVE_CODES.lock().unwrap_or_else(|p| p.into_inner());
        clean_expired_codes(&mut codes);
        if codes.len() >= MAX_ACTIVE_CODES {
            return crate::json_error(
                StatusCode::TOO_MANY_REQUESTS,
                "Too many active pairing codes. Wait for existing codes to expire.",
            );
        }
        let code = generate_pairing_code_string();
        let expires_at = SystemTime::now() + PAIRING_EXPIRY;
        codes.push(PairingCode { code: code.clone(), expires_at });
        (code, expires_at)
    };

    match register_code_with_backend(creds, &code).await {
        Ok(()) => {
            info!(
                "[notifications] Generated pairing code {} (expires {})",
                code,
                to_rfc3339(expires_at)
            );
            tokio::spawn(auto_enable_mobile_push_in_config());
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "code": code,
                    "expires_at": to_rfc3339(expires_at),
                })),
            )
        }
        Err(e) => {
            // Roll back the pending code — we never gave it to the user,
            // and leaving it would eat one of the three active slots
            // until expiry.
            let mut codes = ACTIVE_CODES.lock().unwrap_or_else(|p| p.into_inner());
            codes.retain(|c| c.code != code);
            warn!("[notifications] Failed to register code {} with backend: {}", code, e);
            crate::json_error(
                StatusCode::BAD_GATEWAY,
                "Failed to register pairing code with notification server. Check internet connection.",
            )
        }
    }
}

async fn register_code_with_backend(
    creds: &NotificationCredentials,
    code: &str,
) -> Result<(), String> {
    let hostname = std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let fp = crate::update::get_fingerprint();

    let body = serde_json::json!({
        "device_id": creds.device_id,
        "device_secret": creds.device_secret,
        "code": code,
        "hostname": hostname,
        "fingerprint": fp,
    });

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| format!("failed to build HTTP client: {}", e))?;

    let url = format!("{}/register-code", notification_base_url());
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("failed to reach notification server: {}", e))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("backend returned {}: {}", status, body));
    }

    info!("[notifications] Code {} registered with backend successfully", code);
    Ok(())
}

// -----------------------------------------------------------------------------
// Paired-device proxy endpoints
// -----------------------------------------------------------------------------

/// GET /api/notifications/paired-devices
///
/// Proxies `GET {base}/devices?device_id=X` with `X-Device-Secret`. The
/// backend is authoritative — we don't keep a local device list, because
/// a device can unpair from the iOS app without touching the Pi.
pub async fn list_paired_devices(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let creds = match get_or_create_credentials() {
        Some(c) => c,
        None => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Notification credentials not available",
            );
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to build HTTP client: {}", e),
            );
        }
    };

    let url = format!(
        "{}/devices?device_id={}",
        notification_base_url(),
        creds.device_id
    );
    let resp = match client
        .get(&url)
        .header("X-Device-Secret", &creds.device_secret)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return crate::json_error(
                StatusCode::BAD_GATEWAY,
                "Failed to reach notification backend",
            );
        }
    };

    proxy_response(resp).await
}

/// DELETE /api/notifications/paired-devices/{id}
pub async fn remove_paired_device(
    State(_s): State<AppState>,
    AxumPath(id): AxumPath<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if id.is_empty() {
        return crate::json_error(StatusCode::BAD_REQUEST, "Missing pairing ID");
    }

    let creds = match get_or_create_credentials() {
        Some(c) => c,
        None => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Notification credentials not available",
            );
        }
    };

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("failed to build HTTP client: {}", e),
            );
        }
    };

    let url = format!(
        "{}/devices/{}?device_id={}",
        notification_base_url(),
        id,
        creds.device_id
    );
    let resp = match client
        .delete(&url)
        .header("X-Device-Secret", &creds.device_secret)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return crate::json_error(
                StatusCode::BAD_GATEWAY,
                "Failed to reach notification backend",
            );
        }
    };

    info!("[notifications] Removed paired device: {}", id);
    proxy_response(resp).await
}

/// Forward the backend's status + JSON body. If the backend returned
/// non-JSON (shouldn't happen but let's not 500 on our side), wrap it.
async fn proxy_response(resp: reqwest::Response) -> (StatusCode, Json<serde_json::Value>) {
    let status = resp.status();
    let bytes = resp.bytes().await.unwrap_or_default();
    let status_code = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
        serde_json::json!({ "raw": String::from_utf8_lossy(&bytes).into_owned() })
    });
    (status_code, Json(body))
}

// -----------------------------------------------------------------------------
// Test notification
// -----------------------------------------------------------------------------

/// POST /api/notifications/test
///
/// Sends a test notification to the mobile push backend only.
/// Mirrors Go's sendTestNotification which exclusively targets the
/// Sentry Connect relay — other providers are not exercised here.
pub async fn send_test_notification(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let creds = match get_or_create_credentials() {
        Some(c) => c,
        None => return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "Notification credentials not available"),
    };

    let hostname = std::fs::read_to_string("/etc/hostname")
        .unwrap_or_else(|_| "SentryUSB".to_string());
    let hostname = hostname.trim();

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let result = sentryusb_notify::sentry_connect::send(
        &client,
        &creds.device_id,
        &creds.device_secret,
        "SentryUSB Test",
        &format!("Test notification from {} — push notifications are working!", hostname),
    ).await;

    match result {
        Ok(()) => {
            info!("[notifications] Test notification sent successfully");
            (StatusCode::OK, Json(serde_json::json!({"status": "ok"})))
        }
        Err(e) => {
            warn!("[notifications] Test notification failed: {}", e);
            crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string())
        }
    }
}

// -----------------------------------------------------------------------------
// Runtime send endpoint (called by `/root/bin/send-push-message` wrapper)
// -----------------------------------------------------------------------------

/// Body for `POST /api/notifications/send`.
///
/// Mirrors the positional args of the bash `send-push-message`:
///   * `title`, `message` — required.
///   * `type_hint` (`start` / `finish`) — for the live_activity branch
///     on mobile push.
///   * `notification_type` (`archive_start`, `temperature`, …) — used
///     for the gate check and echoed in mobile push + history.
///   * `archive_total_count` — live_activity payload on archive_start.
#[derive(Deserialize)]
pub struct SendNotificationRequest {
    pub title: String,
    pub message: String,
    #[serde(default, rename = "type")]
    pub type_hint: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub archive_total_count: Option<u32>,
}

/// POST /api/notifications/send
///
/// Single entry point used by the runtime scripts (archiveloop,
/// temperature_monitor, post-archive-process.sh, …) via the
/// `/root/bin/send-push-message` curl wrapper. Behaviourally mirrors
/// the bash script it replaces:
///   1. Gate-check the notification_type against user settings. If
///      disabled, return `{"skipped": true, "reason": "type_disabled"}`
///      without touching any provider (no history event written).
///   2. Dispatch to every configured notifier via the Rust notify crate.
///   3. Record a history event with the per-provider results.
pub async fn send_notification(
    State(_s): State<AppState>,
    Json(body): Json<SendNotificationRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Gate check.
    let notification_type = body.notification_type.as_deref();
    if !crate::notification_center::is_type_enabled(notification_type) {
        return (
            StatusCode::OK,
            Json(serde_json::json!({
                "skipped": true,
                "reason": "type_disabled",
                "type": notification_type.unwrap_or(""),
            })),
        );
    }

    // Dispatch.
    let config = sentryusb_notify::NotifyConfig::from_config();
    let req = sentryusb_notify::NotifyRequest {
        title: &body.title,
        message: &body.message,
        type_hint: body.type_hint.as_deref(),
        notification_type,
        archive_total_count: body.archive_total_count,
    };
    let results = sentryusb_notify::send_to_all_with_context(&config, &req).await;

    // Build the per-provider pass/fail maps the history event shape
    // expects.
    let mut providers: Vec<String> = Vec::with_capacity(results.len());
    let mut result_map: std::collections::HashMap<String, String> =
        std::collections::HashMap::with_capacity(results.len());
    let mut failures: Vec<String> = Vec::new();
    for (name, res) in &results {
        providers.push(name.clone());
        match res {
            Ok(()) => {
                result_map.insert(name.clone(), "ok".to_string());
            }
            Err(e) => {
                result_map.insert(name.clone(), "error".to_string());
                failures.push(format!("{}: {}", name, e));
            }
        }
    }
    let attempted = results.len();
    let providers_for_response = providers.clone();

    // Record history. Type falls back to "general" when unset — matches
    // bash's `${notification_type:-general}`.
    let event = crate::notification_center::NotificationEvent {
        id: String::new(),
        timestamp: 0,
        event_type: notification_type.unwrap_or("general").to_string(),
        title: body.title.clone(),
        message: body.message.clone(),
        providers,
        results: result_map,
    };
    if let Err(e) = crate::notification_center::record_event(event) {
        tracing::warn!("[notifications] Failed to record history event: {}", e);
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "attempted": attempted,
            "providers": providers_for_response,
            "failed": failures,
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_codes_use_allowed_charset_only() {
        let code = generate_pairing_code_string();
        assert_eq!(code.len(), PAIRING_CODE_LEN);
        for c in code.bytes() {
            assert!(
                PAIRING_CHARSET.contains(&c),
                "unexpected char {:?} in pairing code {:?}",
                c as char,
                code
            );
        }
    }

    #[test]
    fn pairing_code_excludes_ambiguous_chars() {
        // Charset must NOT contain 0, O, 1, I.
        for banned in b"0OI" {
            assert!(
                !PAIRING_CHARSET.contains(banned),
                "pairing charset must exclude {:?}",
                *banned as char
            );
        }
    }

    #[test]
    fn random_hex_has_twice_the_byte_length() {
        for n in [0, 1, 16, 32, 64] {
            assert_eq!(random_hex(n).len(), n * 2);
        }
    }

    #[test]
    fn expired_codes_are_dropped() {
        let mut codes = vec![
            PairingCode {
                code: "OLD".into(),
                expires_at: SystemTime::now() - Duration::from_secs(1),
            },
            PairingCode {
                code: "NEW".into(),
                expires_at: SystemTime::now() + Duration::from_secs(300),
            },
        ];
        clean_expired_codes(&mut codes);
        assert_eq!(codes.len(), 1);
        assert_eq!(codes[0].code, "NEW");
    }
}
