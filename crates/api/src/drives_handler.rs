use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;
use sentryusb_drives::{DriveStore, aggregate_telemetry, grouper};

/// Shared with `awake_stop`.
const KEEP_AWAKE_WANTED_FLAG: &str = "/tmp/keep_awake_webui_wanted";

fn keep_awake_owners() -> &'static Mutex<HashSet<String>> {
    static OWNERS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    OWNERS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Registers an idempotent keep-awake owner and writes the flag on 0→1.
pub fn register_keep_awake_want(owner: &str) {
    let mut set = keep_awake_owners().lock().unwrap();
    let was_empty = set.is_empty();
    set.insert(owner.to_string());
    if was_empty {
        let _ = std::fs::write(KEEP_AWAKE_WANTED_FLAG, b"");
    }
}

/// Releases an owner and removes the flag on 1→0.
pub fn release_keep_awake_want(owner: &str) {
    let mut set = keep_awake_owners().lock().unwrap();
    set.remove(owner);
    if set.is_empty() {
        let _ = std::fs::remove_file(KEEP_AWAKE_WANTED_FLAG);
    }
}

/// Clears stale in-memory and on-disk keep-awake state at startup.
pub fn clear_keep_awake_wanted() {
    keep_awake_owners().lock().unwrap().clear();
    let _ = std::fs::remove_file(KEEP_AWAKE_WANTED_FLAG);
}

#[derive(Clone)]
pub struct DriveState {
    pub store: Arc<DriveStore>,
    pub processor: Arc<sentryusb_drives::processor::Processor>,
    /// Blocks processing and reprocessing during JSON import.
    pub importing: Arc<AtomicBool>,
}

/// Returns true for a fresh archive status whose phase is `archiving`.
pub fn is_archiving() -> bool {
    match read_archive_status() {
        Some(v) => v.get("phase").and_then(|p| p.as_str()) == Some("archiving"),
        None => false,
    }
}

/// Reads archive status and removes it when older than 120 seconds.
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

/// Shell preamble shared with archiveloop's keep-awake management.
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

/// Launches `awake_start` with optional expiry metadata.
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

/// Serves cached JSON verbatim, preserving field order and avoiding reserialization.
fn cached_json_response(body: String) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Lists cached drive summaries without decoding route BLOBs.
pub async fn list_drives(State(state): State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    // A cache miss groups every route, so keep it off async workers.
    let store = state.drives.store.clone();
    match tokio::task::spawn_blocking(move || store.get_cached_drives_json()).await {
        Ok(Ok(json)) => cached_json_response(json),
        Ok(Err(e)) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response()
        }
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("drive list task failed: {}", e),
        )
        .into_response(),
    }
}

/// Resolves a drive from BLOB-free summaries, then decodes only its route files.
pub async fn single_drive(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Grouping and BLOB decoding must not stall the async reactor.
    let store = state.drives.store.clone();
    tokio::task::spawn_blocking(move || single_drive_blocking(store, id))
        .await
        .unwrap_or_else(|_| {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "drive detail task failed")
        })
}

