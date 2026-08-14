//! File operations API: list, mkdir, mv, cp, delete, upload, download, zip.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::extract::{Form, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::io::ReaderStream;
use tracing::info;

use crate::router::AppState;

/// Allowed base paths for file operations.
///
/// `/var/www/html/fs` triggers the autofs mounts for auxiliary disk images;
/// their direct `/mnt` paths are otherwise empty `noauto` mountpoints.
const ALLOWED_BASES: &[&str] = &[
    "/mutable",
    "/mnt/cam",
    "/mnt/cam/TeslaCam",
    "/mutable/LicensePlate",
    "/mutable/LockChime",
    "/mnt/music",
    "/mnt/lightshow",
    "/mnt/boombox",
    "/var/www/html/fs",
];

/// Resolve `.` and `..` lexically beneath `/` without following the filesystem.
fn lexical_normalize(req_path: &str) -> PathBuf {
    let mut parts: Vec<std::ffi::OsString> = Vec::new();
    for component in Path::new(req_path).components() {
        match component {
            std::path::Component::Normal(c) => parts.push(c.to_os_string()),
            std::path::Component::ParentDir => {
                parts.pop();
            }
            // The result is already anchored at `/`.
            _ => {}
        }
    }
    let mut p = PathBuf::from("/");
    for part in parts {
        p.push(part);
    }
    p
}

/// Validate and clean a path against the allowed bases.
///
/// Validate the logical path rather than its symlink target. Clip symlinks point
/// into read-only snapshot mounts outside these bases; lexical normalization still
/// blocks traversal, and this API cannot create symlinks.
fn is_path_allowed(req_path: &str) -> (PathBuf, bool) {
    let clean = lexical_normalize(req_path);
    let clean_str = clean.to_str().unwrap_or("");
    for base in ALLOWED_BASES {
        // Require a path boundary so `/mutable` cannot match `/mutable-secret`.
        if clean_str == *base || clean_str.starts_with(&format!("{}/", base)) {
            return (clean, true);
        }
    }
    (clean, false)
}

#[derive(Serialize)]
struct FileEntry {
    name: String,
    path: String,
    is_dir: bool,
    size: i64,
    mod_time: String,
}

#[derive(Serialize)]
struct FileListResponse {
    path: String,
    entries: Vec<FileEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<usize>,
}

#[derive(Deserialize)]
pub struct ListParams {
    path: Option<String>,
    offset: Option<usize>,
    limit: Option<usize>,
    search: Option<String>,
}

/// GET /api/files/ls
pub async fn list_files(
    State(_s): State<AppState>,
    Query(params): Query<ListParams>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Autofs snapshot mounts can make directory reads block for seconds.
    tokio::task::spawn_blocking(move || list_files_blocking(params))
        .await
        .unwrap_or_else(|_| {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, "file listing task failed")
        })
}

