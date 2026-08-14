//! Router served when the drive DB cannot be opened on a real device.
//!
//! Database-dependent endpoints return `503 database_unavailable`; an
//! authenticated allowlist retains status, logs, repair, and system controls.

use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};

use crate::auth::AuthState;

#[derive(Clone)]
pub struct DegradedState {
    pub auth: AuthState,
    pub hub: sentryusb_ws::Hub,
    /// Database-open failure for the banner and journal.
    pub reason: std::sync::Arc<String>,
}

impl FromRef<DegradedState> for AuthState {
    fn from_ref(s: &DegradedState) -> Self {
        s.auth.clone()
    }
}

impl FromRef<DegradedState> for sentryusb_ws::Hub {
    fn from_ref(s: &DegradedState) -> Self {
        s.hub.clone()
    }
}

/// GET /api/status — return 200 with a degraded marker so the SPA can
/// distinguish database failure from disconnection.
async fn degraded_status(State(s): State<DegradedState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "degraded": "database_unavailable",
        "degraded_reason": *s.reason,
    }))
}

/// Catch-all for every API route outside the recovery allowlist.
async fn unavailable(State(s): State<DegradedState>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "database_unavailable",
            "reason": *s.reason,
        })),
    )
}

pub fn build_degraded_router(state: DegradedState) -> Router {
    Router::new()
        .route("/api/status", get(degraded_status))
        // Initial routing needs setup state before rendering the banner.
        .route("/api/setup/status", get(crate::setup::get_setup_status))
        // Keep the recovery allowlist authenticated.
        .route("/api/auth/login", post(crate::auth::handle_login))
        .route("/api/auth/logout", post(crate::auth::handle_logout))
        .route("/api/auth/check", get(crate::auth::handle_auth_check))
        .route("/api/system/version", get(crate::update::get_version))
        .route("/api/system/reboot", post(crate::system::reboot))
        .route("/api/system/shutdown", post(crate::system::shutdown))
        .route("/api/logs/{name}", get(crate::logs::get_log_tail))
        .route("/api/logs/{name}/page", get(crate::logs::get_log_page))
        .route("/api/storage/health", get(crate::storage_repair::storage_health))
        .route("/api/storage/repair", post(crate::storage_repair::storage_repair))
        .route("/api/ws", get(crate::router::ws_handler))
        .route("/api/{*rest}", axum::routing::any(unavailable))
        .with_state(state)
}