fn single_drive_blocking(
    store: std::sync::Arc<sentryusb_drives::DriveStore>,
    id: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let tags = store.get_all_drive_tags().unwrap_or_default();

    // Resolve files and the canonical list summary in one pass; overlay its
    // aggregates because point-walk and per-clip calculations can drift.
    let (idx, files, summary) = match store
        .with_route_summaries(|summaries| {
            let resolved = grouper::find_drive_files(summaries, &id);
            let summary =
                resolved
                    .as_ref()
                    .and_then(|(i, _)| grouper::build_summary_for_idx(summaries, *i, &tags));
            resolved.map(|(i, f)| (i, f, summary))
        }) {
        Ok(Some((i, f, s))) => (i, f, s),
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

    match store.with_routes_by_files(&file_refs, |routes| {
        (
            routes.len(),
            // Scope shared clips to the requested park-split segment.
            grouper::build_single_drive_from_clips(
                routes,
                idx as i32,
                &tags,
                summary.as_ref().map(|s| s.start_time.as_str()),
            ),
        )
    }) {
        Ok((_, Some(mut drive))) => {
            if let Some(s) = summary.as_ref() {
                // Keep per-point fields from the BLOB walk and headline fields canonical.
                drive.distance_mi = s.distance_mi;
                drive.distance_km = s.distance_km;
                drive.avg_speed_mph = s.avg_speed_mph;
                drive.max_speed_mph = s.max_speed_mph;
                drive.avg_speed_kmh = s.avg_speed_kmh;
                drive.max_speed_kmh = s.max_speed_kmh;
                drive.fsd_engaged_ms = s.fsd_engaged_ms;
                drive.fsd_disengagements = s.fsd_disengagements;
                drive.fsd_accel_pushes = s.fsd_accel_pushes;
                drive.fsd_percent = s.fsd_percent;
                drive.fsd_distance_km = s.fsd_distance_km;
                drive.fsd_distance_mi = s.fsd_distance_mi;
                drive.autosteer_engaged_ms = s.autosteer_engaged_ms;
                drive.autosteer_percent = s.autosteer_percent;
                drive.autosteer_distance_km = s.autosteer_distance_km;
                drive.autosteer_distance_mi = s.autosteer_distance_mi;
                drive.tacc_engaged_ms = s.tacc_engaged_ms;
                drive.tacc_percent = s.tacc_percent;
                drive.tacc_distance_km = s.tacc_distance_km;
                drive.tacc_distance_mi = s.tacc_distance_mi;
                drive.assisted_percent = s.assisted_percent;
                // Summon classification requires summary segment bounds.
                drive.summon = s.summon;
                if s.summon {
                    // Stats parity with Sentry-Drive: a detected Summon
                    // emits no FSD events. Every numeric FSD/Autosteer/
                    // TACC field is already zeroed by the overlay above
                    // (the summary path zeroes them at the source), but
                    // fsd_events comes from the full-BLOB walk and would
                    // otherwise plot a phantom disengagement where the
                    // car parked itself. The per-point fsd_states array
                    // is deliberately KEPT as raw evidence.
                    drive.fsd_events.clear();
                }
            }
            (
                StatusCode::OK,
                Json(serde_json::to_value(drive).unwrap_or_default()),
            )
        }
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

/// Lists cached route overviews, downsampled to `max_points` (2–2000).
pub async fn all_routes(
    State(state): State<AppState>,
    Query(q): Query<AllRoutesQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let max_points = q.max_points.unwrap_or(500).clamp(2, 2000);
    // Cached; a miss runs the grouper over all routes, so keep it off the reactor.
    let store = state.drives.store.clone();
    match tokio::task::spawn_blocking(move || store.get_cached_route_overviews_json(max_points)).await {
        Ok(Ok(json)) => cached_json_response(json),
        Ok(Err(e)) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response()
        }
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("routes task: {}", e),
        )
        .into_response(),
    }
}

#[derive(Deserialize, Default)]
pub struct AllRoutesQuery {
    #[serde(default)]
    pub max_points: Option<usize>,
}

/// GET /api/drives/tags — list all tags
pub async fn list_tags(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = state.drives.store.clone();
    match tokio::task::spawn_blocking(move || store.get_all_tag_names()).await {
        Ok(Ok(tags)) => (StatusCode::OK, Json(serde_json::to_value(tags).unwrap_or_default())),
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("tags task: {}", e))
        }
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

    // Include archiveloop progress in the dashboard status.
    if let Some(archive) = read_archive_status() {
        if let Some(obj) = archive.as_object() {
            for (k, v) in obj {
                resp[k] = v.clone();
            }
        }
    }

    (StatusCode::OK, Json(resp))
}

/// Returns lock-free v1→v2 aggregate-backfill progress for UI polling.
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

/// Starts processing new clips. `post_archive=1` delegates keep-awake and
/// archive coordination to archiveloop.
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

/// Re-extracts only clips that may contain missing summon evidence.
pub async fn check_summon(
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
        let result = processor.check_summon().await;
        release_keep_awake_want("processor");
        stop_keep_awake();
        if let Err(e) = result {
            tracing::warn!("summon check error: {}", e);
        }
    });
    crate::json_ok()
}

