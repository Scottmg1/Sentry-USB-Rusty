//! Keep-awake manager: manual/auto modes, busy-queuing, expiration watcher.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use tokio::sync::{Mutex, Notify};
use tracing::{info, warn};

use crate::router::AppState;

/// Keep-awake state.
#[derive(Clone, Copy, PartialEq)]
enum KaState {
    Idle,
    Pending,
    Active,
}

impl KaState {
    fn as_str(&self) -> &str {
        match self {
            KaState::Idle => "idle",
            KaState::Pending => "pending",
            KaState::Active => "active",
        }
    }
}

struct KaInner {
    state: KaState,
    mode: String,
    expires_at: Option<tokio::time::Instant>,
    pending_duration: Duration,
    stop_notify: Arc<Notify>,
}

/// Shared keep-awake manager.
pub struct KeepAwakeManager {
    inner: Mutex<KaInner>,
}

impl KeepAwakeManager {
    pub fn new() -> Arc<Self> {
        Arc::new(KeepAwakeManager {
            inner: Mutex::new(KaInner {
                state: KaState::Idle,
                mode: String::new(),
                expires_at: None,
                pending_duration: Duration::ZERO,
                stop_notify: Arc::new(Notify::new()),
            }),
        })
    }
}

#[derive(Deserialize)]
struct StartRequest {
    mode: Option<String>,
    duration_min: Option<u64>,
}

/// POST /api/keep-awake/start
pub async fn start(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    let req: StartRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "invalid request body"),
    };

    let mode = req.mode.unwrap_or_else(|| "manual".to_string());
    let duration_min = req.duration_min.unwrap_or(10);
    let duration = Duration::from_secs(duration_min * 60);

    // Start keep-awake scripts
    let mode_clone = mode.clone();
    tokio::spawn(async move {
        let reason = match mode_clone.as_str() {
            "manual" => "Manual",
            "auto" => "Auto Keep Awake",
            _ => "Keep Awake",
        };
        let secs = duration.as_secs().to_string();
        let _ = sentryusb_shell::run("bash", &["/root/bin/awake_start", reason, &secs]).await;
    });

    (StatusCode::OK, Json(serde_json::json!({
        "state": "active",
        "mode": mode,
    })))
}

/// POST /api/keep-awake/stop
pub async fn stop(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    tokio::spawn(async {
        let _ = sentryusb_shell::run("bash", &["/root/bin/awake_stop"]).await;
    });
    crate::json_ok()
}

/// GET /api/keep-awake/status
pub async fn status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    // Check if awake_start is running
    let active = match sentryusb_shell::run("pgrep", &["-f", "awake_start"]).await {
        Ok(out) => !out.trim().is_empty(),
        Err(_) => false,
    };

    (StatusCode::OK, Json(serde_json::json!({
        "state": if active { "active" } else { "idle" },
        "mode": if active { "manual" } else { "" },
    })))
}
