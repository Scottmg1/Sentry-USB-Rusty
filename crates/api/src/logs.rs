//! Log file viewer.
//!
//! Returns raw `text/plain` content (the frontend parses the text directly).

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::io::{Read, Seek, SeekFrom};

use crate::router::AppState;

/// Known log files and their paths.
fn log_path(name: &str) -> Option<&'static str> {
    match name {
        "archiveloop" => Some("/mutable/archiveloop.log"),
        "setup" => Some("/sentryusb/sentryusb-setup.log"),
        "diagnostics" => Some("/tmp/diagnostics.txt"),
        "syslog" => Some("/var/log/syslog"),
        "kern" => Some("/var/log/kern.log"),
        "auth" => Some("/var/log/auth.log"),
        "daemon" => Some("/var/log/daemon.log"),
        "sentryusb" => Some("/var/log/sentryusb.log"),
        "sentryusb-ble" => Some("/var/log/sentryusb-ble.log"),
        _ => None,
    }
}

/// Max bytes to return — prevents OOM on 512 MB Pi devices where syslog/kern
/// can grow to 50–200 MB without rotation.
const MAX_TAIL_BYTES: u64 = 512 * 1024;

/// GET /api/logs/{name}
///
/// Returns the tail of the log file as `text/plain`, matching the Go original.
pub async fn get_log(
    State(_s): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return (StatusCode::BAD_REQUEST, "invalid log name").into_response();
    }

    let path = match log_path(&name) {
        Some(p) => p.to_string(),
        None => format!("/var/log/{}", name),
    };

    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "Log file not found").into_response(),
    };

    let meta = match file.metadata() {
        Ok(m) => m,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "Cannot stat log file").into_response(),
    };

    // If the file is larger than the cap, seek to the last MAX_TAIL_BYTES and
    // skip the first partial line so output starts at a clean boundary.
    if meta.len() > MAX_TAIL_BYTES {
        let _ = file.seek(SeekFrom::End(-(MAX_TAIL_BYTES as i64)));
        let mut one = [0u8; 1];
        loop {
            match file.read(&mut one) {
                Ok(1) if one[0] == b'\n' => break,
                Ok(1) => continue,
                _ => break,
            }
        }
    }

    let mut buf = String::new();
    if let Err(_) = file.read_to_string(&mut buf) {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to read log file").into_response();
    }

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        buf,
    )
        .into_response()
}