fn list_files_blocking(params: ListParams) -> (StatusCode, Json<serde_json::Value>) {
    let req_path = params.path.as_deref().unwrap_or("/");
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(0);
    let search = params.search.as_deref().unwrap_or("").to_lowercase();

    let full_path = if Path::new(req_path).is_absolute() {
        req_path.to_string()
    } else {
        let mut found = None;
        for base in ALLOWED_BASES {
            let test = format!("{}/{}", base, req_path);
            if Path::new(&test).exists() {
                found = Some(test);
                break;
            }
        }
        found.unwrap_or_else(|| format!("{}/{}", ALLOWED_BASES[0], req_path))
    };

    let (clean_path, allowed) = is_path_allowed(&full_path);
    if !allowed {
        return crate::json_error(StatusCode::FORBIDDEN, "Access denied");
    }

    let clean_str = clean_path.to_str().unwrap_or("");
    for base in ALLOWED_BASES {
        if clean_str == *base {
            let _ = std::fs::create_dir_all(&clean_path);
            break;
        }
    }

    let mut dir_entries: Vec<(String, bool)> = match std::fs::read_dir(&clean_path) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| (e.file_name().to_string_lossy().to_string(), e.path().is_dir()))
            .collect(),
        Err(_) => {
            return (StatusCode::OK, Json(serde_json::to_value(FileListResponse {
                path: req_path.to_string(),
                entries: Vec::new(),
                total: None,
            }).unwrap_or_default()));
        }
    };

    dir_entries.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    if !search.is_empty() {
        dir_entries.retain(|(name, _)| name.to_lowercase().contains(&search));
    }

    let total = dir_entries.len();

    let paginated = if limit > 0 {
        let start = offset.min(dir_entries.len());
        let end = (start + limit).min(dir_entries.len());
        &dir_entries[start..end]
    } else {
        &dir_entries[..]
    };

    let mut files = Vec::with_capacity(paginated.len());
    for (name, _) in paginated {
        let entry_path = clean_path.join(name);
        if let Ok(meta) = std::fs::metadata(&entry_path) {
            files.push(FileEntry {
                name: name.clone(),
                path: format!("{}/{}", req_path.trim_end_matches('/'), name),
                is_dir: meta.is_dir(),
                size: meta.len() as i64,
                mod_time: meta.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| {
                        chrono::DateTime::from_timestamp(d.as_secs() as i64, 0)
                            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default(),
            });
        }
    }

    let resp = FileListResponse {
        path: req_path.to_string(),
        entries: files,
        total: if limit > 0 { Some(total) } else { None },
    };

    (StatusCode::OK, Json(serde_json::to_value(resp).unwrap_or_default()))
}

#[derive(Deserialize)]
pub struct PathRequest {
    path: String,
}

#[derive(Deserialize)]
pub struct MoveRequest {
    source: String,
    dest: String,
}

/// POST /api/files/mkdir
pub async fn create_dir(State(_s): State<AppState>, Json(req): Json<PathRequest>) -> (StatusCode, Json<serde_json::Value>) {
    let (clean, allowed) = is_path_allowed(&req.path);
    if !allowed {
        return crate::json_error(StatusCode::FORBIDDEN, "Access denied");
    }
    match std::fs::create_dir_all(&clean) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to create directory: {}", e)),
    }
}

/// POST /api/files/mv
pub async fn move_file(State(_s): State<AppState>, Json(req): Json<MoveRequest>) -> (StatusCode, Json<serde_json::Value>) {
    let (src, src_ok) = is_path_allowed(&req.source);
    let (dst, dst_ok) = is_path_allowed(&req.dest);
    if !src_ok || !dst_ok {
        return crate::json_error(StatusCode::FORBIDDEN, "Access denied");
    }
    match std::fs::rename(&src, &dst) {
        Ok(()) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to move: {}", e)),
    }
}

/// POST /api/files/cp
pub async fn copy_file(State(_s): State<AppState>, Json(req): Json<MoveRequest>) -> (StatusCode, Json<serde_json::Value>) {
    let (src, src_ok) = is_path_allowed(&req.source);
    let (dst, dst_ok) = is_path_allowed(&req.dest);
    if !src_ok || !dst_ok {
        return crate::json_error(StatusCode::FORBIDDEN, "Access denied");
    }
    match std::fs::copy(&src, &dst) {
        Ok(_) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to copy: {}", e)),
    }
}

#[derive(Deserialize)]
pub struct DeleteParams {
    path: String,
}

