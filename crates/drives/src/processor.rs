// Directory scanning, GPS extraction orchestration, progress reporting.
// Will be fully implemented in Phase 2 Task 10.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::db::DriveStore;
use crate::extract;
use crate::types::{ProcessingStatus, Route};

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

    /// Reprocess all clip files (clear processed list first).
    pub async fn reprocess_all(&self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            anyhow::bail!("processing already in progress");
        }

        self.store.clear_processed()?;
        self.store.clear_routes()?;
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

            // Extract GPS from the file
            let full_path = format!("{}/{}", self.clip_dir, file);
            match extract::extract_gps_from_file(&full_path) {
                Ok(gps) => {
                    let route = Route {
                        file: file.clone(),
                        date: file.split('/').next().unwrap_or("").to_string(),
                        points: gps.points,
                        gear_states: gps.gear_states,
                        autopilot_states: gps.autopilot_states,
                        speeds: gps.speeds,
                        accel_positions: gps.accel_positions,
                        raw_park_count: gps.raw_park_count,
                        raw_frame_count: gps.raw_frame_count,
                        gear_runs: gps.gear_runs,
                    };
                    if let Err(e) = self.store.upsert_route(&route) {
                        warn!("failed to save route for {}: {}", file, e);
                    }
                }
                Err(e) => {
                    warn!("failed to extract GPS from {}: {}", file, e);
                }
            }

            self.store.mark_processed(file)?;

            // Broadcast progress every 10 files
            if (i + 1) % 10 == 0 || i + 1 == total {
                self.hub.broadcast("drive_process", &serde_json::json!({
                    "status": "progress",
                    "processed": i + 1,
                    "total": total,
                }));
            }

            // Yield to other tasks (10ms throttle like Go version)
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

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
        }));

        info!("processing complete: {} files processed", total);
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
