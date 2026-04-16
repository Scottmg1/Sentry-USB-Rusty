//! File operations API: list, mkdir, mv, cp, delete, upload, download, zip.

use std::io::Write;
use std::path::{Path, PathBuf};

use axum::Json;
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::router::AppState;

/// Allowed base paths for file operations (security).
const ALLOWED_BASES: &[&str] = &[
    "/mutable",
    "/mutable/TeslaCam",
    "/mutable/Wraps",
    "/mutable/LicensePlate",
    "/mutable/LockChime",
    "/var/www/html/fs/Music",
    "/var/www/html/fs/LightShow",
    "/var/www/html/fs/Boombox",
];

/// Validate and clean a path, resolving symlinks and checking against allowed bases.
fn is_path_allowed(req_path: &str) -> (PathBuf, bool) {
    let clean = PathBuf::from(req_path);
    let clean = clean.canonicalize().unwrap_or_else(|_| {
        // canonicalize fails if path doesn't exist — use cleaned version
        let mut p = PathBuf::from("/");
        for component in Path::new(req_path).components() {
            match component {
                std::path::Component::Normal(c) => p.push(c),
                std::path::Component::RootDir => p = PathBuf::from("/"),
                _ => {}
            }
        }
        p
    });

    let clean_str = clean.to_str().unwrap_or("");
    for base in ALLOWED_BASES {
        if clean_str.starts_with(base) || clean_str == *base {
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
    let req_path = params.path.as_deref().unwrap_or("/");
    let offset = params.offset.unwrap_or(0);
    let limit = params.limit.unwrap_or(0);
    let search = params.search.as_deref().unwrap_or("").to_lowercase();

    // Map relative paths to allowed bases
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

    // Auto-create allowed base directories
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

    // Sort: directories first, then alphabetically
    dir_entries.sort_by(|a, b| {
        b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase()))
    });

    // Apply search filter
    if !search.is_empty() {
        dir_entries.retain(|(name, _)| name.to_lowercase().contains(&search));
    }

    let total = dir_entries.len();

    // Apply pagination
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
        // Use std::fs::metadata to follow symlinks
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
            // Clean up snapshot symlinks for SavedClips/SentryClips
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
pub async fn upload_file(State(_s): State<AppState>, body: axum::body::Bytes) -> (StatusCode, Json<serde_json::Value>) {
    // TODO: Implement multipart file upload (requires multipart feature)
    // For now return a helpful error
    crate::json_error(StatusCode::NOT_IMPLEMENTED, "File upload requires multipart support (not yet implemented)")
}

/// GET /api/files/download
pub async fn download_file(State(_s): State<AppState>, Query(params): Query<DeleteParams>) -> impl IntoResponse {
    let (clean, allowed) = is_path_allowed(&params.path);
    if !allowed {
        return (StatusCode::FORBIDDEN, "Access denied").into_response();
    }

    match tokio::fs::read(&clean).await {
        Ok(data) => {
            let filename = clean.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("download");
            (
                StatusCode::OK,
                [
                    (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", filename)),
                    (axum::http::header::CONTENT_TYPE, "application/octet-stream".to_string()),
                ],
                data,
            ).into_response()
        }
        Err(_) => (StatusCode::NOT_FOUND, "File not found").into_response(),
    }
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
        .unwrap_or("download");

    let mut buf = Vec::new();
    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        add_dir_to_zip(&mut zip_writer, &clean, &clean, options);
        let _ = zip_writer.finish();
    }

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/zip".to_string()),
            (axum::http::header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}.zip\"", dirname)),
        ],
        buf,
    ).into_response()
}

/// POST /api/files/download-zip-multi
pub async fn download_zip_multi(State(_s): State<AppState>, Json(req): Json<MultiZipRequest>) -> impl IntoResponse {
    let mut clean_paths = Vec::new();
    for p in &req.paths {
        let (clean, allowed) = is_path_allowed(p);
        if !allowed {
            return (StatusCode::FORBIDDEN, format!("Access denied: {}", p)).into_response();
        }
        if !clean.exists() {
            return (StatusCode::NOT_FOUND, format!("Not found: {}", p)).into_response();
        }
        clean_paths.push(clean);
    }

    let mut buf = Vec::new();
    {
        let mut zip_writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

        for cp in &clean_paths {
            if cp.is_dir() {
                let parent = cp.parent().unwrap_or(cp);
                add_dir_to_zip(&mut zip_writer, cp, parent, options);
            } else if let Ok(data) = std::fs::read(cp) {
                let name = cp.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                if zip_writer.start_file(name, options).is_ok() {
                    let _ = zip_writer.write_all(&data);
                }
            }
        }
        let _ = zip_writer.finish();
    }

    (
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, "application/zip".to_string()),
            (axum::http::header::CONTENT_DISPOSITION, "attachment; filename=\"download.zip\"".to_string()),
        ],
        buf,
    ).into_response()
}

#[derive(Deserialize)]
pub struct MultiZipRequest {
    paths: Vec<String>,
}

fn add_dir_to_zip<W: Write + std::io::Seek>(
    zip_writer: &mut zip::ZipWriter<W>,
    dir: &Path,
    base: &Path,
    options: zip::write::SimpleFileOptions,
) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                add_dir_to_zip(zip_writer, &path, base, options);
            } else if let Ok(data) = std::fs::read(&path) {
                let rel = path.strip_prefix(base).unwrap_or(&path);
                let name = rel.to_str().unwrap_or("");
                if !name.is_empty() && zip_writer.start_file(name, options).is_ok() {
                    let _ = zip_writer.write_all(&data);
                }
            }
        }
    }
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

            // Remove empty event directory
            if let Ok(remaining) = std::fs::read_dir(&event_dir) {
                if remaining.count() == 0 {
                    let _ = std::fs::remove_dir(&event_dir);
                }
            }
        }
    }

    info!("[files] Snapshot symlink cleanup complete for {}/{}", clip_type, event_name);
}