/// DELETE /api/files
pub async fn delete_file(State(_s): State<AppState>, Query(params): Query<DeleteParams>) -> (StatusCode, Json<serde_json::Value>) {
    let (clean, allowed) = is_path_allowed(&params.path);
    if !allowed {
        return crate::json_error(StatusCode::FORBIDDEN, "Access denied");
    }

    let clean_str = clean.to_str().unwrap_or("");
    for base in ALLOWED_BASES {
        if clean_str == *base {
            return crate::json_error(StatusCode::FORBIDDEN, "Cannot delete root directory");
        }
    }

    let result = if clean.is_dir() {
        std::fs::remove_dir_all(&clean)
    } else {
        std::fs::remove_file(&clean)
    };

    match result {
        Ok(()) => {
            // Tombstones prevent reverse-sync from restoring deleted wraps.
            if clean_str.starts_with("/mutable/Wraps/") {
                let tombstone_dir = std::path::Path::new("/mutable/.wraps_deleted");
                if std::fs::create_dir_all(tombstone_dir).is_ok() {
                    if let Some(base) = clean.file_name() {
                        let _ = std::fs::write(tombstone_dir.join(base), b"");
                    }
                }
            }
            if clean_str.contains("/SavedClips/") || clean_str.contains("/SentryClips/") {
                let path = clean_str.to_string();
                tokio::spawn(async move { cleanup_snapshot_symlinks(&path); });
            }
            crate::json_ok()
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to delete: {}", e)),
    }
}

/// POST /api/files/upload
///
/// Multipart form: `file` (required, the file payload) and `path` (required,
/// destination directory). Filename is taken from the upload part's
/// Content-Disposition `filename=`. Streams directly to disk — no in-memory
/// buffering, so files of any size can be uploaded on low-RAM devices.
pub async fn upload_file(
    State(_s): State<AppState>,
    mut multipart: axum::extract::Multipart,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut dest_dir: Option<String> = None;
    let mut filename: Option<String> = None;
    let mut written: u64 = 0;
    let mut file_written = false;

    // Multipart fields may arrive in either order, so stream the file to a
    // temporary path before validating its destination.
    let mut temp_path: Option<PathBuf> = None;

    while let Ok(Some(mut field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "path" => {
                if let Ok(v) = field.text().await {
                    dest_dir = Some(v);
                }
            }
            "file" => {
                let fname = field
                    .file_name()
                    .unwrap_or("upload.bin")
                    .to_string();
                filename = Some(fname);

                // Include process, time, and sequence values so concurrent
                // streaming uploads cannot share a temporary file.
                static UPLOAD_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let seq = UPLOAD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let nanos = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0);
                let tmp = std::env::temp_dir().join(format!(
                    "sentryusb-upload-{}-{}-{}",
                    std::process::id(),
                    seq,
                    nanos
                ));
                let mut file = match tokio::fs::File::create(&tmp).await {
                    Ok(f) => f,
                    Err(e) => {
                        return crate::json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("Failed to create temp file: {}", e),
                        );
                    }
                };

                while let Ok(Some(chunk)) = field.chunk().await {
                    use tokio::io::AsyncWriteExt;
                    if let Err(e) = file.write_all(&chunk).await {
                        let _ = tokio::fs::remove_file(&tmp).await;
                        return crate::json_error(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            &format!("Failed to write chunk: {}", e),
                        );
                    }
                    written += chunk.len() as u64;
                }

                use tokio::io::AsyncWriteExt;
                let _ = file.flush().await;
                temp_path = Some(tmp);
                file_written = true;
            }
            _ => {}
        }
    }

    if !file_written {
        return crate::json_error(StatusCode::BAD_REQUEST, "Missing file in upload");
    }
    let filename = filename.unwrap_or_else(|| "upload.bin".to_string());
    let dest_dir = match dest_dir {
        Some(d) if !d.is_empty() => d,
        _ => {
            if let Some(tmp) = &temp_path { let _ = tokio::fs::remove_file(tmp).await; }
            return crate::json_error(StatusCode::BAD_REQUEST, "Missing path parameter");
        }
    };

    let dest_path = format!("{}/{}", dest_dir.trim_end_matches('/'), filename);
    let (clean, allowed) = is_path_allowed(&dest_path);
    if !allowed {
        if let Some(tmp) = &temp_path { let _ = tokio::fs::remove_file(tmp).await; }
        return crate::json_error(StatusCode::FORBIDDEN, "Access denied");
    }

    if let Some(parent) = clean.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            if let Some(tmp) = &temp_path { let _ = tokio::fs::remove_file(tmp).await; }
            return crate::json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Failed to create directory: {}", e),
            );
        }
    }

    if let Some(tmp) = temp_path {
        if let Err(_) = tokio::fs::rename(&tmp, &clean).await {
            // Cross-filesystem renames require copy and delete.
            if let Err(e) = tokio::fs::copy(&tmp, &clean).await {
                let _ = tokio::fs::remove_file(&tmp).await;
                return crate::json_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &format!("Failed to write file: {}", e),
                );
            }
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "name": filename,
            "path": dest_path,
            "size": written.to_string(),
        })),
    )
}

