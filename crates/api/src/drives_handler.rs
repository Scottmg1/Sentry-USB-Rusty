use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;
use sentryusb_drives::{DriveStore, grouper};

/// Drive-specific state.
#[derive(Clone)]
pub struct DriveState {
    pub store: Arc<DriveStore>,
    pub processor: Arc<sentryusb_drives::processor::Processor>,
    /// Set while an external drive-data import (JSON upload) is running.
    /// Blocks processing and reprocessing until the import completes, matching
    /// Go's `dh.importing` flag (server/api/drives.go:283-287, 378-381).
    pub importing: Arc<AtomicBool>,
}

/// True if archiveloop is currently archiving. Mirrors Go `IsArchiving`:
/// /tmp/archive_status.json present, mtime within 120s, phase == "archiving".
pub fn is_archiving() -> bool {
    const STATUS: &str = "/tmp/archive_status.json";
    let meta = match std::fs::metadata(STATUS) {
        Ok(m) => m,
        Err(_) => return false,
    };
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = std::time::SystemTime::now().duration_since(modified) {
            if age > std::time::Duration::from_secs(120) {
                let _ = std::fs::remove_file(STATUS);
                return false;
            }
        }
    }
    let data = match std::fs::read_to_string(STATUS) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let v: serde_json::Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(_) => return false,
    };
    v.get("phase").and_then(|p| p.as_str()) == Some("archiving")
}

/// Sources envsetup.sh + exports shared PID file so awake_start/awake_stop
/// coordinate with archiveloop's own keep-awake management. Same preamble as
/// Go `awakeShellPreamble` (server/api/drives.go:238-246).
pub(crate) const AWAKE_PREAMBLE: &str = r#"source /root/bin/envsetup.sh 2>/dev/null || true
declare -F log > /dev/null 2>&1 || {
  function log { echo "$(date): $*" >> "${LOG_FILE:-/mutable/archiveloop.log}" 2>/dev/null || true; }
  export -f log
}
export KEEP_AWAKE_PID_FILE=/tmp/keep_awake_nudge_pid
"#;

