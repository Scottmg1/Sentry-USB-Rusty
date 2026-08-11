//! Log file viewer.
//!
//! Returns raw `text/plain` content (the frontend parses the text directly).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
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
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Response {
    // Special-case: the Bluetooth tab isn't a static file — it's a
    // live dump built from systemctl + sysfs + telemetry DB +
    // journalctl. Delegate to the dedicated handler.
    if name == "bluetooth" {
        return crate::ble_debug::get_ble_debug(State(s)).await;
    }
    get_log_tail(Path(name)).await
}

/// State-free tail read — also mounted by the degraded router, where no
/// AppState exists (bluetooth's live dump needs the full app, so only
/// the plain-file path is available there).
pub async fn get_log_tail(Path(name): Path<String>) -> Response {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return (StatusCode::BAD_REQUEST, "invalid log name").into_response();
    }

    // Tail-reading a log seeks + reads up to 512 KB off the SD card — keep
    // it off the reactor so a slow read can't stall the WebSocket heartbeat.
    tokio::task::spawn_blocking(move || read_log_tail(name))
        .await
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "log read task failed").into_response()
        })
}

fn read_log_tail(name: String) -> Response {
    let known = log_path(&name).is_some();
    let path = match log_path(&name) {
        Some(p) => p.to_string(),
        None => format!("/var/log/{}", name),
    };

    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        // Known logs may legitimately be absent (e.g. archiveloop.log when no
        // NAS is configured yet) — return empty 200 so the UI shows "no log
        // output" instead of a scary console 404. Unknown names still 404.
        Err(_) if known => {
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                String::new(),
            ).into_response();
        }
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

// ---------------------------------------------------------------------------
// Paged reads
// ---------------------------------------------------------------------------

/// Default and maximum lines per page. 50 keeps the first paint cheap on the
/// BLE transport, where the 512 KiB tail above cannot finish inside the
/// client's deadline.
const DEFAULT_PAGE_LINES: usize = 50;
const MAX_PAGE_LINES: usize = 2000;

#[derive(Deserialize)]
pub struct LogPageQuery {
    lines: Option<usize>,
    /// Byte offset returned by a previous page. Reads the lines immediately
    /// before it; absent means start at the end of the file.
    before: Option<u64>,
}

/// GET /api/logs/{name}/page?lines=50&before=<offset>
///
/// JSON sibling of `get_log` for incremental "load older lines" scrolling.
/// Deliberately a separate route: `/api/logs/{name}` stays byte-for-byte
/// text/plain for the web viewer, the download link and older app builds.
/// The cursor travels in the body because the BLE proxy drops response
/// headers.
pub async fn get_log_page(
    Path(name): Path<String>,
    Query(q): Query<LogPageQuery>,
) -> Response {
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return page_error(StatusCode::BAD_REQUEST, "invalid log name");
    }

    // A live systemctl/journalctl dump has no stable byte offsets to page over.
    if name == "bluetooth" {
        return page_error(
            StatusCode::BAD_REQUEST,
            "the bluetooth log is generated on demand and cannot be paged",
        );
    }

    let lines = q.lines.unwrap_or(DEFAULT_PAGE_LINES).clamp(1, MAX_PAGE_LINES);
    let before = q.before;

    tokio::task::spawn_blocking(move || read_log_page(name, lines, before))
        .await
        .unwrap_or_else(|_| {
            page_error(StatusCode::INTERNAL_SERVER_ERROR, "log read task failed")
        })
}

fn page_error(status: StatusCode, msg: &str) -> Response {
    crate::json_error(status, msg).into_response()
}

fn page_response(content: String, start: u64) -> Response {
    let has_more = start > 0;
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "content": content,
            // Feed back as `before` to walk further into the past.
            "before": if has_more { Some(start) } else { None },
            "has_more": has_more,
        })),
    )
        .into_response()
}

fn read_log_page(name: String, lines: usize, before: Option<u64>) -> Response {
    let known = log_path(&name).is_some();
    let path = match log_path(&name) {
        Some(p) => p.to_string(),
        None => format!("/var/log/{}", name),
    };

    let mut file = match std::fs::File::open(&path) {
        Ok(f) => f,
        // Same rule as get_log: a known log that hasn't been created yet is an
        // empty page, not an error.
        Err(_) if known => return page_response(String::new(), 0),
        Err(_) => return page_error(StatusCode::NOT_FOUND, "Log file not found"),
    };

    match page_window(&mut file, lines, before) {
        Ok((content, start)) => page_response(content, start),
        Err(PageError::StaleCursor) => page_error(
            StatusCode::CONFLICT,
            "log rotated since that page; reload from the newest page",
        ),
        Err(PageError::Io) => {
            page_error(StatusCode::INTERNAL_SERVER_ERROR, "Failed to read log file")
        }
    }
}

