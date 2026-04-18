//! Community wraps and lock chimes proxy to backend API.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use std::collections::HashMap;

use crate::router::AppState;

const COMMUNITY_API: &str = "https://api.sentry-six.com";

async fn proxy_get(path: &str) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}{}", COMMUNITY_API, path);
    match reqwest::get(&url).await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

/// Library-style proxy: forwards the client's query string and, on any
/// upstream failure (DNS/connect/parse), returns an empty library shape so
/// the UI renders an empty state instead of a 502 error banner.
async fn proxy_library(
    path: &str,
    params: &HashMap<String, String>,
    key: &str,
) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}{}", COMMUNITY_API, path);
    let client = reqwest::Client::new();
    match client.get(&url).query(params).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(_) => (StatusCode::OK, Json(serde_json::json!({ key: [], "total": 0 }))),
        },
        Err(_) => (StatusCode::OK, Json(serde_json::json!({ key: [], "total": 0 }))),
    }
}

async fn proxy_post(path: &str, body: &str) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}{}", COMMUNITY_API, path);
    let client = reqwest::Client::new();
    match client.post(&url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send().await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

// Community lock chimes
pub async fn lock_chime_library(
    State(_s): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    proxy_library("/lockchime/community/library", &params, "chimes").await
}

pub async fn lock_chime_stream(State(_s): State<AppState>, Path(code): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_get(&format!("/lockchime/community/stream/{}", code)).await
}

pub async fn lock_chime_upload(State(_s): State<AppState>, body: axum::body::Bytes) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}/lockchime/community/upload", COMMUNITY_API);
    let client = reqwest::Client::new();
    match client.post(&url).body(body.to_vec()).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

pub async fn lock_chime_download(State(_s): State<AppState>, Path(code): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/lockchime/community/download/{}", code), "{}").await
}

pub async fn lock_chime_admin_validate(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post("/lockchime/community/admin/validate", &body).await
}

pub async fn lock_chime_admin_edit(State(_s): State<AppState>, Path(code): Path<String>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/lockchime/community/admin/edit/{}", code), &body).await
}

pub async fn lock_chime_admin_delete(State(_s): State<AppState>, Path(code): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}/lockchime/community/admin/delete/{}", COMMUNITY_API, code);
    let client = reqwest::Client::new();
    match client.delete(&url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

// Community wraps
pub async fn wraps_library(
    State(_s): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    proxy_library("/wraps/library", &params, "wraps").await
}

pub async fn wraps_thumbnail(State(_s): State<AppState>, Path(code): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_get(&format!("/wraps/thumbnail/{}", code)).await
}

pub async fn wraps_preview(State(_s): State<AppState>, Path(code): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_get(&format!("/wraps/preview/{}", code)).await
}

pub async fn wraps_upload(State(_s): State<AppState>, body: axum::body::Bytes) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}/wraps/upload", COMMUNITY_API);
    let client = reqwest::Client::new();
    match client.post(&url).body(body.to_vec()).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

pub async fn wraps_download(State(_s): State<AppState>, Path(code): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/wraps/download/{}", code), "{}").await
}

pub async fn wraps_admin_validate(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post("/wraps/admin/validate", &body).await
}

pub async fn wraps_admin_edit(State(_s): State<AppState>, Path(code): Path<String>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}/wraps/admin/edit/{}", COMMUNITY_API, code);
    let client = reqwest::Client::new();
    match client.put(&url)
        .header("Content-Type", "application/json")
        .body(body)
        .send().await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

pub async fn wraps_admin_delete(State(_s): State<AppState>, Path(code): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}/wraps/admin/delete/{}", COMMUNITY_API, code);
    let client = reqwest::Client::new();
    match client.delete(&url).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}
