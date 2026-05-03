use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;
use sentryusb_drives::{DriveStore, grouper};

/// Filename kept identical to Go (`server/api/keepawake.go`) so existing
/// `awake_stop` deployments work without changes after this binary lands.
const KEEP_AWAKE_WANTED_FLAG: &str = "/tmp/keep_awake_webui_wanted";

fn keep_awake_owners() -> &'static Mutex<HashSet<String>> {
    static OWNERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Register `owner` as wanting keep-awake. Writes the wanted-flag on the
/// 0→1 transition so `awake_stop`'s top-of-script handoff guard sees it.
/// Idempotent.
pub fn register_keep_awake_want(owner: &str) {
    let mut set = keep_awake_owners().lock().unwrap();
    let was_empty = set.is_empty();
    set.insert(owner.to_string());
    if was_empty {
        let _ = std::fs::write(KEEP_AWAKE_WANTED_FLAG, b"");
    }
}

/// Release `owner`. Removes the wanted-flag on the 1→0 transition.
pub fn release_keep_awake_want(owner: &str) {
    let mut set = keep_awake_owners().lock().unwrap();
    set.remove(owner);
    if set.is_empty() {
        let _ = std::fs::remove_file(KEEP_AWAKE_WANTED_FLAG);
    }
}

/// Clear the wanted-flag and reset the registry. Call on startup so a
/// crashed prior run doesn't leave a stale flag deferring `awake_stop`
/// forever.
pub fn clear_keep_awake_wanted() {
    keep_awake_owners().lock().unwrap().clear();
    let _ = std::fs::remove_file(KEEP_AWAKE_WANTED_FLAG);
}

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
    match read_archive_status() {
        Some(v) => v.get("phase").and_then(|p| p.as_str()) == Some("archiving"),
        None => false,
    }
}

/// Read and parse /tmp/archive_status.json, returning None if absent, stale, or invalid.
/// Removes the file if its mtime is older than 120s (same as Go's IsArchiving).
fn read_archive_status() -> Option<serde_json::Value> {
    const STATUS: &str = "/tmp/archive_status.json";
    let meta = std::fs::metadata(STATUS).ok()?;
    if let Ok(modified) = meta.modified() {
        if let Ok(age) = std::time::SystemTime::now().duration_since(modified) {
            if age > std::time::Duration::from_secs(120) {
                let _ = std::fs::remove_file(STATUS);
                return None;
            }
        }
    }
    let data = std::fs::read_to_string(STATUS).ok()?;
    serde_json::from_str(&data).ok()
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

/// GET /api/drives — list all drives (summaries only).
///
/// Returns a pre-computed JSON string stored in the `meta` table. The
/// cache is built once at startup (or after any mutation) and served
/// directly on every subsequent request — no grouper work, no BLOB
/// decoding, no ORDER-BY sorter allocation.
pub async fn list_drives(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.get_cached_drives_json() {
        Ok(json) => match serde_json::from_str(&json) {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("drive cache parse: {}", e),
            ),
        },
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/{id} — single drive with full point data.
///
/// Two-stage fetch to keep heap usage proportional to the one drive
/// being rendered rather than the whole store:
///
/// 1. Load BLOB-free summaries and resolve `id` to the list of file
///    paths that make up that drive (typically 1-20 clips).
/// 2. Decode full BLOBs for only those files via
///    `with_routes_by_files`. `build_single_drive` still expects a
///    `&[Route]` slice, so the second fetch produces exactly that
///    subset.
pub async fn single_drive(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let tags = state.drives.store.get_all_drive_tags().unwrap_or_default();

    let (idx, files) = match state
        .drives
        .store
        .with_route_summaries(|summaries| grouper::find_drive_files(summaries, &id))
    {
        Ok(Some((i, f))) => (i, f),
        Ok(None) => {
            return crate::json_error(
                StatusCode::NOT_FOUND,
                &format!(
                    "drive not found: summary lookup returned None for id='{}'",
                    id
                ),
            )
        }
        Err(e) => return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
    let file_count = file_refs.len();

    match state.drives.store.with_routes_by_files(&file_refs, |routes| {
        (
            routes.len(),
            grouper::build_single_drive_from_clips(routes, idx as i32, &tags),
        )
    }) {
        Ok((_, Some(drive))) => (
            StatusCode::OK,
            Json(serde_json::to_value(drive).unwrap_or_default()),
        ),
        Ok((rows_fetched, None)) => crate::json_error(
            StatusCode::NOT_FOUND,
            &format!(
                "drive not found: id={} idx={} requested {} files, DB returned {} rows, builder returned None",
                id, idx, file_count, rows_fetched
            ),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("with_routes_by_files failed: {}", e),
        ),
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
    let importing = state.drives.importing.load(Ordering::SeqCst);

    let mut resp = serde_json::json!({
        "running":   status.running,
        "importing": importing,
        "archiving": is_archiving(),
    });

    if status.total_files > 0 {
        resp["process_current"] = status.processed_files.into();
        resp["process_total"]   = status.total_files.into();
    }

    // Merge archive progress fields (phase, current, total) from archiveloop's
    // status file so the dashboard progress bar has the data it needs.
    if let Some(archive) = read_archive_status() {
        if let Some(obj) = archive.as_object() {
            for (k, v) in obj {
                resp[k] = v.clone();
            }
        }
    }

    (StatusCode::OK, Json(resp))
}

/// GET /api/drives/migration-status — surface the v1→v2 aggregate
/// backfill state so the iOS / web app can render a "Migrating drive
/// data..." banner during a first-boot-after-upgrade. Safe to poll at
/// 2-3s cadence; reads three atomics + a small mutex-guarded string,
/// no SQLite contention. Mirrors Go `dh.migrationStatus`
/// (server/api/drives.go:151+).
///
/// Response shape:
///
/// ```json
/// {
///   "active": true,
///   "done": 1234,
///   "total": 5500,
///   "pct": 22.4,
///   "error": "",
///   "disk_full": false
/// }
/// ```
///
/// `active=false` + `error=""` + `done==total` ⇒ migration finished.
/// `active=false` + `error!=""` ⇒ failed/paused; `disk_full=true` means
/// "free space then reboot".
pub async fn migration_status(
    State(_state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let s = sentryusb_drives::migration_status();
    let pct = if s.total > 0 {
        let raw = 100.0 * (s.done as f64) / (s.total as f64);
        if raw > 100.0 { 100.0 } else { raw }
    } else {
        0.0
    };
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "active": s.active,
            "done": s.done,
            "total": s.total,
            "pct": pct,
            "error": s.error,
            "disk_full": s.disk_full,
        })),
    )
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
            register_keep_awake_want("processor");
            start_keep_awake("Drive Processing");
        }
        let result = processor.process_new().await;
        if !post_archive {
            release_keep_awake_want("processor");
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
        register_keep_awake_want("processor");
        start_keep_awake("Drive Processing");
        let result = processor.reprocess_all().await;
        release_keep_awake_want("processor");
        stop_keep_awake();
        if let Err(e) = result {
            tracing::warn!("drive reprocessing error: {}", e);
        }
    });
    crate::json_ok()
}

