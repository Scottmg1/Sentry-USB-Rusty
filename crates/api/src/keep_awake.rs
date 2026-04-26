//! Web-UI keep-awake manager.
//!
//! Mirrors `server/api/keepawake.go`: manual/auto modes, busy-queuing,
//! expiration watcher, and re-arm when archiveloop finishes (archiveloop's
//! own `awake_stop` kills our nudge, so we relaunch when we notice busy→idle).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use tokio::sync::{Mutex, Notify};
use tracing::info;

use crate::router::AppState;

const POLL_INTERVAL: Duration = Duration::from_secs(30);
const AUTO_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KaState {
    Idle,
    Pending,
    Active,
}

impl KaState {
    fn as_str(&self) -> &'static str {
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
    expires_at: Option<SystemTime>,
    started_at: Option<SystemTime>,
    pending_duration: Duration,
    /// Generation counter — incremented on every Stop/new-Start so stale
    /// watcher/queue tasks detect they have been superseded and exit.
    epoch: Arc<AtomicU64>,
    stop_notify: Arc<Notify>,
}

pub struct KeepAwakeManager {
    inner: Mutex<KaInner>,
    is_busy: Arc<dyn Fn() -> bool + Send + Sync>,
}

impl KeepAwakeManager {
    pub fn new(is_busy: Arc<dyn Fn() -> bool + Send + Sync>) -> Arc<Self> {
        Arc::new(KeepAwakeManager {
            inner: Mutex::new(KaInner {
                state: KaState::Idle,
                mode: String::new(),
                expires_at: None,
                started_at: None,
                pending_duration: Duration::ZERO,
                epoch: Arc::new(AtomicU64::new(0)),
                stop_notify: Arc::new(Notify::new()),
            }),
            is_busy,
        })
    }

    /// Start or extend a keep-awake session.
    pub async fn start(self: &Arc<Self>, mode: String, duration: Duration) {
        let mut inner = self.inner.lock().await;

        // Already active/pending: just update expiry/duration.
        if inner.state == KaState::Active {
            inner.expires_at = Some(SystemTime::now() + duration);
            inner.mode = mode;
            return;
        }
        if inner.state == KaState::Pending {
            inner.pending_duration = duration;
            inner.mode = mode;
            return;
        }

        // Fresh start.
        let new_epoch = inner.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        inner.mode = mode.clone();
        inner.pending_duration = duration;
        inner.stop_notify = Arc::new(Notify::new());

        if (self.is_busy)() {
            inner.state = KaState::Pending;
            // Record intent immediately so awake_stop's handoff guard knows
            // the webui session will need the nudge as soon as the busy
            // owner releases.
            crate::drives_handler::register_keep_awake_want("webui");
            keep_awake_log(&format!(
                "Queued (mode: {}, duration: {}s) — waiting for archive/processing to finish",
                mode,
                duration.as_secs()
            ));
            info!("[keep-awake] Queued (mode: {}) — system busy", mode);
            let me = self.clone();
            tokio::spawn(async move { me.wait_for_idle_then_start(new_epoch).await });
        } else {
            inner.state = KaState::Active;
            let now = SystemTime::now();
            inner.started_at = Some(now);
            inner.expires_at = Some(now + duration);
            let expires_unix = unix_ts(inner.expires_at.unwrap());
            keep_awake_log(&format!(
                "Started (mode: {}, duration: {}s)",
                mode,
                duration.as_secs()
            ));
            info!("[keep-awake] Started (mode: {}, duration: {}s)", mode, duration.as_secs());
            crate::drives_handler::register_keep_awake_want("webui");
            crate::drives_handler::start_keep_awake_with(
                &reason_label(&mode),
                Some(expires_unix),
            );
            let me = self.clone();
            tokio::spawn(async move { me.expiration_watcher(new_epoch).await });
        }
    }

