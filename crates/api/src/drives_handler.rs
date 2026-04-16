use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;

use crate::router::AppState;
use sentryusb_drives::{DriveStore, grouper};

/// Drive-specific state.
#[derive(Clone)]
pub struct DriveState {
    pub store: Arc<DriveStore>,
    pub processor: Arc<sentryusb_drives::processor::Processor>,
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
    match state.drives.store.get_all_tags() {
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

/// POST /api/drives/process — start processing new clips
pub async fn process_files(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let processor = state.drives.processor.clone();
    tokio::spawn(async move {
        if let Err(e) = processor.process_new().await {
            tracing::warn!("drive processing error: {}", e);
        }
    });
    crate::json_ok()
}

/// POST /api/drives/reprocess — reprocess all clips
pub async fn reprocess_all(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let processor = state.drives.processor.clone();
    tokio::spawn(async move {
        if let Err(e) = processor.reprocess_all().await {
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
    let processed_count = state.drives.store.processed_count().unwrap_or(0);
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
    match sentryusb_drives::json_compat::export_json(&state.drives.store, tmp) {
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

/// POST /api/drives/data/upload — upload drive data JSON
pub async fn upload_data(
    State(state): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    let tmp = "/tmp/drive-data-upload.json";
    if let Err(e) = std::fs::write(tmp, &body) {
        return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string());
    }
    match sentryusb_drives::json_compat::import_json(tmp, &state.drives.store) {
        Ok(count) => (StatusCode::OK, Json(serde_json::json!({"imported": count}))),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// PUT /api/drives/{id}/tags — set tags for a drive
pub async fn set_drive_tags(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    match state.drives.store.set_tags(&id, &body.tags) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[derive(Deserialize)]
pub struct SetTagsRequest {
    pub tags: Vec<String>,
}
