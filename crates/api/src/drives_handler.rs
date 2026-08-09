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

/// Path must match what `awake_stop` expects so existing deployments
/// keep working unchanged.
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
    /// Set while an external drive-data import (JSON upload) is running;
    /// blocks processing and reprocessing until the import completes.
    pub importing: Arc<AtomicBool>,
}

/// True if archiveloop is currently archiving: /tmp/archive_status.json
/// present, mtime within 120s, phase == "archiving".
pub fn is_archiving() -> bool {
    match read_archive_status() {
        Some(v) => v.get("phase").and_then(|p| p.as_str()) == Some("archiving"),
        None => false,
    }
}

/// Read and parse /tmp/archive_status.json, returning None if absent,
/// stale, or invalid. Removes the file if its mtime is older than 120s.
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

/// Sources envsetup.sh + exports the shared PID file so awake_start/awake_stop
/// coordinate with archiveloop's own keep-awake management.
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
/// so nudge logs can show time remaining.
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

/// Serve an already-serialized JSON string as the response body without
/// the parse-into-`Value`-then-reserialize round-trip. The cache already
/// holds exactly the bytes the client should receive, so hand them back
/// directly with the JSON content-type. (Round-tripping through
/// `serde_json::Value` also alphabetized object keys; serving the cache
/// verbatim preserves the original field order.)
fn cached_json_response(body: String) -> axum::response::Response {
    use axum::http::header;
    use axum::response::IntoResponse;
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// GET /api/drives — list all drives (summaries only).
///
/// Returns a pre-computed JSON string stored in the `meta` table. The
/// cache is built once at startup (or after any mutation) and served
/// directly on every subsequent request — no grouper work, no BLOB
/// decoding, no ORDER-BY sorter allocation.
pub async fn list_drives(State(state): State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    // spawn_blocking: a cache miss runs the grouper over every route
    // (~400ms on a Pi 5 for 7k routes) — keep that off the async workers.
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
    // The detail page runs the grouper over every route summary AND decodes
    // the drive's full point BLOBs to build per-point stats — easily
    // hundreds of ms on a big history. Run it on the blocking pool so it
    // can't stall the async reactor (which drops the WebSocket heartbeat
    // and shows "Reconnecting to SentryUSB…").
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

    // Resolve the drive id to (idx, files) AND grab the canonical
    // DriveSummary the list view emits, in a single summary pass. We
    // overlay the summary's headline aggregates onto the full Drive so
    // the detail page can't disagree with the list on FSD %, distance,
    // etc. — the two are computed via different algorithms (point walk
    // vs per-clip aggregate sum) and drift ~0.1–0.5 % otherwise.
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
            // target_start scopes the full-BLOB rebuild to the requested
            // drive's park-split segment — the fetched parents are whole
            // clips, and a drive sharing a clip with its neighbor (the
            // fused-summon shape) must not draw the neighbor's points on
            // the detail map. The mini-map path (route_overviews) already
            // splits; the two must agree.
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
                // Overlay the headline numbers from the canonical
                // aggregate path. Per-point data (points, fsd_states,
                // fsd_events) stays from build_drive_stats — those
                // need the full BLOB walk.
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
                // Summon is only decided on the summary path — it needs
                // the park-split segment frame bounds that the full-BLOB
                // rebuild above doesn't carry.
                drive.summon = s.summon;
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

/// GET /api/drives/routes — overview routes for map.
///
/// Optional `max_points` query parameter (clamped to 2..=2000, defaults
/// to 500) controls how aggressively each drive's polyline is
/// downsampled. The Drives-list mini-map thumbnails request a small
/// value (e.g. 20) to keep the wire payload manageable when fetching
/// routes for hundreds of drives at once.
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
/// no SQLite contention.
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
/// IsArchiving guard.
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

/// POST /api/drives/check-summon — targeted summon evidence re-read.
///
/// Port of Sentry-Drive's `check-summon` repair: re-extracts only the
/// clips that could hide a summon drive but lack current flag evidence
/// (see `Processor::check_summon`), so old libraries gain summon flags
/// without a full reprocess. Runs async like `reprocess`; progress is
/// visible on `GET /api/drives/status`, and the outcome is broadcast on
/// the WebSocket hub as `summon_check` and logged.
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

/// GET /api/drives/stats — aggregate stats
/// Served from the pre-computed cache; no grouper work or BLOB decoding per request.
/// `processed_count` is injected live from the atomic counter (it changes on
/// every processed clip, independent of the route/tags cache invalidation key).
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
/// Regenerate `/backingfiles/drive-data.json` from the live SQLite store so
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

/// GET /api/drives/data/import-history — read-only handler that returns the
/// last 20 import diagnostics records persisted in the `meta` table. Each
/// entry is `{ timestamp, stats, diagnostics }` so an operator can see why
/// drives may have gone missing without scraping logs. Empty array if no
/// imports have run yet.
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

/// POST /api/drives/bulk-delete — remove the listed drives from the DB.
///
/// Body: `{ "ids": ["<numeric index>" or "<start_time>", ...] }`.
/// Resolves each id through the grouper to the clip files that make up
/// the drive, deduplicates across ids (a parent clip can never be
/// shared between two drives, but the dedupe is a cheap guard against
/// future grouper changes), then deletes the matching rows from
/// `routes`, `processed_files`, and `drive_tags` in one transaction.
///
/// Guards against the two jobs that actually write to these tables:
/// the clip processor (`processor.is_running()`) and a JSON import
/// (`importing`). We deliberately do NOT block on `is_archiving()`:
/// the archive script copies clip files between disks and doesn't
/// touch the DB during the archiving phase; the post-archive
/// reprocess that DOES write goes through `POST /api/drives/process`,
/// which would trip the processor guard at that point. Blocking on
/// archive here is what `delete_all_drives` does, but for a
/// targeted per-drive delete it's just user-hostile latency.
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

    // The grouper pass + delete transaction is blocking + CPU; keep it off
    // the reactor (the guards above are cheap atomic loads and stay async).
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
    // Resolve every id to (drive_key, file_list) in one summary pass —
    // `with_route_summaries` rebuilds the grouper exactly once, not once
    // per id.
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

/// PUT /api/drives/{id}/tags — set tags for a drive.
///
/// `id` from the URL is either the numeric grouper index (what the
/// drives-list response stamps onto `DriveSummary.id`) or the
/// `%Y-%m-%dT%H:%M:%S` start_time string (the same form
/// `/api/drives/{id}` accepts). Either way, the grouper joins tags
/// onto drives strictly by start_time string, so we MUST resolve the
/// URL id to that canonical key before writing — otherwise the row
/// lands under a key like `"3"` that the list endpoint never reads
/// back, and the tag silently disappears from the UI even though the
/// PUT returned 200.
pub async fn set_drive_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Resolving the id runs the grouper over every summary — off the reactor.
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

/// GET /api/drives/{id}/temperature-series
///
/// Time-series of interior + exterior cabin temperature across the
/// drive, sourced from `telemetry_samples` (BLE sampler at 60s cadence)
/// for the union of clip windows that make up the drive. The frontend
/// renders this as a two-line chart on the DriveDetail Climate section,
/// one line per temperature source on a shared time axis.
///
/// Response shape:
/// ```json
/// {
///   "points": [
///     { "ts": 1716471234000, "interiorC": 22.3, "exteriorC": 14.1 },
///     ...
///   ]
/// }
/// ```
/// `interiorC` / `exteriorC` are omitted when the underlying sample had
/// NULL in that column — the frontend treats gaps as "no data" rather
/// than dropping to zero.
pub async fn temperature_series(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Find files + derive window + query — all touch the connection
    // mutex, so run the whole chain on the blocking pool.
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

            // Derive the drive's wall-clock window from the min/max of its
            // clip windows. Files without parseable timestamps (event
            // folders, manual imports) are silently skipped — they wouldn't
            // contribute telemetry rows anyway since the sampler keys by
            // clock time.
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
                    // Ship ts in milliseconds — JS Date and recharts work in
                    // ms, saves the frontend a multiply per row.
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

/// GET /api/drives/{id}/battery-series
///
/// Time-series of battery percent across the drive, sourced from
/// the same `telemetry_samples` table the temperature series uses.
/// The BLE sampler polls `state charge` every 60s in Active mode,
/// so a typical 30-min drive has ~30 samples — enough to draw a
/// smooth line on a playback chart without needing to interpolate
/// between start/end the way the older code did.
///
/// Response shape mirrors `temperature_series`:
/// ```json
/// {
///   "points": [
///     { "ts": 1716471234000, "batteryPct": 73.0 },
///     ...
///   ]
/// }
/// ```
/// `batteryPct` is omitted only when a row in the window has NULL
/// in that column — which is fine for the chart, gaps just don't
/// draw.
pub async fn battery_series(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Find files + derive window + query — all touch the connection
    // mutex, so run the whole chain on the blocking pool.
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

            // Same window-derivation as temperature_series — min/max of the
            // clip-file timestamps that make up this drive.
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
    /// Window size in days. Clamped to [1, 365] to keep the down-sample
    /// query bounded. Defaults to 30 to match the dashboard card spec.
    #[serde(default)]
    pub days: Option<u32>,
}

/// GET /api/telemetry/tire-history
///
/// Per-tire pressure samples for the last `days` days (default 30).
/// Rendered as the Dashboard's TirePressureCard.
///
/// No down-sampling: TPMS sensors only broadcast while the car is
/// driving, so even with the 5-minute sampler cadence
/// (`TIRES_INTERVAL` in the telemetry crate) a typical 30-day window
/// stays well under a thousand points — small enough to ship raw and
/// render smoothly in recharts.
///
/// Response shape:
/// ```json
/// {
///   "points": [
///     { "ts": 1716471234000, "fl": 41.7, "fr": 41.5, "rl": 42.4, "rr": 41.3 },
///     ...
///   ],
///   "days": 30
/// }
/// ```
///
/// A 30-day pressure chart doesn't change meaningfully inside a minute,
/// but the scan measured 5.2s mid-archive and held one of the two read
/// connections for that whole time — long enough to queue every other
/// telemetry poll behind it. Cached per `days` value.
static TIRE_HISTORY_CACHE: crate::ttl_cache::StaleWhileRevalidate<u32, Option<Vec<serde_json::Value>>> =
    crate::ttl_cache::StaleWhileRevalidate::new(std::time::Duration::from_secs(60));

pub async fn tire_history(
    State(state): State<AppState>,
    Query(q): Query<TireHistoryQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let days = q.days.unwrap_or(30).clamp(1, 365);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = now - (days as i64) * 86_400;

    let store = state.drives.store.clone();
    let result = TIRE_HISTORY_CACHE
        .get(days, move || {
        store.with_read_conn(|conn| {
            let mut stmt = conn.prepare(
                "SELECT ts, tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi \
                 FROM telemetry_samples \
                 WHERE ts >= ?1 \
                   AND (tire_fl_psi IS NOT NULL OR tire_fr_psi IS NOT NULL \
                        OR tire_rl_psi IS NOT NULL OR tire_rr_psi IS NOT NULL) \
                 ORDER BY ts ASC",
            )?;
            let rows = stmt.query_map(rusqlite::params![cutoff], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<f64>>(1)?,
                    r.get::<_, Option<f64>>(2)?,
                    r.get::<_, Option<f64>>(3)?,
                    r.get::<_, Option<f64>>(4)?,
                ))
            })?;
            let mut out: Vec<serde_json::Value> = Vec::new();
            for row in rows {
                let (ts, fl, fr, rl, rr) = row?;
                let mut obj = serde_json::Map::new();
                obj.insert("ts".to_string(), serde_json::json!(ts * 1000));
                if let Some(v) = fl {
                    obj.insert("fl".to_string(), serde_json::json!(v));
                }
                if let Some(v) = fr {
                    obj.insert("fr".to_string(), serde_json::json!(v));
                }
                if let Some(v) = rl {
                    obj.insert("rl".to_string(), serde_json::json!(v));
                }
                if let Some(v) = rr {
                    obj.insert("rr".to_string(), serde_json::json!(v));
                }
                out.push(serde_json::Value::Object(obj));
            }
            Ok::<_, anyhow::Error>(out)
        })
        .ok()
        })
        .await;

    match result {
        Some(points) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "points": points,
                "days": days,
            })),
        ),
        None => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "tire-history query failed",
        ),
    }
}
