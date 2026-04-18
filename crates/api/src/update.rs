//! OTA update: check for updates, run update, version info.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;

use crate::router::AppState;
use crate::status::get_sbc_model;

static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

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
                .trim_matches('"');
            let current = std::fs::read_to_string("/opt/sentryusb/version")
                .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
                .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());
            let available = !latest.is_empty() && latest != current.trim();
            (StatusCode::OK, Json(serde_json::json!({
                "available": available,
                "latest": latest,
                "current": current.trim(),
            })))
        }
        Err(_) => (StatusCode::OK, Json(serde_json::json!({"available": false, "error": "could not check"}))),
    }
}

/// GET /api/system/update-status
pub async fn get_update_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let running = UPDATE_RUNNING.load(Ordering::Relaxed);
    (StatusCode::OK, Json(serde_json::json!({
        "status": if running { "running" } else { "idle" },
    })))
}
