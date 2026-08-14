//! Setup wizard configuration API.
//!
//! STARTED and FINISHED markers let setup resume completed phases after a
//! required reboot. A final successful phase replaces STARTED with FINISHED.

use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use tracing::info;

use crate::router::AppState;

static SETUP_RUNNING: AtomicBool = AtomicBool::new(false);

const SETUP_FINISHED_PATHS: &[&str] = &[
    "/sentryusb/SENTRYUSB_SETUP_FINISHED",
    "/boot/firmware/SENTRYUSB_SETUP_FINISHED",
    "/boot/SENTRYUSB_SETUP_FINISHED",
];

const SETUP_STARTED_PATHS: &[&str] = &[
    "/sentryusb/SENTRYUSB_SETUP_STARTED",
    "/boot/firmware/SENTRYUSB_SETUP_STARTED",
    "/boot/SENTRYUSB_SETUP_STARTED",
];

fn is_setup_finished() -> bool {
    SETUP_FINISHED_PATHS.iter().any(|p| std::path::Path::new(p).exists())
}

fn is_setup_started() -> bool {
    SETUP_STARTED_PATHS.iter().any(|p| std::path::Path::new(p).exists())
}

/// Persistent failure state for page reloads and auto-resume decisions.
const SETUP_ERROR_MARKER: &str = "/sentryusb/SENTRYUSB_SETUP_ERROR";

#[derive(Debug, Clone, PartialEq)]
struct SetupFailure {
    /// "config" = user must fix settings (don't auto-retry);
    /// "transient" = network/apt/disk hiccup (safe to auto-retry on boot).
    kind: &'static str,
    message: String,
}

impl SetupFailure {
    /// Classify an `anyhow` error from the setup runner. Config-validation
    /// failures surface as a downcastable `sentryusb_setup::ConfigError`.
    fn from_error(err: &anyhow::Error) -> Self {
        let kind = if err.downcast_ref::<sentryusb_setup::ConfigError>().is_some() {
            "config"
        } else {
            "transient"
        };
        SetupFailure { kind, message: format!("{err:#}") }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({ "kind": self.kind, "message": self.message })
    }

    fn parse(s: &str) -> Option<SetupFailure> {
        let v: serde_json::Value = serde_json::from_str(s).ok()?;
        // Unknown kinds remain retryable.
        let kind = if v.get("kind").and_then(|k| k.as_str()) == Some("config") {
            "config"
        } else {
            "transient"
        };
        let message = v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string();
        Some(SetupFailure { kind, message })
    }
}

/// Resume interrupted or transient failures, but wait for user correction of
/// configuration failures.
fn should_auto_resume(started: bool, finished: bool, failure: Option<&SetupFailure>) -> bool {
    started && !finished && failure.map_or(true, |f| f.kind != "config")
}

fn read_setup_failure() -> Option<SetupFailure> {
    SetupFailure::parse(&std::fs::read_to_string(SETUP_ERROR_MARKER).ok()?)
}

fn write_setup_failure(f: &SetupFailure) {
    let _ = std::process::Command::new("bash")
        .args(["-c", "/root/bin/remountfs_rw"])
        .status();
    let _ = std::fs::write(SETUP_ERROR_MARKER, f.to_json().to_string());
}

fn clear_setup_failure() {
    let _ = std::fs::remove_file(SETUP_ERROR_MARKER);
}

/// Call at server startup to resume an interrupted setup after reboot.
pub fn auto_resume_setup(hub: sentryusb_ws::Hub) {
    let failure = read_setup_failure();
    if should_auto_resume(is_setup_started(), is_setup_finished(), failure.as_ref()) {
        info!("[setup] Detected interrupted setup (STARTED marker present, no FINISHED). Auto-resuming...");
        spawn_setup(hub);
    } else if let Some(f) = failure.filter(|f| f.kind == "config") {
        // Leave configuration failures visible for an explicit retry.
        info!(
            "[setup] Last setup failed on a configuration error ({}). Not auto-resuming; awaiting user fix + retry.",
            f.message
        );
    }
}