/// GET /api/files/download
pub async fn download_file(State(_s): State<AppState>, Query(params): Query<DeleteParams>) -> impl IntoResponse {
    let (clean, allowed) = is_path_allowed(&params.path);
    if !allowed {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }

    let file = match tokio::fs::File::open(&clean).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    };
    // Unix can open a directory as a file, but it cannot be streamed.
    match file.metadata().await {
        Ok(m) if !m.is_dir() => {}
        _ => return (StatusCode::NOT_FOUND, "File not found").into_response(),
    }

    let filename = clean.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", filename)),
            (axum::http::header::CONTENT_TYPE, "application/octet-stream".to_string()),
        ],
        Body::from_stream(ReaderStream::new(file)),
    ).into_response()
}

/// GET /api/files/download-zip
pub async fn download_zip(State(_s): State<AppState>, Query(params): Query<DeleteParams>) -> impl IntoResponse {
    let (clean, allowed) = is_path_allowed(&params.path);
    if !allowed {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }

    if !clean.is_dir() {
        return (StatusCode::BAD_REQUEST, "Path is not a directory").into_response();
    }

    let dirname = clean.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download")
        .to_string();

    let root = clean.clone();
    spawn_zip_stream(
        format!("attachment; filename=\"{}.zip\"", dirname),
        move |z, central| walk_dir(z, central, &root, &root),
    )
}

/// POST /api/files/download-zip-multi
pub async fn download_zip_multi(State(_s): State<AppState>, Form(req): Form<MultiZipRequest>) -> impl IntoResponse {
    let paths: Vec<String> = match serde_json::from_str(&req.paths) {
        Ok(p) => p,
        Err(_) => return (StatusCode::BAD_REQUEST, "Missing or invalid paths").into_response(),
    };
    if paths.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing or invalid paths").into_response();
    }

    let mut clean_paths = Vec::new();
    for p in &paths {
        let (clean, allowed) = is_path_allowed(p);
        if !allowed {
            return (StatusCode::FORBIDDEN, format!("Access denied: {}", p)).into_response();
        }
        if !clean.exists() {
            return (StatusCode::NOT_FOUND, format!("Not found: {}", p)).into_response();
        }
        clean_paths.push(clean);
    }

    spawn_zip_stream(
        "attachment; filename=\"download.zip\"".to_string(),
        move |z, central| {
            for cp in &clean_paths {
                if cp.is_dir() {
                    let parent = cp.parent().unwrap_or(cp);
                    walk_dir(z, central, cp, parent)?;
                } else {
                    let name = cp.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                    write_stored_entry(z, central, name, cp)?;
                }
            }
            Ok(())
        },
    )
}

#[derive(Deserialize)]
pub struct MultiZipRequest {
    /// JSON-encoded array submitted in the form's `paths` field.
    paths: String,
}

// Stream Stored ZIP entries using data descriptors, retaining only central-directory
// metadata. This avoids buffering or recompressing video. ZIP64 covers 32-bit size
// or offset overflow and more than 65,535 entries.

const ZIP_CHUNK: usize = 64 * 1024;
const ZIP_CHANNEL_DEPTH: usize = 8;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_DATA_DESC: u32 = 0x0807_4b50;
const SIG_CENTRAL: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;
const SIG_ZIP64_EOCD: u32 = 0x0606_4b50;
const SIG_ZIP64_LOCATOR: u32 = 0x0706_4b50;
/// General-purpose flags: bit 3 (data descriptor follows) + bit 11 (UTF-8 name).
const GP_FLAGS: u16 = 0x0808;
const U32_MAX: u64 = 0xFFFF_FFFF;

/// ZIP64 thresholds, reduced in tests to exercise overflow paths cheaply.
#[derive(Clone, Copy)]
struct Zip64Thresholds {
    bytes: u64,
    entries: usize,
}