/// GET /api/drives/stats — aggregate stats
/// Served from the pre-computed cache; no grouper work or BLOB decoding per request.
/// `processed_count` is injected live from the atomic counter (it changes on
/// every processed clip, independent of the route/tags cache invalidation key).
pub async fn drive_stats(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.get_cached_drive_stats_json() {
        Ok(json) => match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(mut v) => {
                v["processed_count"] = state.drives.store.processed_count().into();
                (StatusCode::OK, Json(v))
            }
            Err(e) => crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("drive stats cache parse: {}", e),
            ),
        },
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/drives/fsd-analytics?period=day|week|all — FSD analytics.
///
/// `period=week` (and missing/invalid) is served from the pre-computed
/// meta cache (no grouper work, no BLOB decoding) — this is what the
/// FSD page hits on first paint. `period=day` and `period=all` recompute
/// against the live route_summaries since the cache is week-shaped only.
///
/// Earlier the handler ignored the query entirely and always returned
/// the week cache, so the Day / Week / All Time toggle on the FSD page
/// silently no-op'd. Combined with a frontend bug that crashed when
/// `fsd_grade` was undefined, hitting the FSD button on a fresh-DB Pi
/// produced an unhandled `Cannot read properties of undefined (reading
/// 'length')` from the toggle's first click.
pub async fn fsd_analytics(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let period = params
        .get("period")
        .map(|s| s.as_str())
        .unwrap_or("week");
    let period = match period {
        "day" | "week" | "all" => period,
        _ => "week",
    };

    if period == "week" {
        return match state.drives.store.get_cached_fsd_analytics_json() {
            Ok(json) => match serde_json::from_str::<serde_json::Value>(&json) {
                Ok(v) => (StatusCode::OK, Json(v)),
                Err(e) => crate::json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("fsd analytics cache parse: {}", e),
                ),
            },
            Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
    }

    // Day / All — recompute against current route summaries. This is
    // O(routes), not O(routes * BLOB-size), because we go through the
    // BLOB-free `with_route_summaries` path.
    let store = state.drives.store.clone();
    let period_owned = period.to_string();
    let result = tokio::task::spawn_blocking(move || {
        store.with_route_summaries(|summaries| {
            sentryusb_drives::grouper::fsd_analytics_from_summaries_for_period(
                summaries,
                &period_owned,
            )
        })
    })
    .await;
    match result {
        Ok(Ok(analytics)) => match serde_json::to_value(&analytics) {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("fsd analytics serialize: {}", e),
            ),
        },
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("fsd analytics task: {}", e),
        ),
    }
}

