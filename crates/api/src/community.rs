//! Community wraps and lock chimes proxy to backend API.
//!
//! Mirrors `server/api/community_wraps.go`:
//! - Validates wrap/chime codes against `^[A-Za-z0-9]{3,10}$` before proxying.
//! - Forwards `X-Passcode` (admin fingerprint access) on every route that
//!   accepts it — library, upload, download, admin, plus the chime variants.
//! - Injects `X-Fingerprint` on upload/download so the backend can rate-limit
//!   and tie uploads to a device.
//! - Returns binary PNG for thumbnail/preview with `Cache-Control: max-age=3600`.
//! - Preserves upstream status codes rather than collapsing to 200.

use std::collections::HashMap;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use std::sync::OnceLock;

use crate::router::AppState;

const COMMUNITY_API: &str = "https://api.sentry-six.com";

fn valid_code(code: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9]{3,10}$").unwrap())
        .is_match(code)
}

fn forward_headers(src: &HeaderMap, inject_fingerprint: bool) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(v) = src.get("x-passcode") {
        if let Ok(val) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) {
            h.insert("x-passcode", val);
        }
    }
    if inject_fingerprint {
        let fp = crate::update::get_fingerprint();
        if !fp.is_empty() {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(fp) {
                h.insert("x-fingerprint", val);
            }
        }
    }
    h
}

fn bad_gateway(msg: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

async fn proxy_json(
    method: reqwest::Method,
    path: &str,
    body: Option<Vec<u8>>,
    headers: reqwest::header::HeaderMap,
    timeout: Duration,
) -> Response {
    let url = format!("{}{}", COMMUNITY_API, path);
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
        Err(e) => return bad_gateway(&format!("Community service unreachable: {}", e)),
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

async fn proxy_library(
    path: &str,
    params: &HashMap<String, String>,
    key: &str,
    headers: HeaderMap,
) -> Response {
    let url = format!("{}{}", COMMUNITY_API, path);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(15)).build() {
        Ok(c) => c,
        Err(_) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({ key: [], "total": 0 })),
            )
                .into_response();
        }
    };
    let resp = match client
        .get(&url)
        .headers(forward_headers(&headers, false))
        .query(params)
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({ key: [], "total": 0 })),
            )
                .into_response();
        }
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::OK,
                Json(serde_json::json!({ key: [], "total": 0 })),
            )
                .into_response();
        }
    };
    let mut r = Response::new(axum::body::Body::from(bytes));
    *r.status_mut() = status;
    r.headers_mut()
        .insert("content-type", "application/json".parse().unwrap());
    r
}

async fn proxy_image(path: &str) -> Response {
    let url = format!("{}{}", COMMUNITY_API, path);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(15)).build() {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return bad_gateway("Failed to fetch image"),
    };
    let upstream_status = resp.status();
    let status = StatusCode::from_u16(upstream_status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut r = Response::new(axum::body::Body::from(bytes));
    *r.status_mut() = status;
    if upstream_status.is_success() {
        r.headers_mut()
            .insert("content-type", "image/png".parse().unwrap());
        r.headers_mut()
            .insert("cache-control", "public, max-age=3600".parse().unwrap());
    } else {
        r.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
    }
    r
}

fn invalid_code() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "Invalid code"})),
    )
        .into_response()
}

// --- Community lock chimes ---

pub async fn lock_chime_library(
    State(_s): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    proxy_library("/lockchime/library", &params, "chimes", headers).await
}

pub async fn lock_chime_stream(
    State(_s): State<AppState>,
    Path(code): Path<String>,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }
    // Streams audio (WAV) — binary passthrough with appropriate cache headers.
    let url = format!("{}/lockchime/download/{}", COMMUNITY_API, code);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(_) => return bad_gateway("Failed to fetch chime"),
    };
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("audio/wav")
        .to_string();
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut r = Response::new(axum::body::Body::from(bytes));
    *r.status_mut() = status;
    r.headers_mut().insert(
        "content-type",
        ct.parse().unwrap_or_else(|_| "audio/wav".parse().unwrap()),
    );
    r
}