impl Default for Zip64Thresholds {
    fn default() -> Self {
        Zip64Thresholds { bytes: U32_MAX, entries: 0xFFFF }
    }
}

/// Non-seekable response sink that tracks central-directory offsets.
struct ZipStream {
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    offset: u64,
    limits: Zip64Thresholds,
}

impl ZipStream {
    /// Send a block, returning an error if the client disconnected.
    fn send(&mut self, data: Vec<u8>) -> std::io::Result<()> {
        let len = data.len() as u64;
        self.tx.blocking_send(data).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "client disconnected")
        })?;
        self.offset += len;
        Ok(())
    }
}

/// Per-entry metadata retained to build the central directory at the end.
struct CentralEntry {
    name: Vec<u8>,
    crc: u32,
    size: u64,
    offset: u64,
    dos_date: u16,
    dos_time: u16,
    zip64_size: bool,
}

fn le_u16(v: &mut Vec<u8>, n: u16) {
    v.extend_from_slice(&n.to_le_bytes());
}
fn le_u32(v: &mut Vec<u8>, n: u32) {
    v.extend_from_slice(&n.to_le_bytes());
}
fn le_u64(v: &mut Vec<u8>, n: u64) {
    v.extend_from_slice(&n.to_le_bytes());
}

/// Convert a file mtime to a DOS date/time pair (the FAT epoch is 1980).
fn dos_datetime(t: SystemTime) -> (u16, u16) {
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    match chrono::DateTime::from_timestamp(secs as i64, 0) {
        Some(dt) => {
            use chrono::{Datelike, Timelike};
            if dt.year() < 1980 {
                return (0x0021, 0); // 1980-01-01 00:00:00
            }
            let date = (((dt.year() - 1980) as u16) << 9)
                | ((dt.month() as u16) << 5)
                | (dt.day() as u16);
            let time = ((dt.hour() as u16) << 11)
                | ((dt.minute() as u16) << 5)
                | ((dt.second() as u16) / 2);
            (date, time)
        }
        None => (0x0021, 0),
    }
}

/// Stream one file and retain its central-directory metadata.
/// Unreadable files are skipped; an error means the client disconnected.
fn write_stored_entry(
    z: &mut ZipStream,
    central: &mut Vec<CentralEntry>,
    name: &str,
    path: &Path,
) -> std::io::Result<()> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Ok(()),
    };
    let meta = f.metadata().ok();
    let (dos_date, dos_time) = meta
        .as_ref()
        .and_then(|m| m.modified().ok())
        .map(dos_datetime)
        .unwrap_or((0x0021, 0));
    let stat_size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    // Select ZIP64 before writing so the header and descriptor agree.
    let zip64 = stat_size >= z.limits.bytes;
    let name_bytes = name.as_bytes();
    let offset = z.offset;

    let mut h = Vec::with_capacity(30 + name_bytes.len() + 20);
    le_u32(&mut h, SIG_LOCAL);
    le_u16(&mut h, if zip64 { 45 } else { 20 }); // version needed
    le_u16(&mut h, GP_FLAGS);
    le_u16(&mut h, 0); // method: stored
    le_u16(&mut h, dos_time);
    le_u16(&mut h, dos_date);
    le_u32(&mut h, 0); // crc-32 (in data descriptor)
    let size_field = if zip64 { U32_MAX as u32 } else { 0 };
    le_u32(&mut h, size_field); // compressed size (in descriptor / zip64 extra)
    le_u32(&mut h, size_field); // uncompressed size
    le_u16(&mut h, name_bytes.len() as u16);
    let extra = if zip64 {
        let mut e = Vec::new();
        le_u16(&mut e, 0x0001); // ZIP64 extended information tag
        le_u16(&mut e, 16); // body size: two u64s
        le_u64(&mut e, stat_size); // uncompressed
        le_u64(&mut e, stat_size); // compressed
        e
    } else {
        Vec::new()
    };
    le_u16(&mut h, extra.len() as u16);
    h.extend_from_slice(name_bytes);
    h.extend_from_slice(&extra);
    z.send(h)?;

    let mut hasher = crc32fast::Hasher::new();
    let mut size: u64 = 0;
    let mut buf = vec![0u8; ZIP_CHUNK];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
        z.send(buf[..n].to_vec())?;
    }
    let crc = hasher.finalize();

    let mut d = Vec::with_capacity(24);
    le_u32(&mut d, SIG_DATA_DESC);
    le_u32(&mut d, crc);
    if zip64 {
        le_u64(&mut d, size); // compressed
        le_u64(&mut d, size); // uncompressed
    } else {
        le_u32(&mut d, size as u32);
        le_u32(&mut d, size as u32);
    }
    z.send(d)?;

    central.push(CentralEntry {
        name: name_bytes.to_vec(),
        crc,
        size,
        offset,
        dos_date,
        dos_time,
        zip64_size: zip64,
    });
    Ok(())
}