/// GET /api/drives/data/download — download drive data as JSON.
///
/// Streams the exported JSON directly from a tempfile to the HTTP
/// response body without ever buffering the full document in memory.
/// Prior implementation read the tempfile into a `String`, parsed it
/// into a `serde_json::Value`, then re-serialized — three allocations
/// of a file that could easily be 10-20 MB. Combined with the
/// streaming export in `json_compat::export_json`, peak heap for this
/// endpoint drops from ~30 MB to a few hundred KB.
pub async fn download_data(
    State(state): State<AppState>,
) -> axum::response::Response {
    use axum::body::Body;
    use axum::http::header;
    use axum::response::IntoResponse;

    // Export is a blocking rusqlite walk; keep it off the tokio reactor.
    let store = state.drives.store.clone();
    let tmp = "/tmp/drive-data-export.json".to_string();
    let export_path = tmp.clone();
    let export_result = tokio::task::spawn_blocking(move || {
        store.export_json_to_file(&export_path)
    })
    .await;

    match export_result {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            let (status, json) = crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
            return (status, json).into_response();
        }
        Err(e) => {
            let (status, json) = crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("export task panic: {}", e));
            return (status, json).into_response();
        }
    }

    let file = match tokio::fs::File::open(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            let (status, json) = crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
            return (status, json).into_response();
        }
    };

    // `ReaderStream` pulls 8 KB at a time from the file into Bytes
    // chunks; those chunks are handed to hyper and flushed to the
    // socket as they become available. Nothing full-file is resident.
    let stream = tokio_util::io::ReaderStream::new(file);
    axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"drive-data.json\"",
        )
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            // Builder only fails on malformed headers (static here);
            // fall back to a plain JSON error for completeness.
            let (status, json) = crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response build failed",
            );
            (status, json).into_response()
        })
}

/// POST /api/drives/data/export-for-sync
///
/// Regenerate `/mutable/drive-data.json` from the live SQLite store so
/// `post-archive-process.sh` can ship it to the rsync / rclone archive
/// server. Returns the byte count of the regenerated file so the shell
/// script can log it.
///
/// Runs on the blocking thread pool so the 3+ minute regeneration on a
/// well-used Pi (~848 MB on a year of dashcam data) doesn't block any
/// async runtime worker. `DriveStore::export_json_for_sync` opens its
/// own read-only SQLite handle, so the writer mutex stays free for
/// concurrent `/api/drives` requests too.
pub async fn export_for_sync(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = state.drives.store.clone();
    let export_result = tokio::task::spawn_blocking(move || store.export_json_for_sync()).await;
    match export_result {
        Ok(Ok(())) => {
            let bytes = std::fs::metadata(sentryusb_drives::db::DEFAULT_JSON_MIRROR_PATH)
                .map(|m| m.len())
                .unwrap_or(0);
            (
                StatusCode::OK,
                Json(serde_json::json!({ "status": "ok", "bytes": bytes })),
            )
        }
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("export task panic: {}", e),
        ),
    }
}

/// POST /api/drives/data/upload — upload drive data JSON.
///
/// Streams the request body to a temp file chunk-by-chunk, then runs the
/// JSON import in a blocking task. The import itself runs in a blocking task;
/// `importing` is held for the duration so concurrent `process`/`reprocess`
/// requests 409.
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

    // Stream body → temp file.
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
    // live progress bar during what may be a multi-minute restore.
    // Phases: starting → progress (every 50 routes) → complete/error.
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

/// DELETE /api/drives/data — wipe all drive data (routes, processed_files, tags).
pub async fn delete_all_drives(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    if state.drives.processor.is_running() {
        return crate::json_error(
            StatusCode::CONFLICT,
            "processing in progress — please wait until it finishes",
        );
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
    match state.drives.store.clear_all_drives() {
        Ok(()) => (
            StatusCode::OK,
            Json(serde_json::json!({"deleted": true})),
        ),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
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