    /// Auto-mode heartbeat: extend by AUTO_TIMEOUT, start if idle.
    pub async fn heartbeat(self: &Arc<Self>) -> KaState {
        {
            let mut inner = self.inner.lock().await;
            match inner.state {
                KaState::Active => {
                    inner.expires_at = Some(SystemTime::now() + AUTO_TIMEOUT);
                    return KaState::Active;
                }
                KaState::Pending => {
                    inner.pending_duration = AUTO_TIMEOUT;
                    return KaState::Pending;
                }
                KaState::Idle => {}
            }
        }
        self.start("auto".to_string(), AUTO_TIMEOUT).await;
        let inner = self.inner.lock().await;
        inner.state
    }

    /// Immediately stop / cancel keep-awake.
    pub async fn stop(self: &Arc<Self>) {
        let mut inner = self.inner.lock().await;
        let was_active = inner.state == KaState::Active;
        let was_pending = inner.state == KaState::Pending;
        inner.epoch.fetch_add(1, Ordering::SeqCst);
        inner.stop_notify.notify_waiters();
        inner.state = KaState::Idle;
        inner.expires_at = None;
        inner.started_at = None;
        drop(inner);

        if was_active || was_pending {
            crate::drives_handler::release_keep_awake_want("webui");
        }

        if was_active {
            // Don't kill the shared nudge if archive/processor still owns it.
            // Web-UI clears its own state; the active owner issues its own
            // awake_stop on completion.
            if (self.is_busy)() {
                keep_awake_log(
                    "Stopped by user — system still busy (archive/processor); leaving nudge to current owner",
                );
                info!("[keep-awake] Stop deferred — archive/processor still owns the nudge");
            } else {
                keep_awake_log("Stopped by user");
                info!("[keep-awake] Stopped by user");
                crate::drives_handler::stop_keep_awake_bg();
            }
        } else {
            keep_awake_log("Cancelled (was pending)");
            info!("[keep-awake] Cancelled (was pending)");
        }
    }