/// GET /api/setup/status
pub async fn get_setup_status() -> (StatusCode, Json<serde_json::Value>) {
    let running = SETUP_RUNNING.load(Ordering::Relaxed);
    let finished = is_setup_finished();

    // An active retry supersedes the prior failure marker.
    let failure = if running { None } else { read_setup_failure() };

    // A recorded failure makes STARTED terminal until the next retry.
    let effective_running = running || (!finished && is_setup_started() && failure.is_none());

    let mut body = serde_json::json!({
        "setup_finished": finished,
        "setup_running": effective_running,
    });
    if let Some(f) = failure {
        body["error"] = f.to_json();
    }

    (StatusCode::OK, Json(body))
}

/// GET /api/setup/config
pub async fn get_setup_config(State(_s): State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::parse_file(config_path) {
        Ok((active, commented)) => {
            let mut merged = serde_json::Map::new();
            for (k, v) in &commented {
                merged.insert(k.clone(), serde_json::json!({
                    "value": v,
                    "active": false,
                }));
            }
            for (k, v) in &active {
                merged.insert(k.clone(), serde_json::json!({
                    "value": v,
                    "active": true,
                }));
            }
            // Configuration changes only through explicit PUT requests.
            (
                StatusCode::OK,
                [(axum::http::header::CACHE_CONTROL, "private, max-age=30")],
                Json(serde_json::Value::Object(merged)),
            )
                .into_response()
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to read config: {}", e)).into_response(),
    }
}

/// PUT /api/setup/config
pub async fn save_setup_config(
    State(_s): State<AppState>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    // Archive reachability probes consume ARCHIVE_SERVER; derive it for rsync
    // without overwriting an explicit value.
    let body = mirror_archive_server(body);

    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::write_file(config_path, &body) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to write config: {}", e)),
    }
}

/// Derive an rsync reachability host when ARCHIVE_SERVER is empty.
/// Rclone remotes are not hostnames and must not be mirrored.
/// Idempotent: a non-empty incoming ARCHIVE_SERVER wins.
fn mirror_archive_server(
    mut body: std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let system = body
        .get("ARCHIVE_SYSTEM")
        .map(|s| s.as_str())
        .unwrap_or("");
    let already_set = body
        .get("ARCHIVE_SERVER")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if already_set {
        return body;
    }
    if system != "rsync" {
        return body;
    }
    if let Some(v) = body.get("RSYNC_SERVER").cloned() {
        if !v.trim().is_empty() {
            body.insert("ARCHIVE_SERVER".to_string(), v);
        }
    }
    body
}

/// Shared logic: spawn the setup task in the background.
fn spawn_setup(hub: sentryusb_ws::Hub) {
    if SETUP_RUNNING.swap(true, Ordering::SeqCst) {
        info!("[setup] Setup already running, skipping duplicate spawn");
        return;
    }

    tokio::spawn(async move {
        // A new attempt supersedes the prior failure.
        clear_setup_failure();
        hub.broadcast("setup_status", &serde_json::json!({"status": "running"}));
        info!("[setup] Starting native Rust setup");

        let hub_progress = hub.clone();
        let hub_phase = hub.clone();
        let emitter = sentryusb_setup::runner::make_emitter(
            move |msg: &str| {
                hub_progress.broadcast("setup_progress", &serde_json::json!({"message": msg}));
            },
            move |id: &str, label: &str| {
                hub_phase.broadcast("setup_phase", &serde_json::json!({"id": id, "label": label}));
            },
        );

        let result = sentryusb_setup::runner::run_full_setup(emitter).await;

        SETUP_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(()) => {
                hub.broadcast("setup", &serde_json::json!({"status": "complete"}));
            }
            Err(e) => {
                tracing::error!("[setup] Failed: {:#}", e);
                // Persist failure class for reload and auto-resume behavior.
                let failure = SetupFailure::from_error(&e);
                write_setup_failure(&failure);
                // Surface the terminal error in the wizard log as well as journalctl.
                let line = format!("ERROR: setup failed: {:#}", e);
                let stamped = format!(
                    "{} : {}",
                    tokio::process::Command::new("date")
                        .output()
                        .await
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_else(|_| "???".to_string()),
                    line,
                );
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("/sentryusb/sentryusb-setup.log")
                {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", stamped);
                }
                hub.broadcast("setup_progress", &serde_json::json!({"message": stamped}));
                hub.broadcast(
                    "setup",
                    &serde_json::json!({
                        "status": "error",
                        "error": e.to_string(),
                        "kind": failure.kind,
                    }),
                );
            }
        }
    });
}

