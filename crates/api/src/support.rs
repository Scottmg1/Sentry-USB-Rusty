//! Support ticket proxy to backend API.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use crate::router::AppState;

const SUPPORT_API: &str = "https://api.sentryusb.com";

async fn proxy_get(path: &str) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}{}", SUPPORT_API, path);
    match reqwest::get(&url).await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

async fn proxy_post(path: &str, body: &str) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}{}", SUPPORT_API, path);
    let client = reqwest::Client::new();
    match client.post(&url)
        .header("Content-Type", "application/json")
        .body(body.to_string())
        .send()
        .await
    {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

pub async fn check_available(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_get("/support/check").await
}

pub async fn create_ticket(State(_s): State<AppState>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post("/support/ticket", &body).await
}

pub async fn send_message(State(_s): State<AppState>, Path(id): Path<String>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/support/ticket/{}/message", id), &body).await
}

pub async fn upload_media(State(_s): State<AppState>, Path(id): Path<String>, body: axum::body::Bytes) -> (StatusCode, Json<serde_json::Value>) {
    let url = format!("{}/support/ticket/{}/media", SUPPORT_API, id);
    let client = reqwest::Client::new();
    match client.post(&url).body(body.to_vec()).send().await {
        Ok(resp) => match resp.json::<serde_json::Value>().await {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
        },
        Err(e) => crate::json_error(StatusCode::BAD_GATEWAY, &e.to_string()),
    }
}

pub async fn fetch_messages(State(_s): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_get(&format!("/support/ticket/{}/messages", id)).await
}

pub async fn close_ticket(State(_s): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/support/ticket/{}/close", id), "{}").await
}

pub async fn mark_read(State(_s): State<AppState>, Path(id): Path<String>) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/support/ticket/{}/mark-read", id), "{}").await
}

pub async fn register_device(State(_s): State<AppState>, Path(id): Path<String>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/support/ticket/{}/register-device", id), &body).await
}

pub async fn unregister_device(State(_s): State<AppState>, Path(id): Path<String>, body: String) -> (StatusCode, Json<serde_json::Value>) {
    proxy_post(&format!("/support/ticket/{}/unregister-device", id), &body).await
}