/// Recursively add every regular file under `dir` to the archive, naming entries
/// relative to `base`.
fn walk_dir(
    z: &mut ZipStream,
    central: &mut Vec<CentralEntry>,
    dir: &Path,
    base: &Path,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir(z, central, &path, base)?;
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            if let Some(name) = rel.to_str() {
                if !name.is_empty() {
                    write_stored_entry(z, central, name, &path)?;
                }
            }
        }
    }
    Ok(())
}

/// Write the central directory and end-of-central-directory records (with ZIP64
/// structures when offsets/sizes/counts overflow their 32-/16-bit fields).
fn write_central_directory(z: &mut ZipStream, central: &[CentralEntry]) -> std::io::Result<()> {
    let central_start = z.offset;
    for e in central {
        let offset_zip64 = e.offset >= z.limits.bytes;
        let size_zip64 = e.zip64_size;

        let mut extra = Vec::new();
        if size_zip64 || offset_zip64 {
            let mut body = Vec::new();
            if size_zip64 {
                le_u64(&mut body, e.size); // uncompressed
                le_u64(&mut body, e.size); // compressed
            }
            if offset_zip64 {
                le_u64(&mut body, e.offset);
            }
            le_u16(&mut extra, 0x0001);
            le_u16(&mut extra, body.len() as u16);
            extra.extend_from_slice(&body);
        }
        let version: u16 = if extra.is_empty() { 20 } else { 45 };

        let mut h = Vec::with_capacity(46 + e.name.len() + extra.len());
        le_u32(&mut h, SIG_CENTRAL);
        le_u16(&mut h, (3 << 8) | version); // version made by (UNIX)
        le_u16(&mut h, version); // version needed
        le_u16(&mut h, GP_FLAGS);
        le_u16(&mut h, 0); // method: stored
        le_u16(&mut h, e.dos_time);
        le_u16(&mut h, e.dos_date);
        le_u32(&mut h, e.crc);
        let size_field = if size_zip64 { U32_MAX as u32 } else { e.size as u32 };
        le_u32(&mut h, size_field); // compressed
        le_u32(&mut h, size_field); // uncompressed
        le_u16(&mut h, e.name.len() as u16);
        le_u16(&mut h, extra.len() as u16);
        le_u16(&mut h, 0); // file comment length
        le_u16(&mut h, 0); // disk number start
        le_u16(&mut h, 0); // internal attributes
        // Store a readable regular-file mode in the Unix attribute bits.
        le_u32(&mut h, 0o100644 << 16); // external attributes
        le_u32(&mut h, if offset_zip64 { U32_MAX as u32 } else { e.offset as u32 });
        h.extend_from_slice(&e.name);
        h.extend_from_slice(&extra);
        z.send(h)?;
    }

    let central_size = z.offset - central_start;
    let count = central.len();
    let need_zip64 = central_start >= z.limits.bytes
        || central_size >= z.limits.bytes
        || count >= z.limits.entries;

    if need_zip64 {
        let zip64_eocd_at = z.offset;
        let mut r = Vec::with_capacity(56);
        le_u32(&mut r, SIG_ZIP64_EOCD);
        le_u64(&mut r, 44); // size of the record that follows this field
        le_u16(&mut r, (3 << 8) | 45); // version made by
        le_u16(&mut r, 45); // version needed
        le_u32(&mut r, 0); // number of this disk
        le_u32(&mut r, 0); // disk with central directory
        le_u64(&mut r, count as u64); // entries on this disk
        le_u64(&mut r, count as u64); // total entries
        le_u64(&mut r, central_size);
        le_u64(&mut r, central_start);
        z.send(r)?;

        let mut loc = Vec::with_capacity(20);
        le_u32(&mut loc, SIG_ZIP64_LOCATOR);
        le_u32(&mut loc, 0); // disk with the ZIP64 EOCD
        le_u64(&mut loc, zip64_eocd_at);
        le_u32(&mut loc, 1); // total number of disks
        z.send(loc)?;
    }

    // ZIP64 sentinels are required only for fields that actually overflow.
    let mut eocd = Vec::with_capacity(22);
    le_u32(&mut eocd, SIG_EOCD);
    le_u16(&mut eocd, 0); // number of this disk
    le_u16(&mut eocd, 0); // disk with central directory
    let count16 = if count >= 0xFFFF { 0xFFFF } else { count as u16 };
    le_u16(&mut eocd, count16);
    le_u16(&mut eocd, count16);
    le_u32(&mut eocd, if central_size >= U32_MAX { U32_MAX as u32 } else { central_size as u32 });
    le_u32(&mut eocd, if central_start >= U32_MAX { U32_MAX as u32 } else { central_start as u32 });
    le_u16(&mut eocd, 0); // comment length
    z.send(eocd)?;
    Ok(())
}