/// Returns cached aggregate stats with a live processed-file count.
pub async fn drive_stats(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // spawn_blocking: cache misses run the grouper — see list_drives.
    let store = state.drives.store.clone();
    let result =
        match tokio::task::spawn_blocking(move || store.get_cached_drive_stats_json()).await {
            Ok(r) => r,
            Err(e) => {
                return crate::json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("drive stats task failed: {}", e),
                );
            }
        };
    match result {
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

/// Returns FSD analytics; week uses the cache, while day/all use live summaries.
pub async fn fsd_analytics(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let period = params
        .get("period")
        .map(|s| s.as_str())
        .unwrap_or("week");
    let period = match period {
        "day" | "week" | "all" => period,
        _ => "week",
    };

    if period == "week" {
        // spawn_blocking: cache misses run the grouper — see list_drives.
        let store = state.drives.store.clone();
        return match tokio::task::spawn_blocking(move || store.get_cached_fsd_analytics_json())
            .await
        {
            Ok(Ok(json)) => cached_json_response(json),
            Ok(Err(e)) => {
                crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response()
            }
            Err(e) => crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("fsd analytics task failed: {}", e),
            )
            .into_response(),
        };
    }

    // Day/all are O(routes) through BLOB-free summaries.
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
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("fsd analytics serialize: {}", e),
            )
            .into_response(),
        },
        Ok(Err(e)) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response()
        }
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("fsd analytics task: {}", e),
        )
        .into_response(),
    }
}

/// Returns live safety analytics from BLOB-free summaries.
pub async fn safety_analytics(
    State(state): State<AppState>,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let period = params.get("period").map(|s| s.as_str()).unwrap_or("month");
    let period = match period {
        "day" | "week" | "month" | "all" => period,
        _ => "month",
    };

    let store = state.drives.store.clone();
    let period_owned = period.to_string();
    let result = tokio::task::spawn_blocking(move || {
        store.with_route_summaries(|summaries| {
            sentryusb_drives::grouper::safety_analytics_from_summaries_for_period(
                summaries,
                &period_owned,
            )
        })
    })
    .await;
    match result {
        Ok(Ok(analytics)) => match serde_json::to_value(&analytics) {
            Ok(v) => (StatusCode::OK, Json(v)).into_response(),
            Err(e) => crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("safety analytics serialize: {}", e),
            )
            .into_response(),
        },
        Ok(Err(e)) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response()
        }
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("safety analytics task: {}", e),
        )
        .into_response(),
    }
}

/// Streams an exported JSON tempfile without buffering the full document.
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
            // Static headers should be valid; retain a defensive fallback.
            let (status, json) = crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "response build failed",
            );
            (status, json).into_response()
        })
}

/// Regenerates the archive-sync JSON using a read-only handle on the blocking pool.
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

/// Streams an uploaded JSON file to disk, then imports it on the blocking pool.
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

    let bytes_written = match stream_result {
        Ok(n) => n,
        Err((status, msg)) => {
            tracing::warn!(
                "upload_data: body stream failed status={} msg={} tmp={}",
                status.as_u16(),
                msg,
                tmp
            );
            state.drives.importing.store(false, Ordering::SeqCst);
            return crate::json_error(status, &msg);
        }
    };
    tracing::info!(
        "upload_data: received {} byte(s) at {}",
        bytes_written,
        tmp
    );

    // Broadcast starting, periodic progress, and completion/error phases.
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

    // Cleanup is best-effort because the importer may already have removed it.
    let _ = std::fs::remove_file(tmp);

    match result {
        Ok(Ok(stats)) => {
            tracing::info!(
                "upload_data: import success routes={} processed_files={} drive_tags={}",
                stats.routes,
                stats.processed_files,
                stats.drive_tags
            );
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
            tracing::warn!("upload_data: import failed: {}", e);
            hub.broadcast(
                "drive_import",
                &serde_json::json!({"phase": "error", "error": e.to_string()}),
            );
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
        Err(e) => {
            tracing::warn!("upload_data: import task panicked: {}", e);
            hub.broadcast(
                "drive_import",
                &serde_json::json!({"phase": "error", "error": e.to_string()}),
            );
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string())
        }
    }
}

