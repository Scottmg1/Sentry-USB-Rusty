//! Clip processor — scans the TeslaCam tree, extracts GPS, feeds the DB.
//!
//! Incremental save semantics match Go's processor.go:
//!   * Each file is marked processed via `add_route` (which opens a short
//!     transaction per clip — already durable after each call).
//!   * A passive WAL checkpoint fires every `SAVE_EVERY` files so the
//!     `-wal` file doesn't grow unbounded on long reprocess runs.
//!   * Per-file errors are collected and broadcast via the WebSocket hub
//!     so the web UI can show which clips failed and why.
//!   * 10ms throttle between files keeps the processor from pegging a Pi
//!     4 at 100% CPU while the car is recording.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::DriveStore;
use crate::extract;
use crate::types::ProcessingStatus;

/// Fire a `PRAGMA wal_checkpoint(PASSIVE)` every N files processed. Keeps
/// the `-wal` file bounded during long processing runs without blocking
/// other readers/writers.
const SAVE_EVERY: usize = 50;

/// Maximum per-file error messages retained for UI display. Anything
/// past this is counted but not individually surfaced — keeps memory
/// bounded on pathological datasets (corrupted SD card with thousands
/// of unreadable files).
const MAX_ERROR_MESSAGES: usize = 200;

/// Orchestrates GPS extraction from TeslaCam clip files.
pub struct Processor {
    store: Arc<DriveStore>,
    hub: sentryusb_ws::Hub,
    running: AtomicBool,
    status: Mutex<ProcessingStatus>,
    clip_dir: String,
}

impl Processor {
    /// Default clip directory on the Pi.
    pub const DEFAULT_CLIP_DIR: &str = "/mutable/TeslaCam";

    pub fn new(store: Arc<DriveStore>, hub: sentryusb_ws::Hub) -> Self {
        Processor {
            store,
            hub,
            running: AtomicBool::new(false),
            status: Mutex::new(ProcessingStatus {
                running: false,
                total_files: 0,
                processed_files: 0,
                current_file: None,
            }),
            clip_dir: Self::DEFAULT_CLIP_DIR.to_string(),
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub async fn get_status(&self) -> ProcessingStatus {
        self.status.lock().await.clone()
    }

    /// Start processing new (unprocessed) clip files.
    pub async fn process_new(&self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            anyhow::bail!("processing already in progress");
        }

        let result = self.do_process(false).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    /// Reprocess all clip files. Just clears `processed_files`; routes
    /// are upserted in place by `add_route`, so there's no need to wipe
    /// them first (matches Go's `ClearProcessedForReprocess` behavior).
    pub async fn reprocess_all(&self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            anyhow::bail!("processing already in progress");
        }

        self.store.clear_processed_for_reprocess()?;
        let result = self.do_process(true).await;
        self.running.store(false, Ordering::SeqCst);
        result
    }

    async fn do_process(&self, _reprocess: bool) -> Result<()> {
        // Scan for -front.mp4 files
        let clip_dir = std::path::Path::new(&self.clip_dir);
        if !clip_dir.exists() {
            info!("clip directory does not exist: {}", self.clip_dir);
            return Ok(());
        }

        let mut files: Vec<String> = Vec::new();
        self.scan_dir(clip_dir, &mut files)?;
        files.sort();

        // Filter out already-processed files
        let unprocessed: Vec<String> = files
            .into_iter()
            .filter(|f| !self.store.is_processed(f).unwrap_or(true))
            .collect();

        let total = unprocessed.len();
        let mut routes_found: usize = 0;
        let mut files_with_gps: usize = 0;
        let mut errors: Vec<String> = Vec::new();
        let mut error_count: usize = 0;
        info!("found {} unprocessed clip files", total);

        {
            let mut status = self.status.lock().await;
            status.running = true;
            status.total_files = total;
            status.processed_files = 0;
            status.current_file = None;
        }

        self.hub.broadcast("drive_process", &serde_json::json!({
            "status": "started",
            "total": total,
        }));

        for (i, file) in unprocessed.iter().enumerate() {
            {
                let mut status = self.status.lock().await;
                status.current_file = Some(file.clone());
                status.processed_files = i;
            }

            // Extract GPS from the file.
            let full_path = format!("{}/{}", self.clip_dir, file);
            let date = file.split('/').next().unwrap_or("").to_string();
            match extract::extract_gps_from_file(&full_path) {
                Ok(gps) => {
                    if !gps.points.is_empty() {
                        files_with_gps += 1;
                    }
                    // add_route both marks the file processed AND writes
                    // the route row (with v2 aggregate columns). Single
                    // transaction per clip — durable on return.
                    match self.store.add_route(
                        file,
                        &date,
                        &gps.points,
                        &gps.gear_states,
                        &gps.autopilot_states,
                        &gps.speeds,
                        &gps.accel_positions,
                        gps.raw_park_count,
                        gps.raw_frame_count,
                        &gps.gear_runs,
                    ) {
                        Ok(()) => routes_found += 1,
                        Err(e) => {
                            warn!("failed to save route for {}: {}", file, e);
                            error_count += 1;
                            if errors.len() < MAX_ERROR_MESSAGES {
                                errors.push(format!("{}: save failed — {}", file, e));
                            }
                            // Still mark processed so we don't retry forever.
                            let _ = self.store.mark_processed(file);
                        }
                    }
                }
                Err(e) => {
                    warn!("failed to extract GPS from {}: {}", file, e);
                    error_count += 1;
                    if errors.len() < MAX_ERROR_MESSAGES {
                        errors.push(format!("{}: extract failed — {}", file, e));
                    }
                    // Mark processed anyway — clip has no extractable GPS,
                    // retrying won't change that.
                    self.store.mark_processed(file)?;
                }
            }

            // Broadcast progress every 10 files.
            if (i + 1) % 10 == 0 || i + 1 == total {
                self.hub.broadcast("drive_process", &serde_json::json!({
                    "status": "progress",
                    "processed": i + 1,
                    "total": total,
                    "errorCount": error_count,
                }));
            }

            // Passive WAL checkpoint every SAVE_EVERY files so the WAL
            // doesn't grow unbounded on a long reprocess run.
            if (i + 1) % SAVE_EVERY == 0 {
                if let Err(e) = self.store.save() {
                    warn!("processor WAL checkpoint failed: {}", e);
                }
            }

            // 10 ms throttle — matches Go processor.go so we don't peg a
            // Pi 4 while the car is still recording clips behind us.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Final checkpoint on the way out.
        let _ = self.store.save();

        {
            let mut status = self.status.lock().await;
            status.running = false;
            status.processed_files = total;
            status.current_file = None;
        }

        self.hub.broadcast("drive_process", &serde_json::json!({
            "status": "complete",
            "processed": total,
            "total": total,
            "routes_found": routes_found,
            "files_with_gps": files_with_gps,
            "errorCount": error_count,
            "errors": errors,
        }));

        info!(
            "processing complete: {} files processed, {} routes found, {} with GPS, {} errors",
            total, routes_found, files_with_gps, error_count
        );
        Ok(())
    }

    /// Recursively scan for -front.mp4 files.
    fn scan_dir(&self, dir: &std::path::Path, files: &mut Vec<String>) -> Result<()> {
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                self.scan_dir(&path, files)?;
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with("-front.mp4") {
                    // Store relative path from clip_dir
                    if let Ok(rel) = path.strip_prefix(&self.clip_dir) {
                        if let Some(rel_str) = rel.to_str() {
                            files.push(rel_str.replace('\\', "/"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
