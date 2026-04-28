//! Setup wizard configuration API.
//!
//! The setup process supports mid-setup reboots (e.g. for dwc2 overlay or
//! root partition shrink). The boot-loop works like this:
//!
//! 1. User clicks "Run Setup" in the web wizard → `POST /api/setup/run`
//! 2. `run_full_setup` creates `SENTRYUSB_SETUP_STARTED`, runs phases.
//! 3. If a phase requires a reboot, setup exits early (marker still present).
//! 4. Pi reboots → systemd starts the web server → `auto_resume_setup()`
//!    sees STARTED without FINISHED → re-spawns `run_full_setup`.
//! 5. `run_full_setup` skips already-completed phases and continues.
//! 6. When all phases finish, STARTED is removed and FINISHED is created.

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

/// Call at server startup to resume an interrupted setup after reboot.
pub fn auto_resume_setup(hub: sentryusb_ws::Hub) {
    if is_setup_started() && !is_setup_finished() {
        info!("[setup] Detected interrupted setup (STARTED marker present, no FINISHED). Auto-resuming...");
        spawn_setup(hub);
    }
}

/// GET /api/setup/status
pub async fn get_setup_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let running = SETUP_RUNNING.load(Ordering::Relaxed);
    let finished = is_setup_finished();

    // If setup was started but not finished, treat as running
    let effective_running = running || (!finished && is_setup_started());

    (StatusCode::OK, Json(serde_json::json!({
        "setup_finished": finished,
        "setup_running": effective_running,
    })))
}

/// GET /api/setup/config
pub async fn get_setup_config(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
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
            (StatusCode::OK, Json(serde_json::Value::Object(merged)))
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to read config: {}", e)),
    }
}

/// PUT /api/setup/config
pub async fn save_setup_config(
    State(_s): State<AppState>,
    Json(body): Json<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Remount filesystem read-write (root fs may be read-only)
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    // The vendored bash archive scripts (run/{cifs,rsync,rclone,nfs}_archive/
    // archive-is-reachable.sh and friends) all read `$ARCHIVE_SERVER`
    // and pass it as $1 to the reachability probe. The wizard, though,
    // collects the per-system server name in a per-system variable
    // (RSYNC_SERVER for rsync, RCLONE_DRIVE for rclone). Without
    // mirroring it into ARCHIVE_SERVER, archiveloop hands the bash
    // script an empty string, the script exits 1 with "Name or service
    // not known", and the loop is permanently stuck on
    // "Waiting for archive to be reachable...".
    //
    // CIFS and NFS already use ARCHIVE_SERVER directly in the wizard,
    // so they're fine unchanged. Mirror only when the user-provided
    // ARCHIVE_SERVER is empty so we don't clobber an explicit value.
    let body = mirror_archive_server(body);

    let config_path = sentryusb_config::find_config_path();
    match sentryusb_config::write_file(config_path, &body) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to write config: {}", e)),
    }
}

/// Backfill ARCHIVE_SERVER from RSYNC_SERVER for rsync setups so the
/// legacy bash archive scripts (which all read $ARCHIVE_SERVER) get a
/// real hostname to probe. cifs and nfs already collect ARCHIVE_SERVER
/// directly in the wizard. rclone is intentionally NOT mirrored: its
/// per-system key is RCLONE_DRIVE (a remote name like "myremote"), not
/// a pingable hostname — copying that into ARCHIVE_SERVER would just
/// substitute one form of "Name or service not known" for another.
/// rclone instead has its own ARCHIVE_SERVER input (an IP to ping for
/// liveness), validated separately on the wizard side.
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
                // Surface the error to the wizard's live log too. Without
                // this, the failure only lands in journalctl and the
                // wizard log just stops mid-phase with no explanation —
                // the user sees "Mounting backingfiles partition..." as
                // the last line and has no way to know what went wrong.
                let line = format!("ERROR: setup failed: {:#}", e);
                let stamped = format!(
                    "{} : {}",
                    std::process::Command::new("date")
                        .output()
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
                hub.broadcast("setup", &serde_json::json!({"status": "error", "error": e.to_string()}));
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
    let phases: Vec<serde_json::Value> = std::fs::read_to_string(SETUP_PHASES_FILE)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    (StatusCode::OK, Json(serde_json::json!({ "phases": phases })))
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
/// Mirrors `server/api/setup.go:testArchive` — an actual mount/connect probe,
/// not just a ping.
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

    // `mount -t nfs` / `-t cifs` need userspace helpers (`mount.nfs` from
    // nfs-common, `mount.cifs` from cifs-utils). Without them the kernel
    // falls through to its own mount API which can't parse `server:/export`
    // or `//server/share` — the user-visible symptom is:
    //   "NFS: mount program didn't pass remote address. fsconfig() failed"
    // so we install the helper on demand before running the mount test.
    // Idempotent: apt-get skips already-installed packages quickly.
    //
    // Runs in a single request: the frontend's fetch awaits this whole
    // flow, so the "Testing..." spinner stays up through install +
    // mount probe. We also broadcast `archive_test_status` so the UI can
    // show a more specific label ("Installing nfs-common...") instead of
    // leaving the user wondering what's taking so long.
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
        // `DPkg::Lock::Timeout` tells apt to wait up to N seconds for
        // the dpkg frontend lock instead of failing immediately. The
        // common collision is the setup wizard's own
        // install_required_packages phase holding the lock when the
        // user clicks "Test connection" in the Archive step — both
        // are legitimate apt invocations racing for the same lock.
        // Shell timeout is a little higher than the apt wait so a
        // pathological hang surfaces cleanly.
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
            let target = format!("{}@{}", user, server);
            let res = sentryusb_shell::run_with_timeout(
                timeout, "ssh", &[
                    "-o", "ConnectTimeout=10",
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "BatchMode=yes",
                    &target, "echo", "ok",
                ],
            ).await;
            res.map(|_| ()).map_err(|e| e.to_string())
        }
        "rclone" => {
            let drive = params.get("RCLONE_DRIVE").cloned().unwrap_or_default();
            let rpath = params.get("RCLONE_PATH").cloned().unwrap_or_default();
            if drive.is_empty() || rpath.is_empty() {
                return crate::json_error(StatusCode::BAD_REQUEST, "Missing required rclone fields");
            }
            let target = format!("{}:{}", drive, rpath);
            let res = sentryusb_shell::run_with_timeout(
                timeout, "rclone", &["lsd", &target],
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
            let res = sentryusb_shell::run_with_timeout(
                timeout, "mount", &["-t", "nfs", &src, tmp_dir, "-o", "nolock,soft,timeo=50"],
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
            // Strip the "stderr: " prefix the shell helpers prepend, matching
            // Go's cosmetic cleanup before displaying to the user.
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
        "WRAPS_SIZE",
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

    // Use df on /backingfiles. If not mounted yet (fresh install), we
    // can't compute available — return ok with checked=false so the
    // wizard can proceed and the setup phase will do the real check.
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