pub async fn lock_chime_upload(
    State(_s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let url = format!("{}/lockchime/upload", COMMUNITY_API);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut req = client.post(&url);
    if let Some(ct) = headers.get("content-type") {
        if let Ok(v) = reqwest::header::HeaderValue::from_bytes(ct.as_bytes()) {
            req = req.header("Content-Type", v);
        }
    }
    req = req.headers(forward_headers(&headers, true));
    let resp = match req.body(body.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("Community service unreachable: {}", e)),
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

/// POST /api/lockchime/community/download/{code} — fetch a community
/// chime from the support server (`GET /lockchime/download/{code}`,
/// note the upstream method is GET even though the local entry-point
/// is POST), validate it as a 5s-or-less mono 44.1k WAV under 1MB,
/// save it to /mutable/LockChime, and return JSON success.
///
/// Earlier ports of this just JSON-proxied the upstream call with a
/// POST body — the support server returned 404 because it only
/// accepts GET, and even if it had succeeded the bytes would have
/// been forwarded to the browser instead of saved on the Pi.
pub async fn lock_chime_download(
    State(_s): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }

    let url = format!("{}/lockchime/download/{}", COMMUNITY_API, code);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let resp = match client
        .get(&url)
        .headers(forward_headers(&headers, true))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("Community lock chime service unreachable: {}", e)),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        // Forward upstream error JSON verbatim.
        let bytes = resp.bytes().await.unwrap_or_default();
        let mut r = Response::new(axum::body::Body::from(bytes));
        *r.status_mut() = status;
        r.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        return r;
    }

    let sound_name = resp
        .headers()
        .get("x-sound-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or(&code)
        .to_string();
    let raw_bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&format!("Failed to download sound: {}", e)),
    };

    if raw_bytes.len() > crate::lock_chime::LOCK_CHIME_MAX_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Downloaded sound exceeds 1 MB size limit"})),
        )
            .into_response();
    }

    let normalized = match crate::lock_chime::ensure_mono_wav(&raw_bytes) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Downloaded file is not a valid WAV: {}", e)})),
            )
                .into_response();
        }
    };
    let duration = match crate::lock_chime::parse_wav_duration(&normalized) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": format!("Downloaded file is not a valid WAV: {}", e)})),
            )
                .into_response();
        }
    };
    if duration > crate::lock_chime::LOCK_CHIME_MAX_SECONDS {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Sound is {:.1} seconds — must be {:.0} seconds or less",
                    duration, crate::lock_chime::LOCK_CHIME_MAX_SECONDS)
            })),
        )
            .into_response();
    }

    let mut base_name = crate::lock_chime::sanitize_lock_chime_name(&sound_name);
    // Avoid collision with the active /mutable/LockChime.wav target.
    let stem_lower = std::path::Path::new(&base_name)
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if stem_lower == "lockchime" {
        base_name = format!("{}.wav", code);
    }

    let _ = std::fs::create_dir_all(crate::lock_chime::LOCK_CHIME_DIR);
    let (dest_path, final_name) = match crate::lock_chime::deduplicate_filename(
        crate::lock_chime::LOCK_CHIME_DIR, &base_name,
    ) {
        Some(t) => t,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "Too many duplicate filenames"})),
            )
                .into_response();
        }
    };

    if let Err(e) = std::fs::write(&dest_path, &normalized) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to save sound: {}", e)})),
        )
            .into_response();
    }
    // Default umask of 0022 gives 0644 on the freshly-written file,
    // which is what we want for chime files anyway.

    Json(serde_json::json!({
        "success": true,
        "filename": final_name,
        "path": dest_path.to_string_lossy(),
        "size": normalized.len(),
    }))
    .into_response()
}

pub async fn lock_chime_admin_validate(
    State(_s): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if headers.get("x-passcode").is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }
    proxy_json(
        reqwest::Method::POST,
        "/lockchime/admin/validate",
        None,
        forward_headers(&headers, false),
        Duration::from_secs(15),
    )
    .await
}

pub async fn lock_chime_admin_edit(
    State(_s): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }
    if headers.get("x-passcode").is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }
    proxy_json(
        reqwest::Method::PUT,
        &format!("/lockchime/admin/edit/{}", code),
        Some(body.to_vec()),
        forward_headers(&headers, false),
        Duration::from_secs(15),
    )
    .await
}

pub async fn lock_chime_admin_delete(
    State(_s): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }
    if headers.get("x-passcode").is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }
    proxy_json(
        reqwest::Method::DELETE,
        &format!("/lockchime/admin/delete/{}", code),
        None,
        forward_headers(&headers, false),
        Duration::from_secs(15),
    )
    .await
}

// --- Community wraps ---

pub async fn wraps_library(
    State(_s): State<AppState>,
    headers: HeaderMap,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    proxy_library("/wraps/library", &params, "wraps", headers).await
}

pub async fn wraps_thumbnail(
    State(_s): State<AppState>,
    Path(code): Path<String>,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }
    proxy_image(&format!("/wraps/thumbnail/{}", code)).await
}

pub async fn wraps_preview(
    State(_s): State<AppState>,
    Path(code): Path<String>,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }
    proxy_image(&format!("/wraps/preview/{}", code)).await
}