/// Returns the last 20 persisted import diagnostics records.
pub async fn import_history(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = state.drives.store.clone();
    match tokio::task::spawn_blocking(move || store.import_history())
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("import-history task: {}", e)))
    {
        Ok(history) => (
            StatusCode::OK,
            Json(serde_json::json!({ "history": history })),
        ),
        Err(e) => {
            tracing::warn!("import_history: failed to read meta: {}", e);
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

#[derive(Deserialize)]
pub struct BulkDeleteRequest {
    pub ids: Vec<String>,
}

/// Deletes drives resolved from numeric or start-time IDs in one transaction.
/// Processing and import block deletion; archive copying does not write this DB.
pub async fn bulk_delete_drives(
    State(state): State<AppState>,
    Json(body): Json<BulkDeleteRequest>,
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
    if body.ids.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({"deleted": 0, "drives": 0})),
        );
    }

    // Keep grouping and the delete transaction off the async reactor.
    let store = state.drives.store.clone();
    tokio::task::spawn_blocking(move || bulk_delete_drives_blocking(store, body.ids))
        .await
        .unwrap_or_else(|_| {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "bulk delete task failed")
        })
}

fn bulk_delete_drives_blocking(
    store: std::sync::Arc<sentryusb_drives::DriveStore>,
    ids: Vec<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Resolve all IDs in one summary pass.
    let (files, drive_keys, not_found): (Vec<String>, Vec<String>, Vec<String>) = match store
        .with_route_summaries(|summaries| {
            let mut files = std::collections::HashSet::new();
            let mut keys = std::collections::HashSet::new();
            let mut missing = Vec::new();
            for id in &ids {
                let resolved = grouper::find_drive_files(summaries, id);
                let key = grouper::find_drive_start_time(summaries, id);
                match (resolved, key) {
                    (Some((_, fs)), Some(k)) => {
                        for f in fs {
                            files.insert(f);
                        }
                        keys.insert(k);
                    }
                    _ => missing.push(id.clone()),
                }
            }
            (
                files.into_iter().collect(),
                keys.into_iter().collect(),
                missing,
            )
        }) {
        Ok(v) => v,
        Err(e) => return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    if !not_found.is_empty() && files.is_empty() {
        return crate::json_error(
            StatusCode::NOT_FOUND,
            &format!("no matching drives for ids: {}", not_found.join(", ")),
        );
    }

    match store.delete_routes_by_files(&files, &drive_keys) {
        Ok(deleted_routes) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "deleted": deleted_routes,
                "drives": ids.len() - not_found.len(),
                "not_found": not_found,
            })),
        ),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// Sets tags after resolving numeric or start-time IDs to the canonical start time.
pub async fn set_drive_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // ID resolution groups all summaries, so keep it off the reactor.
    let store = state.drives.store.clone();
    tokio::task::spawn_blocking(move || set_drive_tags_blocking(store, id, body.tags))
        .await
        .unwrap_or_else(|_| {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "set tags task failed")
        })
}