const SETUP_PHASES_FILE: &str = "/sentryusb/setup-phases.jsonl";

/// GET /api/setup/phases — returns the list of phases that have already been
/// announced during the current (possibly multi-reboot) setup run. The web UI
/// fetches this on mount and on WebSocket reconnect so it can reconstruct the
/// phase list that was built up before the tab connected.
pub async fn get_setup_phases(
    State(_s): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let phases = dedup_phases(&std::fs::read_to_string(SETUP_PHASES_FILE).unwrap_or_default());
    (StatusCode::OK, Json(serde_json::json!({ "phases": phases })))
}

/// Deduplicate the append-only, cross-reboot phase ledger by phase ID.
fn dedup_phases(ledger: &str) -> Vec<serde_json::Value> {
    let mut seen = std::collections::HashSet::new();
    ledger
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|p| {
            match p.get("id").and_then(|v| v.as_str()) {
                Some(id) => seen.insert(id.to_string()),
                None => true,
            }
        })
        .collect()
}

/// POST /api/setup/run
pub async fn run_setup(State(s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if SETUP_RUNNING.load(Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Setup is already running");
    }

    spawn_setup(s.hub.clone());

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

/// POST /api/setup/test-archive
///
/// Body: JSON map with keys matching sentryusb.conf entries:
/// `ARCHIVE_SYSTEM` (cifs|rsync|rclone|nfs), plus protocol-specific fields.
/// Performs a mount or connection probe rather than only a ping.
pub async fn test_archive(
    State(s): State<AppState>,
    Json(params): Json<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let system = params
        .get("ARCHIVE_SYSTEM")
        .map(|s| s.as_str())
        .unwrap_or("");
    if system.is_empty() || system == "none" {
        return crate::json_error(StatusCode::BAD_REQUEST, "No archive system specified");
    }

    let timeout = std::time::Duration::from_secs(15);
    let tmp_dir = "/tmp/sentryusb-archive-test";

    // NFS and CIFS probes require their userspace mount helpers. Install them
    // idempotently and broadcast progress during the same request.
    async fn ensure_mount_helper(
        hub: &sentryusb_ws::Hub,
        pkg: &str,
        helper_path: &str,
    ) -> Result<(), String> {
        if std::path::Path::new(helper_path).exists() {
            return Ok(());
        }
        hub.broadcast(
            "archive_test_status",
            &serde_json::json!({ "stage": "installing", "package": pkg }),
        );
        // Wait for concurrent setup package work; bound the shell beyond apt's lock wait.
        sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(240),
            "apt-get",
            &[
                "-o", "DPkg::Lock::Timeout=180",
                "install", "-y", "--no-install-recommends", pkg,
            ],
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("{} helper missing and apt-get install failed: {}", pkg, e))
    }

    let test_result: Result<(), String> = match system {
        "cifs" => {
            let server = params.get("ARCHIVE_SERVER").cloned().unwrap_or_default();
            let share = params.get("SHARE_NAME").cloned().unwrap_or_default();
            let user = params.get("SHARE_USER").cloned().unwrap_or_default();
            let pass = params.get("SHARE_PASSWORD").cloned().unwrap_or_default();
            let domain = params.get("SHARE_DOMAIN").cloned().unwrap_or_default();
            let cifs_ver = params.get("CIFS_VERSION").cloned().unwrap_or_default();
            if server.is_empty() || share.is_empty() || user.is_empty() || pass.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required CIFS fields");
            }
            if let Err(e) = ensure_mount_helper(&s.hub, "cifs-utils", "/sbin/mount.cifs").await {
                return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e);
            }
            s.hub.broadcast(
                "archive_test_status",
                &serde_json::json!({ "stage": "testing" }),
            );
            let _ = std::fs::create_dir_all(tmp_dir);
            let mut opts = format!("username={},password={},iocharset=utf8", user, pass);
            if !domain.is_empty() {
                opts.push_str(&format!(",domain={}", domain));
            }
            if !cifs_ver.is_empty() {
                opts.push_str(&format!(",vers={}", cifs_ver));
            }
            let src = format!("//{}/{}", server, share);
            let res = sentryusb_shell::run_with_timeout(
                timeout, "mount", &["-t", "cifs", &src, tmp_dir, "-o", &opts],
            ).await;
            if res.is_ok() {
                let _ = sentryusb_shell::run_with_timeout(
                    std::time::Duration::from_secs(5), "umount", &[tmp_dir],
                ).await;
            }
            let _ = std::fs::remove_dir(tmp_dir);
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        "rsync" => {
            let server = params.get("RSYNC_SERVER").cloned().unwrap_or_default();
            let user = params.get("RSYNC_USER").cloned().unwrap_or_default();
            let path = params.get("RSYNC_PATH").cloned().unwrap_or_default();
            if server.is_empty() || user.is_empty() || path.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required rsync fields");
            }
            // Probe the configured port, not an implicit port 22.
            let port = match sentryusb_config::validate_rsync_ssh_port(&params) {
                Ok(p) => sentryusb_shell::SshPort::new(p),
                Err(e) => return crate::json_error(StatusCode::BAD_REQUEST, &e),
            };
            let target = format!("{}@{}", user, server);
            let mut args: Vec<&str> = vec![
                "-o", "ConnectTimeout=10",
                "-o", "StrictHostKeyChecking=no",
                "-o", "BatchMode=yes",
            ];
            args.extend(port.ssh_args());
            args.extend_from_slice(&[&target, "echo", "ok"]);
            let res = sentryusb_shell::run_with_timeout(timeout, "ssh", &args).await;
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        "rclone" => {
            let drive = params.get("RCLONE_DRIVE").cloned().unwrap_or_default();
            let rpath = params.get("RCLONE_PATH").cloned().unwrap_or_default();
            if drive.is_empty() || rpath.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required rclone fields");
            }
            let target = format!("{}:{}", drive, rpath);
            // Probe the same explicit rclone config used by archive jobs.
            let res = sentryusb_shell::run_with_timeout(
                timeout,
                "rclone",
                &["--config", "/root/.config/rclone/rclone.conf", "lsd", &target],
            ).await;
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        "nfs" => {
            let server = params.get("ARCHIVE_SERVER").cloned().unwrap_or_default();
            let export = params.get("SHARE_NAME").cloned().unwrap_or_default();
            if server.is_empty() || export.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required NFS fields");
            }
            if let Err(e) = ensure_mount_helper(&s.hub, "nfs-common", "/sbin/mount.nfs").await {
                return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e);
            }
            s.hub.broadcast(
                "archive_test_status",
                &serde_json::json!({ "stage": "testing" }),
            );
            let _ = std::fs::create_dir_all(tmp_dir);
            let src = format!("{}:{}", server, export);
            // Bound failures; skip NLM and use NFSv3/TCP for NAS compatibility.
            let opts = "nolock,soft,timeo=50,proto=tcp,vers=3";
            let res = sentryusb_shell::run_with_timeout(
                timeout, "mount", &["-t", "nfs", &src, tmp_dir, "-o", opts],
            ).await;
            if res.is_ok() {
                let _ = sentryusb_shell::run_with_timeout(
                    std::time::Duration::from_secs(5), "umount", &[tmp_dir],
                ).await;
            }
            let _ = std::fs::remove_dir(tmp_dir);
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        other => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                &format!("Unknown archive system: {}", other),
            );
        }
    };

    match test_result {
        Ok(()) => {
            info!("[setup] Archive test succeeded for {}", system);
            (StatusCode::OK, Json(serde_json::json!({"success": true})))
        }
        Err(mut err_msg) => {
            // Remove the shell helper's stderr prefix for display.
            if let Some(idx) = err_msg.find("stderr: ") {
                err_msg = err_msg[idx + "stderr: ".len()..].to_string();
            }
            tracing::warn!("[setup] Archive test failed for {}: {}", system, err_msg);
            (StatusCode::OK, Json(serde_json::json!({
                "success": false,
                "error": err_msg.trim(),
            })))
        }
    }
}

