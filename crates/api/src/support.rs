//! Support-ticket proxy to backend API.
//!
//! Mirrors `server/api/support.go`:
//! - Forwards `X-Auth-Token` (per-ticket session token) and `X-Passcode`
//!   (fingerprint-based admin access) from the incoming request.
//! - Re-serializes JSON bodies to normalize broken Unicode escapes (\usb etc.
//!   appear in diagnostics text and choke strict JSON parsers).
//! - Preserves the upstream status code instead of flattening to 200/502.

use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::router::AppState;

const SUPPORT_API: &str = "https://api.sentry-six.com";

fn forward_headers(src: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    for name in ["x-auth-token", "x-passcode"] {
        if let Some(val) = src.get(name) {
            if let Ok(parsed) = reqwest::header::HeaderValue::from_bytes(val.as_bytes()) {
                if let Ok(hname) =
                    reqwest::header::HeaderName::from_bytes(name.as_bytes())
                {
                    h.insert(hname, parsed);
                }
            }
        }
    }
    h
}

/// Re-parse + re-serialize JSON to normalize encoding issues. If parse fails,
/// return the original bytes unchanged (upstream may accept non-strict JSON).
fn sanitize_json(raw: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(raw) {
        Ok(v) => serde_json::to_vec(&v).unwrap_or_else(|_| raw.to_vec()),
        Err(_) => raw.to_vec(),
    }
}

fn bad_gateway(msg: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

async fn proxy_request(
    method: reqwest::Method,
    path: &str,
    body: Option<Vec<u8>>,
    headers: reqwest::header::HeaderMap,
    timeout: Duration,
) -> Response {
    let url = format!("{}{}", SUPPORT_API, path);
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut req = client.request(method, &url).headers(headers);
    if let Some(b) = body {
        req = req.header("Content-Type", "application/json").body(b);
    }
    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("Support server unreachable: {}", e)),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut r = Response::new(axum::body::Body::from(bytes));
    *r.status_mut() = status;
    r.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    r
}

const JSON_TIMEOUT: Duration = Duration::from_secs(30);
const MEDIA_TIMEOUT: Duration = Duration::from_secs(600);

pub async fn check_available(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("{}/health", SUPPORT_API);
    match client.get(&url).send().await {
        Ok(r) => (
            StatusCode::OK,
            Json(serde_json::json!({"available": r.status().is_success()})),
        ),
        Err(_) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "available": false,
                "error": "Cannot reach support server. Check internet connection.",
            })),
        ),
    }
}

pub async fn create_ticket(
    State(_s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let clean = sanitize_json(&body);
    proxy_request(
        reqwest::Method::POST,
        "/chat/ticket",
        Some(clean),
        forward_headers(&headers),
        JSON_TIMEOUT,
    )
    .await
}

pub async fn send_message(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let clean = sanitize_json(&body);
    proxy_request(
        reqwest::Method::POST,
        &format!("/chat/ticket/{}/message", id),
        Some(clean),
        forward_headers(&headers),
        JSON_TIMEOUT,
    )
    .await
}

pub async fn upload_media(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let url = format!("{}/chat/ticket/{}/media", SUPPORT_API, id);
    let client = match reqwest::Client::builder()
        .timeout(MEDIA_TIMEOUT)
        .build()
    {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut req = client.post(&url);
    if let Some(ct) = headers.get("content-type") {
        if let Ok(v) = reqwest::header::HeaderValue::from_bytes(ct.as_bytes()) {
            req = req.header("Content-Type", v);
        }
    }
    req = req.headers(forward_headers(&headers));
    let resp = match req.body(body.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("Support server unreachable: {}", e)),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut r = Response::new(axum::body::Body::from(bytes));
    *r.status_mut() = status;
    r.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    r
}

#[derive(serde::Deserialize)]
pub struct FetchQuery {
    since: Option<String>,
}

pub async fn fetch_messages(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<FetchQuery>,
    headers: HeaderMap,
) -> Response {
    let mut path = format!("/chat/ticket/{}/messages", id);
    if let Some(since) = q.since {
        if !since.is_empty() {
            path.push_str(&format!("?since={}", since));
        }
    }
    proxy_request(
        reqwest::Method::GET,
        &path,
        None,
        forward_headers(&headers),
        Duration::from_secs(15),
    )
    .await
}

pub async fn close_ticket(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = if body.is_empty() {
        b"{}".to_vec()
    } else {
        body.to_vec()
    };
    proxy_request(
        reqwest::Method::POST,
        &format!("/chat/ticket/{}/close", id),
        Some(body),
        forward_headers(&headers),
        Duration::from_secs(15),
    )
    .await
}

pub async fn mark_read(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    proxy_request(
        reqwest::Method::POST,
        &format!("/chat/ticket/{}/mark-read", id),
        Some(b"{}".to_vec()),
        forward_headers(&headers),
        Duration::from_secs(15),
    )
    .await
}

pub async fn register_device(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_request(
        reqwest::Method::POST,
        &format!("/chat/ticket/{}/register-device", id),
        Some(body.to_vec()),
        forward_headers(&headers),
        Duration::from_secs(15),
    )
    .await
}

pub async fn unregister_device(
    State(_s): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    proxy_request(
        reqwest::Method::POST,
        &format!("/chat/ticket/{}/unregister-device", id),
        Some(body.to_vec()),
        forward_headers(&headers),
        Duration::from_secs(15),
    )
    .await
}