fn set_drive_tags_blocking(
    store: std::sync::Arc<sentryusb_drives::DriveStore>,
    id: String,
    tags: Vec<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = match store
        .with_route_summaries(|summaries| grouper::find_drive_start_time(summaries, &id))
    {
        Ok(Some(k)) => k,
        Ok(None) => {
            return crate::json_error(
                StatusCode::NOT_FOUND,
                &format!("drive not found for id='{}'", id),
            )
        }
        Err(e) => return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    match store.set_drive_tags(&key, &tags) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct SetTagsRequest {
    pub tags: Vec<String>,
}

/// Returns interior/exterior temperatures within a drive's clip window.
/// Null values are omitted so clients preserve gaps rather than plotting zero.
pub async fn temperature_series(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // File lookup, window derivation, and query share the connection mutex.
    let store = state.drives.store.clone();
    let id_err = id.clone();
    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<serde_json::Value>> {
            let files = match store
                .with_route_summaries(|summaries| grouper::find_drive_files(summaries, &id))?
            {
                Some((_idx, f)) => f,
                None => return Ok(None),
            };

            // Ignore files without clock timestamps when deriving the telemetry window.
            let mut start_ts: Option<i64> = None;
            let mut end_ts: Option<i64> = None;
            for f in &files {
                if let Some((s, e)) = aggregate_telemetry::window_for_route_file(f) {
                    start_ts = Some(start_ts.map_or(s, |cur| cur.min(s)));
                    end_ts = Some(end_ts.map_or(e, |cur| cur.max(e)));
                }
            }
            let (start_ts, end_ts) = match (start_ts, end_ts) {
                (Some(s), Some(e)) => (s, e),
                _ => return Ok(Some(serde_json::json!({ "points": [] }))),
            };

            let points = store.with_read_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT ts, interior_temp_c, exterior_temp_c \
                     FROM telemetry_samples \
                     WHERE ts BETWEEN ?1 AND ?2 \
                       AND (interior_temp_c IS NOT NULL OR exterior_temp_c IS NOT NULL) \
                     ORDER BY ts ASC",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![start_ts, end_ts],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<f64>>(1)?,
                            r.get::<_, Option<f64>>(2)?,
                        ))
                    },
                )?;
                let mut out: Vec<serde_json::Value> = Vec::new();
                for row in rows {
                    let (ts, interior, exterior) = row?;
                    let mut obj = serde_json::Map::new();
                    // Client timestamps use milliseconds.
                    obj.insert("ts".to_string(), serde_json::json!(ts * 1000));
                    if let Some(v) = interior {
                        obj.insert("interiorC".to_string(), serde_json::json!(v));
                    }
                    if let Some(v) = exterior {
                        obj.insert("exteriorC".to_string(), serde_json::json!(v));
                    }
                    out.push(serde_json::Value::Object(obj));
                }
                Ok::<_, anyhow::Error>(out)
            })?;
            Ok(Some(serde_json::json!({ "points": points })))
        })
        .await;

    match result {
        Ok(Ok(Some(v))) => (StatusCode::OK, Json(v)),
        Ok(Ok(None)) => crate::json_error(
            StatusCode::NOT_FOUND,
            &format!("drive not found for id='{}'", id_err),
        ),
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("series task: {}", e))
        }
    }
}

/// Returns battery samples within a drive's clip window, omitting null values.
pub async fn battery_series(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // File lookup, window derivation, and query share the connection mutex.
    let store = state.drives.store.clone();
    let id_err = id.clone();
    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<serde_json::Value>> {
            let files = match store
                .with_route_summaries(|summaries| grouper::find_drive_files(summaries, &id))?
            {
                Some((_idx, f)) => f,
                None => return Ok(None),
            };

            // Match the temperature-series clip window.
            let mut start_ts: Option<i64> = None;
            let mut end_ts: Option<i64> = None;
            for f in &files {
                if let Some((s, e)) = aggregate_telemetry::window_for_route_file(f) {
                    start_ts = Some(start_ts.map_or(s, |cur| cur.min(s)));
                    end_ts = Some(end_ts.map_or(e, |cur| cur.max(e)));
                }
            }
            let (start_ts, end_ts) = match (start_ts, end_ts) {
                (Some(s), Some(e)) => (s, e),
                _ => return Ok(Some(serde_json::json!({ "points": [] }))),
            };

            let points = store.with_read_conn(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT ts, battery_pct \
                     FROM telemetry_samples \
                     WHERE ts BETWEEN ?1 AND ?2 \
                       AND battery_pct IS NOT NULL \
                     ORDER BY ts ASC",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![start_ts, end_ts],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<f64>>(1)?)),
                )?;
                let mut out: Vec<serde_json::Value> = Vec::new();
                for row in rows {
                    let (ts, battery) = row?;
                    let mut obj = serde_json::Map::new();
                    obj.insert("ts".to_string(), serde_json::json!(ts * 1000));
                    if let Some(v) = battery {
                        obj.insert("batteryPct".to_string(), serde_json::json!(v));
                    }
                    out.push(serde_json::Value::Object(obj));
                }
                Ok::<_, anyhow::Error>(out)
            })?;
            Ok(Some(serde_json::json!({ "points": points })))
        })
        .await;

    match result {
        Ok(Ok(Some(v))) => (StatusCode::OK, Json(v)),
        Ok(Ok(None)) => crate::json_error(
            StatusCode::NOT_FOUND,
            &format!("drive not found for id='{}'", id_err),
        ),
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("series task: {}", e))
        }
    }
}