/// POST /api/setup/preflight
///
/// Body: JSON map of `*_SIZE` keys (CAM_SIZE, MUSIC_SIZE, etc.) as
/// human-readable size strings ("40G", "4GB", "100M").
///
/// Returns whether the proposed sizes will fit on the backingfiles
/// partition with the runtime safety reserve. Used by the wizard to
/// reject Apply before any destructive action runs and direct the
/// user at the snapshot management page when they need to free
/// space.
///
/// On a fresh install where /backingfiles isn't mounted yet, returns
/// `ok: true` with `checked: false` — the actual check will run at
/// setup time and bail with the same message if sizes don't fit.
pub async fn preflight(
    State(_s): State<AppState>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    const SIZE_KEYS: &[&str] = &[
        "CAM_SIZE",
        "MUSIC_SIZE",
        "LIGHTSHOW_SIZE",
        "BOOMBOX_SIZE",
    ];

    let mut requested_kb: u64 = 0;
    let mut breakdown = serde_json::Map::new();
    for key in SIZE_KEYS {
        let raw = body.get(*key).cloned().unwrap_or_default();
        let kb = sentryusb_setup::disk_images::dehumanize(&raw).unwrap_or(0);
        requested_kb = requested_kb.saturating_add(kb);
        breakdown.insert((*key).to_string(), serde_json::json!({
            "raw": raw,
            "kb": kb,
        }));
    }

    // Before partitioning, `/backingfiles` reports root-filesystem capacity.
    if !sentryusb_setup::partition::partitions_exist().await {
        return (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "checked": false,
            "reason": "backingfiles partition not created yet (fresh install)",
            "requested_kb": requested_kb,
            "breakdown": breakdown,
        })));
    }

    // A transiently unmounted partition defers validation to setup.
    let df = sentryusb_shell::run(
        "df", &["--output=size,avail", "--block-size=1K", "/backingfiles/"],
    ).await;

    let (total_kb, avail_kb) = match df {
        Ok(out) => {
            let mut total = 0u64;
            let mut avail = 0u64;
            if let Some(line) = out.lines().last() {
                let mut it = line.split_whitespace();
                if let Some(t) = it.next() {
                    total = t.parse().unwrap_or(0);
                }
                if let Some(a) = it.next() {
                    avail = a.parse().unwrap_or(0);
                }
            }
            (total, avail)
        }
        Err(_) => (0, 0),
    };

    if total_kb == 0 {
        return (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "checked": false,
            "reason": "backingfiles partition not mounted yet (fresh install)",
            "requested_kb": requested_kb,
            "breakdown": breakdown,
        })));
    }

    // Mirror disk_images::available_space_kb: 10% of total, capped 2-10 GB.
    let ten_pct = total_kb / 10;
    let min_pad = 2 * 1024 * 1024; // 2 GB in KB
    let max_pad = 10 * 1024 * 1024; // 10 GB in KB
    let padding = ten_pct.max(min_pad).min(max_pad);
    let usable_kb = avail_kb.saturating_sub(padding);

    if requested_kb <= usable_kb {
        return (StatusCode::OK, Json(serde_json::json!({
            "ok": true,
            "checked": true,
            "requested_kb": requested_kb,
            "available_kb": usable_kb,
            "padding_kb": padding,
            "breakdown": breakdown,
        })));
    }

    let need_kb = requested_kb - usable_kb;
    let need_gb = (need_kb + 1024 * 1024 - 1) / (1024 * 1024);
    let req_gb = requested_kb / 1024 / 1024;
    let avail_gb = usable_kb / 1024 / 1024;

    (StatusCode::OK, Json(serde_json::json!({
        "ok": false,
        "checked": true,
        "requested_kb": requested_kb,
        "available_kb": usable_kb,
        "padding_kb": padding,
        "need_kb": need_kb,
        "breakdown": breakdown,
        "error": format!(
            "Disk images need {} GB but backingfiles has only {} GB free \
             (after safety reserve). Free at least {} GB by deleting \
             snapshots from the snapshot management page, then re-run setup.",
            req_gb, avail_gb, need_gb,
        ),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_config_error() {
        let e: anyhow::Error = sentryusb_setup::ConfigError("two providers".into()).into();
        assert_eq!(SetupFailure::from_error(&e).kind, "config");
    }

    #[test]
    fn classifies_transient_error() {
        let e = anyhow::anyhow!("network unreachable");
        assert_eq!(SetupFailure::from_error(&e).kind, "transient");
    }

    #[test]
    fn from_error_keeps_the_message() {
        let e: anyhow::Error = sentryusb_setup::ConfigError("SENTRY_CASE must be 1-3".into()).into();
        assert!(SetupFailure::from_error(&e).message.contains("SENTRY_CASE"));
    }

    #[test]
    fn failure_json_round_trips() {
        let f = SetupFailure { kind: "config", message: "bad \"quoted\" value".into() };
        let parsed = SetupFailure::parse(&f.to_json().to_string()).unwrap();
        assert_eq!(parsed, f);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(SetupFailure::parse("not json at all").is_none());
    }

    #[test]
    fn dedup_phases_collapses_repeated_ids_keeping_first() {
        let ledger = concat!(
            r#"{"id":"verify","label":"Verifying configuration"}"#, "\n",
            r#"{"id":"root_shrink","label":"Shrinking root filesystem"}"#, "\n",
            r#"{"id":"root_shrink","label":"Shrinking root partition table"}"#, "\n",
            r#"{"id":"root_shrink","label":"Shrinking root partition table"}"#, "\n",
            r#"{"id":"gadget","label":"USB gadget overlay"}"#, "\n",
        );
        let phases = dedup_phases(ledger);
        assert_eq!(phases.len(), 3);
        assert_eq!(phases[0]["id"], "verify");
        assert_eq!(phases[1]["id"], "root_shrink");
        assert_eq!(phases[1]["label"], "Shrinking root filesystem");
        assert_eq!(phases[2]["id"], "gadget");
    }

    #[test]
    fn dedup_phases_skips_malformed_lines() {
        let phases = dedup_phases("not json\n{\"id\":\"a\",\"label\":\"A\"}\n");
        assert_eq!(phases.len(), 1);
        assert_eq!(phases[0]["id"], "a");
    }

    #[test]
    fn config_failure_blocks_auto_resume() {
        let f = SetupFailure { kind: "config", message: "x".into() };
        assert!(!should_auto_resume(true, false, Some(&f)));
    }

    #[test]
    fn transient_failure_still_auto_resumes() {
        let f = SetupFailure { kind: "transient", message: "x".into() };
        assert!(should_auto_resume(true, false, Some(&f)));
    }

    #[test]
    fn mid_flow_reboot_with_no_failure_resumes() {
        assert!(should_auto_resume(true, false, None));
    }

    #[test]
    fn finished_setup_does_not_resume() {
        assert!(!should_auto_resume(true, true, None));
    }

    #[test]
    fn never_started_does_not_resume() {
        assert!(!should_auto_resume(false, false, None));
    }
}