pub async fn wraps_upload(
    State(_s): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // Match Go's 2 MiB cap on multipart payload.
    const MAX_BYTES: usize = 2 * 1024 * 1024;
    if body.len() > MAX_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({"error": "upload too large"})),
        )
            .into_response();
    }

    let url = format!("{}/wraps/upload", COMMUNITY_API);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let mut req = client.post(&url);
    if let Some(ct) = headers.get("content-type") {
        if let Ok(v) = reqwest::header::HeaderValue::from_bytes(ct.as_bytes()) {
            req = req.header("Content-Type", v);
        }
    }
    req = req.headers(forward_headers(&headers, true));
    let resp = match req.body(body.to_vec()).send().await {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("Community service unreachable: {}", e)),
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

/// POST /api/wraps/download/{code} — fetch a community wrap PNG from
/// the support server and save it under /mutable/Wraps. Same shape as
/// `lock_chime_download`: the upstream call is a GET (the local
/// entry-point is POST to enable simple form posts), and the bytes
/// are saved on the Pi rather than streamed back to the browser.
///
/// Without this fix every "Download to Pi" click returned 404 because
/// the upstream server only accepts GET on /wraps/download/{code}.
pub async fn wraps_download(
    State(_s): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }

    const WRAPS_DIR: &str = "/mutable/Wraps";
    const WRAPS_MAX_BYTES: usize = 20 * 1024 * 1024;

    let url = format!("{}/wraps/download/{}", COMMUNITY_API, code);
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => return bad_gateway(&e.to_string()),
    };
    let resp = match client
        .get(&url)
        .headers(forward_headers(&headers, true))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return bad_gateway(&format!("Community wraps service unreachable: {}", e)),
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    if !status.is_success() {
        let bytes = resp.bytes().await.unwrap_or_default();
        let mut r = Response::new(axum::body::Body::from(bytes));
        *r.status_mut() = status;
        r.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        return r;
    }

    let wrap_name_header = resp
        .headers()
        .get("x-wrap-name")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let raw_bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&format!("Failed to download wrap: {}", e)),
    };
    if raw_bytes.len() > WRAPS_MAX_BYTES {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "Downloaded wrap exceeds 20 MB size limit"})),
        )
            .into_response();
    }

    // Sanitize filename: keep alphanumeric, spaces, dot, underscore, dash.
    // Anything else gets stripped. Force .png suffix.
    let raw_name = if wrap_name_header.is_empty() {
        code.clone()
    } else {
        wrap_name_header
    };
    let safe_re = regex::Regex::new(r"[^a-zA-Z0-9 \-_.]").unwrap();
    let mut wrap_name = safe_re.replace_all(&raw_name, "").trim().to_string();
    if wrap_name.is_empty() || wrap_name == ".png" {
        wrap_name = "wrap".to_string();
    }
    if !wrap_name.to_lowercase().ends_with(".png") {
        wrap_name.push_str(".png");
    }

    let _ = std::fs::create_dir_all(WRAPS_DIR);
    let (dest_path, final_name) =
        match crate::lock_chime::deduplicate_filename(WRAPS_DIR, &wrap_name) {
            Some(t) => t,
            None => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({"error": "Too many duplicate filenames"})),
                )
                    .into_response();
            }
        };

    if let Err(e) = std::fs::write(&dest_path, &raw_bytes) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": format!("Failed to save wrap: {}", e)})),
        )
            .into_response();
    }

    Json(serde_json::json!({
        "success": true,
        "filename": final_name,
        "path": dest_path.to_string_lossy(),
        "size": raw_bytes.len(),
    }))
    .into_response()
}

pub async fn wraps_admin_validate(
    State(_s): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if headers.get("x-passcode").is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }
    proxy_json(
        reqwest::Method::POST,
        "/wraps/admin/validate",
        None,
        forward_headers(&headers, false),
        Duration::from_secs(15),
    )
    .await
}

pub async fn wraps_admin_edit(
    State(_s): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }
    if headers.get("x-passcode").is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }
    proxy_json(
        reqwest::Method::PUT,
        &format!("/wraps/admin/edit/{}", code),
        Some(body.to_vec()),
        forward_headers(&headers, false),
        Duration::from_secs(15),
    )
    .await
}

pub async fn wraps_admin_delete(
    State(_s): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !valid_code(&code) {
        return invalid_code();
    }
    if headers.get("x-passcode").is_none() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "Unauthorized"})),
        )
            .into_response();
    }
    proxy_json(
        reqwest::Method::DELETE,
        &format!("/wraps/admin/delete/{}", code),
        None,
        forward_headers(&headers, false),
        Duration::from_secs(15),
    )
    .await
}
