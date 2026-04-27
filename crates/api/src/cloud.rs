//! Local API surface for the SentryCloud upload pipeline.
//!
//! All routes go behind the daemon's existing `auth_middleware` (covers
//! every `/api/*` except `/api/auth/*` and `/api/status` exempts). The
//! production UI never calls `POST /api/cloud/upload-now` — uploads are
//! triggered automatically at the tail of the archive lifecycle by the
//! `Notify` wired into `Processor`. The endpoint exists for dev/debug.

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

/// Shared cloud handler state. Held inside `AppState::cloud`; handlers
/// extract `State<AppState>` and pull `state.cloud.uploader` from it.
#[derive(Clone)]
pub struct CloudHandlerState {
    pub uploader: Arc<CloudUploader>,
}

use crate::router::AppState;

/// `GET /api/cloud/status` — paired/unpaired + latest counters.
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

/// `POST /api/cloud/pair/begin` — kick off pairing with a 6-digit code.
/// Spawns the pairing task and returns immediately; the UI polls
/// `GET /api/cloud/status` for progress (the pairingState/pairingError
/// fields update as the background task progresses).
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

    // Refuse if already paired (force-flag re-pair would belong here in
    // a future iteration — for now, the user must unpair first).
    let snap = state.cloud.uploader.status().await;
    if snap.paired {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "error": "already paired; unpair first" })),
        )
            .into_response();
    }

    // Spawn the pairing task. We don't await it — the UI watches state.
    let handle = state.cloud.uploader.clone();
    let code = body.code.clone();
    tokio::spawn(async move {
        if let Err(e) = handle.pair_begin(&code).await {
            warn!("cloud pair begin failed: {}", e);
        }
    });
    (StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response()
}

/// `POST /api/cloud/pair/cancel` — abort an in-flight pairing.
pub async fn pair_cancel(State(state): State<AppState>) -> impl IntoResponse {
    state.cloud.uploader.pair_cancel().await;
    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// `POST /api/cloud/unpair` — wipe local credentials + transition unpaired.
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

/// `POST /api/cloud/upload-now` — dev-only force-sweep. Idempotent.
pub async fn upload_now(State(state): State<AppState>) -> impl IntoResponse {
    state.cloud.uploader.nudge();
    (StatusCode::ACCEPTED, Json(json!({ "ok": true })))
}