#[derive(Deserialize, Default)]
pub struct TireHistoryQuery {
    /// Window size in days, clamped to 1–365 and defaulting to 30.
    #[serde(default)]
    pub days: Option<u32>,
}

/// Returns cached per-tire pressure history for the requested 1–365 day window.
/// Cached JSON is shared by `Arc` to avoid cloning large result sets.
static TIRE_HISTORY_CACHE: crate::ttl_cache::StaleWhileRevalidate<u32, std::sync::Arc<String>> =
    crate::ttl_cache::StaleWhileRevalidate::new(std::time::Duration::from_secs(60));

/// Even-stride cap for slowly varying pressure history.
const TIRE_HISTORY_MAX_POINTS: usize = 2_000;

pub async fn tire_history(
    State(state): State<AppState>,
    Query(q): Query<TireHistoryQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let days = q.days.unwrap_or(30).clamp(1, 365);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now - (days as i64) * 86_400;

    let store = state.drives.store.clone();
    let body = TIRE_HISTORY_CACHE
        .get(days, move || {
            // Build JSON once; cache hits clone only the `Arc`.
            let rows: Vec<(i64, Option<f32>, Option<f32>, Option<f32>, Option<f32>)> = store
                .with_read_conn(|conn| {
                    let mut stmt = conn.prepare(
                        "SELECT ts, tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi \
                         FROM telemetry_samples \
                         WHERE ts >= ?1 \
                           AND (tire_fl_psi IS NOT NULL OR tire_fr_psi IS NOT NULL \
                                OR tire_rl_psi IS NOT NULL OR tire_rr_psi IS NOT NULL) \
                         ORDER BY ts ASC",
                    )?;
                    let mapped = stmt.query_map(rusqlite::params![cutoff], |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, Option<f32>>(1)?,
                            r.get::<_, Option<f32>>(2)?,
                            r.get::<_, Option<f32>>(3)?,
                            r.get::<_, Option<f32>>(4)?,
                        ))
                    })?;
                    let mut out = Vec::new();
                    for row in mapped {
                        out.push(row?);
                    }
                    Ok::<_, anyhow::Error>(out)
                })
                // Preserve stale cache data when refresh fails.
                .ok()?;

            let stride = (rows.len() / TIRE_HISTORY_MAX_POINTS).max(1);
            let mut points = Vec::with_capacity(rows.len().min(TIRE_HISTORY_MAX_POINTS) + 1);
            for (i, (ts, fl, fr, rl, rr)) in rows.iter().enumerate() {
                // Always retain the newest sample.
                if i % stride != 0 && i + 1 != rows.len() {
                    continue;
                }
                let mut obj = serde_json::Map::new();
                obj.insert("ts".to_string(), serde_json::json!(ts * 1000));
                for (name, v) in [("fl", fl), ("fr", fr), ("rl", rl), ("rr", rr)] {
                    if let Some(v) = v {
                        obj.insert(name.to_string(), serde_json::json!(v));
                    }
                }
                points.push(serde_json::Value::Object(obj));
            }

            let doc = serde_json::json!({ "points": points, "days": days });
            Some(std::sync::Arc::new(doc.to_string()))
        })
        .await;

    match body {
        Some(json) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json.as_str().to_owned(),
        )
            .into_response(),
        None => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "tire-history query failed",
        )
        .into_response(),
    }
}