#[derive(Debug)]
enum PageError {
    /// `before` points past EOF — the log rotated or was truncated.
    StaleCursor,
    Io,
}

/// Returns the last `lines` lines ending at `before` (default EOF), plus the
/// byte offset the window starts at. Split out from the handler so the
/// backwards line walk is unit-testable.
fn page_window(
    file: &mut std::fs::File,
    lines: usize,
    before: Option<u64>,
) -> Result<(String, u64), PageError> {
    let len = file.metadata().map_err(|_| PageError::Io)?.len();

    let end = before.unwrap_or(len);
    if end > len {
        return Err(PageError::StaleCursor);
    }
    if end == 0 {
        return Ok((String::new(), 0));
    }

    // A trailing newline terminates the final line rather than separating two,
    // so exclude it from the count.
    let mut scan_end = end;
    let mut one = [0u8; 1];
    if file.seek(SeekFrom::Start(end - 1)).is_ok()
        && file.read_exact(&mut one).is_ok()
        && one[0] == b'\n'
    {
        scan_end -= 1;
    }

    const CHUNK: u64 = 8 * 1024;
    let mut buf = vec![0u8; CHUNK as usize];
    let mut start = scan_end;
    let mut found = 0usize;

    'outer: while start > 0 && end - start < MAX_TAIL_BYTES {
        let step = CHUNK.min(start);
        let pos = start - step;
        if file.seek(SeekFrom::Start(pos)).is_err() {
            break;
        }
        let slice = &mut buf[..step as usize];
        if file.read_exact(slice).is_err() {
            break;
        }
        for i in (0..step as usize).rev() {
            if slice[i] == b'\n' {
                found += 1;
                if found == lines {
                    start = pos + i as u64 + 1;
                    break 'outer;
                }
            }
        }
        start = pos;
    }

    // Hitting the byte cap before the line count means one page is enormous —
    // return the bounded fragment rather than an empty page that stalls paging.
    if end - start > MAX_TAIL_BYTES {
        start = end - MAX_TAIL_BYTES;
    }

    let take = (end - start) as usize;
    let mut out = vec![0u8; take];
    file.seek(SeekFrom::Start(start)).map_err(|_| PageError::Io)?;
    file.read_exact(&mut out).map_err(|_| PageError::Io)?;

    Ok((String::from_utf8_lossy(&out).into_owned(), start))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(body: &str) -> std::fs::File {
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        f
    }

    #[test]
    fn returns_last_n_lines_with_trailing_newline() {
        let mut f = fixture("a\nb\nc\nd\n");
        let (content, start) = page_window(&mut f, 2, None).unwrap();
        assert_eq!(content, "c\nd\n");
        assert_eq!(start, 4);
    }

    #[test]
    fn returns_last_n_lines_without_trailing_newline() {
        let mut f = fixture("a\nb\nc\nd");
        let (content, start) = page_window(&mut f, 2, None).unwrap();
        assert_eq!(content, "c\nd");
        assert_eq!(start, 4);
    }

    #[test]
    fn walks_backwards_through_pages_to_the_start() {
        let mut f = fixture("a\nb\nc\nd\n");
        let (_, first) = page_window(&mut f, 2, None).unwrap();
        let (content, start) = page_window(&mut f, 2, Some(first)).unwrap();
        assert_eq!(content, "a\nb\n");
        assert_eq!(start, 0);
    }

    #[test]
    fn asking_for_more_lines_than_exist_returns_the_whole_file() {
        let mut f = fixture("a\nb\n");
        let (content, start) = page_window(&mut f, 500, None).unwrap();
        assert_eq!(content, "a\nb\n");
        assert_eq!(start, 0);
    }

    #[test]
    fn empty_file_is_an_empty_page() {
        let mut f = fixture("");
        let (content, start) = page_window(&mut f, 50, None).unwrap();
        assert!(content.is_empty());
        assert_eq!(start, 0);
    }

    #[test]
    fn spans_the_chunk_boundary() {
        // Forces the backwards walk across more than one 8 KiB read.
        let body: String = (0..4000).map(|i| format!("line {i}\n")).collect();
        let mut f = fixture(&body);
        let (content, _) = page_window(&mut f, 3, None).unwrap();
        assert_eq!(content, "line 3997\nline 3998\nline 3999\n");
    }

    #[test]
    fn cursor_past_eof_is_rejected() {
        let mut f = fixture("a\nb\n");
        assert!(matches!(
            page_window(&mut f, 2, Some(9_999)),
            Err(PageError::StaleCursor)
        ));
    }
}