pub(crate) fn shell_quote(s: &str) -> String {
    let escaped = s.replace('\'', r#"'\''"#);
    format!("'{}'", escaped)
}

/// Launch awake_start in the background. `expires_at_unix` is passed through
/// so nudge logs can show time remaining (Go drives.go:251-265).
pub(crate) fn start_keep_awake_with(reason: &str, expires_at_unix: Option<i64>) {
    let mut script = AWAKE_PREAMBLE.to_string();
    script.push_str(&format!("export KEEP_AWAKE_REASON={}\n", shell_quote(reason)));
    if let Some(ts) = expires_at_unix {
        script.push_str(&format!("export KEEP_AWAKE_EXPIRES_AT={}\n", ts));
    }
    script.push_str("/root/bin/awake_start");
    tokio::spawn(async move {
        if let Err(e) = sentryusb_shell::run("/bin/bash", &["-c", &script]).await {
            tracing::warn!("[drives] awake_start failed: {}", e);
        }
    });
}

pub(crate) fn stop_keep_awake_bg() {
    let script = format!("{}/root/bin/awake_stop", AWAKE_PREAMBLE);
    tokio::spawn(async move {
        if let Err(e) = sentryusb_shell::run("/bin/bash", &["-c", &script]).await {
            tracing::warn!("[drives] awake_stop failed: {}", e);
        }
    });
}

fn start_keep_awake(reason: &'static str) {
    start_keep_awake_with(reason, None);
}

fn stop_keep_awake() {
    stop_keep_awake_bg();
}

#[derive(Deserialize, Default)]
pub struct ProcessQuery {
    #[serde(default)]
    post_archive: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct ProcessBody {
    #[serde(default)]
    pub clips_dir: Option<String>,
    #[serde(default)]
    pub throttle_ms: Option<u64>,
}

/// GET /api/drives — list all drives (summaries only)
pub async fn list_drives(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.with_routes(|routes| {
        let tags = state.drives.store.get_all_drive_tags().unwrap_or_default();
        grouper::group_summaries(routes, &tags)
    }) {
        Ok(summaries) => (StatusCode::OK, Json(serde_json::to_value(summaries).unwrap_or_default())),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/{id} — single drive with full point data
pub async fn single_drive(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.with_routes(|routes| {
        let tags = state.drives.store.get_all_drive_tags().unwrap_or_default();
        grouper::build_single_drive(routes, &id, &tags)
    }) {
        Ok(Some(drive)) => (StatusCode::OK, Json(serde_json::to_value(drive).unwrap_or_default())),
        Ok(None) => crate::json_error(StatusCode::NOT_FOUND, "drive not found"),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/routes — overview routes for map
pub async fn all_routes(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.with_routes(|routes| {
        grouper::route_overviews(routes, 500)
    }) {
        Ok(overviews) => (StatusCode::OK, Json(serde_json::to_value(overviews).unwrap_or_default())),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/tags — list all tags
pub async fn list_tags(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.get_all_tag_names() {
        Ok(tags) => (StatusCode::OK, Json(serde_json::to_value(tags).unwrap_or_default())),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/process and GET /api/drives/status — processing status
pub async fn processing_status(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let status = state.drives.processor.get_status().await;
    (StatusCode::OK, Json(serde_json::to_value(status).unwrap_or_default()))
}

/// POST /api/drives/process — start processing new clips.
///
/// Query: `post_archive=1` — allow running during archiveloop's post-archive
/// hook; skip keep-awake (archiveloop manages its own) and bypass the
/// IsArchiving guard. Mirrors Go drives.go:292-294,326-332.
pub async fn process_files(
    State(state): State<AppState>,
    Query(q): Query<ProcessQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    if state.drives.processor.is_running() {
        return crate::json_error(StatusCode::CONFLICT, "processing already in progress");
    }
    if state.drives.importing.load(Ordering::SeqCst) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "drive data import in progress — please wait until it finishes",
        );
    }
    let post_archive = q.post_archive.as_deref() == Some("1");
    if !post_archive && is_archiving() {
        return crate::json_error(
            StatusCode::CONFLICT,
            "archive is currently running — please wait until it finishes",
        );
    }

    let processor = state.drives.processor.clone();
    tokio::spawn(async move {
        if !post_archive {
            start_keep_awake("Drive Processing");
        }
        let result = processor.process_new().await;
        if !post_archive {
            stop_keep_awake();
        }
        if let Err(e) = result {
            tracing::warn!("drive processing error: {}", e);
        }
    });
    crate::json_ok()
}

/// POST /api/drives/reprocess — reprocess all clips
pub async fn reprocess_all(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    if state.drives.processor.is_running() {
        return crate::json_error(StatusCode::CONFLICT, "processing already in progress");
    }
    if state.drives.importing.load(Ordering::SeqCst) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "drive data import in progress — please wait until it finishes",
        );
    }
    if is_archiving() {
        return crate::json_error(
            StatusCode::CONFLICT,
            "archive is currently running — please wait until it finishes",
        );
    }

    let processor = state.drives.processor.clone();
    tokio::spawn(async move {
        start_keep_awake("Drive Processing");
        let result = processor.reprocess_all().await;
        stop_keep_awake();
        if let Err(e) = result {
            tracing::warn!("drive reprocessing error: {}", e);
        }
    });
    crate::json_ok()
}

/// GET /api/drives/stats — aggregate stats
/// Manually builds snake_case JSON to match Go API output expected by frontend.
pub async fn drive_stats(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let processed_count = state.drives.store.processed_count();
    match state.drives.store.with_routes(|routes| {
        grouper::compute_aggregate_stats(routes)
    }) {
        Ok(stats) => {
            let r = |v: f64| -> f64 { (v * 100.0).round() / 100.0 };
            (StatusCode::OK, Json(serde_json::json!({
                "drives_count": stats.drives_count,
                "routes_count": stats.routes_count,
                "processed_count": processed_count,
                "total_distance_km": r(stats.total_distance_km),
                "total_distance_mi": r(stats.total_distance_mi),
                "total_duration_ms": stats.total_duration_ms,
                "fsd_engaged_ms": stats.fsd_engaged_ms,
                "fsd_distance_km": r(stats.fsd_distance_km),
                "fsd_distance_mi": r(stats.fsd_distance_mi),
                "fsd_percent": stats.fsd_percent,
                "fsd_disengagements": stats.fsd_disengagements,
                "fsd_accel_pushes": stats.fsd_accel_pushes,
                "autosteer_engaged_ms": stats.autosteer_engaged_ms,
                "autosteer_distance_km": r(stats.autosteer_distance_km),
                "autosteer_distance_mi": r(stats.autosteer_distance_mi),
                "tacc_engaged_ms": stats.tacc_engaged_ms,
                "tacc_distance_km": r(stats.tacc_distance_km),
                "tacc_distance_mi": r(stats.tacc_distance_mi),
                "assisted_percent": stats.assisted_percent,
            })))
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/fsd-analytics — FSD analytics
pub async fn fsd_analytics(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.with_routes(|routes| {
        grouper::fsd_analytics(routes)
    }) {
        Ok(analytics) => (StatusCode::OK, Json(serde_json::to_value(analytics).unwrap_or_default())),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/data/download — download drive data as JSON
pub async fn download_data(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Export as JSON for compatibility
    let tmp = "/tmp/drive-data-export.json";
    match state.drives.store.export_json_to_file(tmp) {
        Ok(()) => {
            match std::fs::read_to_string(tmp) {
                Ok(data) => match serde_json::from_str::<serde_json::Value>(&data) {
                    Ok(v) => (StatusCode::OK, Json(v)),
                    Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
                },
                Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
            }
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// POST /api/drives/data/export-for-sync
///
/// Regenerate `/mutable/drive-data.json` from the live SQLite store so
/// `post-archive-process.sh` can ship it to the rsync / rclone archive
/// server. Returns the byte count of the regenerated file so the shell
/// script can log it.
pub async fn export_for_sync(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.export_json_for_sync() {
        Ok(()) => {
            let bytes = std::fs::metadata(sentryusb_drives::db::DEFAULT_JSON_MIRROR_PATH)
                .map(|m| m.len())
                .unwrap_or(0);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "ok", "bytes": bytes })),
            )
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Max accepted drive-data upload size. Mirrors Go's streamed-file size guard
/// at drives.go:518-605 — prevents OOM on a malicious/malformed upload before
/// we hand the payload to the JSON parser.
const MAX_UPLOAD_BYTES: usize = 256 * 1024 * 1024; // 256 MiB

/// POST /api/drives/data/upload — upload drive data JSON.
///
/// Streams the request body to a temp file chunk-by-chunk with a running
/// byte counter. If the cap is exceeded mid-stream, we abort, unlink the
/// partial file, and return 413 without ever holding the full payload in
/// memory. Mirrors Go's streamed implementation (drives.go:518-605) so OOM
/// risk on Pi Zero stays tied to the cap, not the body size.
///
/// The import itself runs in a blocking task; `importing` is held for the
/// duration so concurrent `process`/`reprocess` requests 409.
pub async fn upload_data(
    State(state): State<AppState>,
    body: axum::body::Body,
) -> (StatusCode, Json<serde_json::Value>) {
    use axum::body::Body;
    use futures_util::StreamExt;
    use std::io::Write;

    if state.drives.processor.is_running() {
        return crate::json_error(
            StatusCode::CONFLICT,
            "processing in progress — please wait until it finishes",
        );
    }
    if state.drives.importing.swap(true, Ordering::SeqCst) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "drive data import already in progress",
        );
    }

    let tmp = "/tmp/drive-data-upload.json";

    // Stream body → temp file, bailing if we cross the cap.
    let stream_result: Result<usize, (StatusCode, String)> = async {
        let file = std::fs::File::create(tmp)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        let mut file = std::io::BufWriter::new(file);
        let mut written: usize = 0;
        let mut stream = Body::into_data_stream(body);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| (StatusCode::BAD_REQUEST, format!("read body: {}", e)))?;
            written += chunk.len();
            if written > MAX_UPLOAD_BYTES {
                let _ = std::fs::remove_file(tmp);
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!(
                        "upload exceeds {} MiB",
                        MAX_UPLOAD_BYTES / (1024 * 1024)
                    ),
                ));
            }
            file.write_all(&chunk)
                .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        }
        file.flush()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
        Ok(written)
    }
    .await;

    if let Err((status, msg)) = stream_result {
        state.drives.importing.store(false, Ordering::SeqCst);
        return crate::json_error(status, &msg);
    }

    // Emit `drive_import` WebSocket events so the web UI can show a
    // progress state during what may be a multi-minute restore. Matches
    // Go's per-phase broadcasts in server/api/drives.go:557-609 —
    // starting → progress → complete/error. Our progress fires once
    // with the total route count (the JSON decoder materializes the
    // whole StoreData before we see routes). The phases still give the
    // frontend enough signal to swap the "uploading" UI for an
    // "importing 5243 routes" UI instead of a stale spinner.
    let hub = state.hub.clone();
    hub.broadcast("drive_import", &serde_json::json!({"phase": "starting"}));

    let store = state.drives.store.clone();
    let importing = state.drives.importing.clone();
    let hub_task = hub.clone();
    let result = tokio::task::spawn_blocking(move || {
        let hub_cb = hub_task.clone();
        let res = store.import_json_file_with_progress(tmp, move |routes| {
            hub_cb.broadcast(
                "drive_import",
                &serde_json::json!({"phase": "progress", "routes": routes}),
            );
        });
        importing.store(false, Ordering::SeqCst);
        res
    })
    .await;

    // Best-effort cleanup; ignore errors (e.g. already-removed on panic).
    let _ = std::fs::remove_file(tmp);

    match result {
        Ok(Ok(stats)) => {
            hub.broadcast(
                "drive_import",
                &serde_json::json!({
                    "phase": "complete",
                    "routes": stats.routes,
                    "processedFiles": stats.processed_files,
                    "driveTags": stats.drive_tags,
                }),
            );
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "imported": stats.routes,
                    "routes": stats.routes,
                    "processedFiles": stats.processed_files,
                    "driveTags": stats.drive_tags,
                })),
            )
        }
        Ok(Err(e)) => {
            hub.broadcast(
                "drive_import",
                &serde_json::json!({"phase": "error", "error": e.to_string()}),
            );
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
        Err(e) => {
            hub.broadcast(
                "drive_import",
                &serde_json::json!({"phase": "error", "error": e.to_string()}),
            );
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// PUT /api/drives/{id}/tags — set tags for a drive
pub async fn set_drive_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.set_drive_tags(&id, &body.tags) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct SetTagsRequest {
    pub tags: Vec<String>,
}