    pub async fn status(&self) -> serde_json::Value {
        let inner = self.inner.lock().await;
        let mut obj = serde_json::json!({
            "state": inner.state.as_str(),
            "mode": inner.mode,
        });
        if inner.state == KaState::Active {
            if let Some(exp) = inner.expires_at {
                let remaining = exp
                    .duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO);
                obj["expires_at"] = serde_json::Value::String(rfc3339(exp));
                obj["remaining_sec"] = serde_json::Value::Number(remaining.as_secs().into());
            }
        }
        obj
    }

    async fn wait_for_idle_then_start(self: Arc<Self>, epoch: u64) {
        let notify = {
            let inner = self.inner.lock().await;
            inner.stop_notify.clone()
        };
        loop {
            tokio::select! {
                _ = notify.notified() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            // Stale if another start/stop happened.
            {
                let inner = self.inner.lock().await;
                if inner.epoch.load(Ordering::SeqCst) != epoch || inner.state != KaState::Pending {
                    return;
                }
            }
            if (self.is_busy)() {
                continue;
            }

            let (mode, expires_at_unix) = {
                let mut inner = self.inner.lock().await;
                if inner.epoch.load(Ordering::SeqCst) != epoch || inner.state != KaState::Pending {
                    return;
                }
                inner.state = KaState::Active;
                let now = SystemTime::now();
                inner.started_at = Some(now);
                inner.expires_at = Some(now + inner.pending_duration);
                (
                    inner.mode.clone(),
                    unix_ts(inner.expires_at.unwrap()),
                )
            };
            keep_awake_log(&format!(
                "Started (mode: {}) — archive/processing finished",
                mode
            ));
            info!("[keep-awake] Started (mode: {}) — system now idle", mode);
            crate::drives_handler::start_keep_awake_with(
                &reason_label(&mode),
                Some(expires_at_unix),
            );
            let me = self.clone();
            tokio::spawn(async move { me.expiration_watcher(epoch).await });
            return;
        }
    }

    async fn expiration_watcher(self: Arc<Self>, epoch: u64) {
        let notify = {
            let inner = self.inner.lock().await;
            inner.stop_notify.clone()
        };
        let mut was_busy = (self.is_busy)();

        loop {
            tokio::select! {
                _ = notify.notified() => return,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
            let (expired, mode, expires_at) = {
                let inner = self.inner.lock().await;
                if inner.epoch.load(Ordering::SeqCst) != epoch || inner.state != KaState::Active {
                    return;
                }
                let expired = inner.expires_at.map_or(false, |e| SystemTime::now() >= e);
                (expired, inner.mode.clone(), inner.expires_at)
            };

            if expired {
                let mut inner = self.inner.lock().await;
                if inner.epoch.load(Ordering::SeqCst) == epoch && inner.state == KaState::Active {
                    inner.state = KaState::Idle;
                    inner.expires_at = None;
                    inner.started_at = None;
                    inner.epoch.fetch_add(1, Ordering::SeqCst);
                    drop(inner);

                    crate::drives_handler::release_keep_awake_want("webui");

                    // Don't kill the shared nudge mid-archive/processing.
                    if (self.is_busy)() {
                        keep_awake_log(
                            "Expired — system still busy; leaving nudge to current owner",
                        );
                        info!(
                            "[keep-awake] Expired — archive/processor still owns the nudge"
                        );
                    } else {
                        keep_awake_log("Expired, stopping keep-awake");
                        info!("[keep-awake] Expired");
                        crate::drives_handler::stop_keep_awake_bg();
                    }
                }
                return;
            }

            // Re-arm if archive just finished: archiveloop's awake_stop killed
            // our nudge, so relaunch it with the same expiry.
            let now_busy = (self.is_busy)();
            if was_busy && !now_busy {
                keep_awake_log(&format!(
                    "Archive/processing finished — re-launching keep-awake (mode: {})",
                    mode
                ));
                info!(
                    "[keep-awake] Re-arming nudge after archive finished (mode: {})",
                    mode
                );
                crate::drives_handler::start_keep_awake_with(
                    &reason_label(&mode),
                    expires_at.map(unix_ts),
                );
            }
            was_busy = now_busy;
        }
    }
}

fn reason_label(mode: &str) -> String {
    match mode {
        "manual" => "Manual",
        "auto" => "Auto Keep Awake",
        _ => "Keep Awake",
    }
    .to_string()
}

fn unix_ts(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn rfc3339(t: SystemTime) -> String {
    let dt: chrono::DateTime<chrono::Utc> = t.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn keep_awake_log(msg: &str) {
    use std::io::Write;
    const LOG_PATH: &str = "/mutable/archiveloop.log";
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(LOG_PATH)
    {
        let ts = chrono::Local::now().format("%a %e %b %H:%M:%S %Z %Y");
        let _ = writeln!(f, "{}: [keep-awake-webui] {}", ts, msg);
    }
}

// --- HTTP handlers ---

#[derive(Deserialize)]
struct StartRequest {
    mode: Option<String>,
    duration_min: Option<u64>,
}

/// POST /api/keep-awake/start
pub async fn start(
    State(s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let req: StartRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return crate::json_error(StatusCode::BAD_REQUEST, "invalid request body"),
    };
    let mode = req.mode.unwrap_or_else(|| "manual".to_string());
    let duration = Duration::from_secs(req.duration_min.unwrap_or(10) * 60).max(Duration::from_secs(1));
    s.keep_awake.start(mode, duration).await;
    (StatusCode::OK, Json(s.keep_awake.status().await))
}

/// POST /api/keep-awake/heartbeat
pub async fn heartbeat(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let state = s.keep_awake.heartbeat().await;
    (
        StatusCode::OK,
        Json(serde_json::json!({ "state": state.as_str() })),
    )
}

/// POST /api/keep-awake/stop
pub async fn stop(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    s.keep_awake.stop().await;
    crate::json_ok()
}

/// GET /api/keep-awake/status
pub async fn status(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::OK, Json(s.keep_awake.status().await))
}
