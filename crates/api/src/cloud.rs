

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;
use tracing::warn;

use sentryusb_cloud_uploader::CloudUploader;

#[derive(Clone)]
pub struct CloudHandlerState {
    pub uploader: Arc<CloudUploader>,
}

use crate::router::AppState;

pub async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.cloud.uploader.status().await;
    Json(json!({
        "paired": snap.paired,
        "userId": snap.user_id,
        "piId": snap.pi_id,
        "pairedAt": snap.paired_at,
        "lastUploadAt": snap.last_upload_at,
        "lastUploadError": snap.last_upload_error,
        "pendingRouteCount": snap.pending_route_count,
        "totalUploadedRouteCount": snap.total_uploaded_route_count,
        "dekRotationGeneration": snap.dek_rotation_generation,
        "cloudBaseUrl": snap.cloud_base_url,
        "pairingState": snap.pairing_state,
        "pairingError": snap.pairing_error,
    }))
}

#[derive(Deserialize)]
pub struct PairBeginBody {
    pub code: String,
}

pub async fn pair_begin(
    State(state): State<AppState>,
    Json(body): Json<PairBeginBody>,
) -> impl IntoResponse {
    if !body.code.chars().all(|c| c.is_ascii_digit()) || body.code.len() != 6 {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "code must be 6 digits" })),
        )
            .into_response();
    }

    let snap = state.cloud.uploader.status().await;
    if snap.paired {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "already paired; unpair first" })),
        )
            .into_response();
    }

    let handle = state.cloud.uploader.clone();
    let code = body.code.clone();
    tokio::spawn(async move {
        if let Err(e) = handle.pair_begin(&code).await {
            warn!("cloud pair begin failed: {}", e);
        }
    });
    (StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response()
}

pub async fn pair_cancel(State(state): State<AppState>) -> impl IntoResponse {
    state.cloud.uploader.pair_cancel().await;
    (StatusCode::OK, Json(json!({ "ok": true })))
}

pub async fn unpair(State(state): State<AppState>) -> impl IntoResponse {
    match state.cloud.uploader.unpair().await {
        Ok(_) => (StatusCode::OK, Json(json!({ "ok": true }))).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub async fn upload_now(State(state): State<AppState>) -> impl IntoResponse {
    state.cloud.uploader.nudge();
    (StatusCode::ACCEPTED, Json(json!({ "ok": true })))
}

#[derive(Deserialize, Default)]
pub struct QueueQuery {

    pub limit: Option<i64>,
}

pub async fn get_queue(
    State(state): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<QueueQuery>,
) -> impl IntoResponse {
    let limit = q.limit.unwrap_or(100).clamp(1, 200);
    match state.cloud.uploader.pending_queue(limit) {
        Ok(entries) => {

            let pending = state.cloud.uploader.status().await.pending_route_count;
            Json(json!({
                "entries": entries,
                "pending": pending,
                "limit": limit,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}