/// Spawn a blocking task that streams a zip built by `build` to the response body.
/// Returns a `200` streaming response with the given Content-Disposition;
/// validation/status decisions happen in the caller, so the contract is unchanged.
fn spawn_zip_stream<F>(disposition: String, build: F) -> Response
where
    F: FnOnce(&mut ZipStream, &mut Vec<CentralEntry>) -> std::io::Result<()> + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(ZIP_CHANNEL_DEPTH);
    tokio::task::spawn_blocking(move || {
        let mut z = ZipStream { tx, offset: 0, limits: Zip64Thresholds::default() };
        let mut central = Vec::new();
        // A disconnected client cannot receive a usable central directory.
        if build(&mut z, &mut central).is_ok() {
            let _ = write_central_directory(&mut z, &central);
        }
    });

    let stream = ReceiverStream::new(rx).map(Ok::<Vec<u8>, std::io::Error>);
    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/zip".to_string()),
            (axum::http::header::CONTENT_DISPOSITION, disposition),
        ],
        Body::from_stream(stream),
    )
        .into_response()
}

/// Clean up snapshot symlinks after deleting SavedClips/SentryClips files.
fn cleanup_snapshot_symlinks(deleted_path: &str) {
    let mut clip_type = "";
    let mut event_name = "";

    for ct in &["SavedClips", "SentryClips"] {
        let marker = format!("/{}/", ct);
        if let Some(idx) = deleted_path.find(&marker) {
            clip_type = ct;
            let rest = &deleted_path[idx + marker.len()..];
            event_name = rest.split('/').next().unwrap_or("");
            break;
        }
    }

    if clip_type.is_empty() || event_name.is_empty() {
        return;
    }

    info!("[files] Cleaning up snapshot symlinks for {}/{}", clip_type, event_name);

    let snapshots_base = Path::new("/backingfiles/snapshots");
    if let Ok(entries) = std::fs::read_dir(snapshots_base) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !entry.path().is_dir() || !name_str.starts_with("snap-") {
                continue;
            }

            let event_dir = snapshots_base.join(&name).join("mnt/TeslaCam").join(clip_type).join(event_name);
            if !event_dir.exists() {
                continue;
            }

            if let Ok(clip_entries) = std::fs::read_dir(&event_dir) {
                for ce in clip_entries.flatten() {
                    let link_path = ce.path();
                    if let Ok(meta) = std::fs::symlink_metadata(&link_path) {
                        if meta.file_type().is_symlink() {
                            let _ = std::fs::remove_file(&link_path);
                        }
                    }
                }
            }

            if let Ok(remaining) = std::fs::read_dir(&event_dir) {
                if remaining.count() == 0 {
                    let _ = std::fs::remove_dir(&event_dir);
                }
            }
        }
    }

    info!("[files] Snapshot symlink cleanup complete for {}/{}", clip_type, event_name);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn allows_clip_paths_and_blocks_traversal() {
        assert!(
            is_path_allowed(
                "/mutable/TeslaCam/SavedClips/2026-05-02_08-50-13/2026-05-02_08-46-34-back.mp4"
            )
            .1
        );
        assert!(is_path_allowed("/mutable").1);
        assert!(is_path_allowed("/mnt/cam/TeslaCam/SentryClips/x/y.mp4").1);

        assert!(!is_path_allowed("/mutable/../etc/passwd").1);
        assert!(!is_path_allowed("/mutable/TeslaCam/../../../../etc/shadow").1);
        assert!(!is_path_allowed("/etc/passwd").1);
        assert!(!is_path_allowed("/mutable-secret/data").1);
    }

    #[test]
    fn lexical_normalize_does_not_follow_symlinks() {
        let dir = TempDir::new().unwrap();
        let link = dir.path().join("escape");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", &link).unwrap();
        let through = link.join("passwd");
        assert_eq!(lexical_normalize(through.to_str().unwrap()), through);

        assert_eq!(lexical_normalize("/a/b/../c"), PathBuf::from("/a/c"));
        assert_eq!(lexical_normalize("/../../x"), PathBuf::from("/x"));
    }

    async fn build_archive(root: &Path, limits: Zip64Thresholds) -> Vec<u8> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(ZIP_CHANNEL_DEPTH);
        let root = root.to_path_buf();
        let handle = tokio::task::spawn_blocking(move || {
            let mut z = ZipStream { tx, offset: 0, limits };
            let mut central = Vec::new();
            walk_dir(&mut z, &mut central, &root, &root).unwrap();
            write_central_directory(&mut z, &central).unwrap();
        });
        // Drain concurrently so the bounded channel cannot stall the writer.
        let mut bytes = Vec::new();
        while let Some(chunk) = rx.recv().await {
            bytes.extend_from_slice(&chunk);
        }
        handle.await.unwrap();
        bytes
    }

    #[tokio::test]
    async fn streaming_zip_roundtrips() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let big = vec![7u8; 300 * 1024]; // spans multiple ZIP_CHUNK sends
        std::fs::write(dir.path().join("sub").join("b.bin"), &big).unwrap();

        let bytes = build_archive(dir.path(), Zip64Thresholds::default()).await;

        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("valid zip");
        let mut names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.txt".to_string(), "sub/b.bin".to_string()]);

        let a = archive.by_name("a.txt").unwrap();
        assert_eq!(a.compression(), zip::CompressionMethod::Stored);
        drop(a);

        let mut s = String::new();
        archive.by_name("a.txt").unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello");

        let mut buf = Vec::new();
        archive.by_name("sub/b.bin").unwrap().read_to_end(&mut buf).unwrap();
        assert_eq!(buf, big);
    }

    #[tokio::test]
    async fn streaming_zip64_path_roundtrips() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let big = vec![9u8; 200 * 1024];
        std::fs::write(dir.path().join("b.bin"), &big).unwrap();
        std::fs::write(dir.path().join("c.txt"), b"tail").unwrap();

        let limits = Zip64Thresholds { bytes: 10, entries: 2 };
        let bytes = build_archive(dir.path(), limits).await;

        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("valid zip64 archive");
        assert_eq!(archive.len(), 3);

        let mut got = String::new();
        archive.by_name("a.txt").unwrap().read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello");

        let mut b = Vec::new();
        archive.by_name("b.bin").unwrap().read_to_end(&mut b).unwrap();
        assert_eq!(b, big);

        let mut t = String::new();
        archive.by_name("c.txt").unwrap().read_to_string(&mut t).unwrap();
        assert_eq!(t, "tail");
    }
}
