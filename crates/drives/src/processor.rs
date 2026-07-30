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
    /// Optional: woken with `notify_one()` whenever `do_process` finishes.
    /// The cloud-uploader subscribes to this so it can run a sweep at the
    /// tail of the archive lifecycle without polling. None on call sites
    /// that don't want a wake (e.g. tests).
    on_complete: Option<Arc<tokio::sync::Notify>>,
    /// Optional hook run at the tail of every process pass, right after
    /// the gap-fill manifest is rewritten. The daemon attaches the
    /// RecentClips backfill-link pass (usb_gadget crate) here — snapshots
    /// on real devices go through the bash make_snapshot.sh, which never
    /// reaches the Rust snapshot code, so the daemon is the only
    /// OTA-updatable place the backfill can run. Blocking-friendly: the
    /// hook is invoked via spawn_blocking.
    after_process: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Processor {
    /// Default clip directory on the Pi.
    pub const DEFAULT_CLIP_DIR: &str = "/mutable/TeslaCam";

    pub fn new(store: Arc<DriveStore>, hub: sentryusb_ws::Hub) -> Self {
        Self::with_on_complete(store, hub, None)
    }

    /// Same as `new`, but with a `Notify` wake-channel attached. The
    /// processor calls `notify.notify_one()` after every successful
    /// `do_process` (whether triggered automatically or via manual
    /// reprocess). Designed to feed the cloud-uploader's sweep loop.
    pub fn with_on_complete(
        store: Arc<DriveStore>,
        hub: sentryusb_ws::Hub,
        on_complete: Option<Arc<tokio::sync::Notify>>,
    ) -> Self {
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
            on_complete,
            after_process: None,
        }
    }

    /// Attach the post-process hook (see the `after_process` field docs).
    /// Must be called before the processor is shared (`Arc::new`).
    pub fn set_after_process(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.after_process = Some(hook);
    }

    /// Test-only: a processor scanning a temp clip tree instead of the
    /// Pi's /mutable/TeslaCam.
    #[cfg(test)]
    pub(crate) fn with_clip_dir_for_test(store: Arc<DriveStore>, clip_dir: String) -> Self {
        let mut p = Self::new(store, sentryusb_ws::Hub::new());
        p.clip_dir = clip_dir;
        p
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
    /// them first.
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

        // Filter out already-processed files. One bulk query into a
        // HashSet rather than a locked `SELECT EXISTS` per file (which
        // is N round-trips on the connection mutex before any work
        // starts). `processed_set` normalizes stored paths to forward
        // slashes and `scan_dir` already pushes forward-slash paths, so
        // membership here matches `is_processed` exactly. Propagate a
        // query failure instead of silently treating everything as
        // unprocessed and re-ingesting the whole tree.
        let processed = self.store.processed_set()?;

        // Continuous-recording timeline for gap-fill hole detection: the
        // disk scan ∪ already-processed keys (clips rotated off disk still
        // anchor historical holes). Event keys are excluded — a gap-filled
        // clip must keep reading as a filled hole, not a new slot.
        let mut recent_ts: Vec<chrono::NaiveDateTime> = files
            .iter()
            .filter_map(|f| crate::grouper::parse_clip_timestamp(f))
            .collect();
        recent_ts.extend(
            processed
                .iter()
                .filter(|k| !crate::grouper::is_event_folder_path(k))
                .filter_map(|k| crate::grouper::parse_clip_timestamp(k)),
        );
        recent_ts.sort();
        recent_ts.dedup();

        // Membership must compare CANONICAL keys: the scan yields physical
        // paths (`RecentClips/YYYY-MM-DD/x.mp4` under the snapshot symlink
        // layout) while the store keys rows by `normalize_path` — which
        // strips that prefix. Comparing the raw scan path would miss every
        // stored row and re-extract the whole RecentClips tree each run.
        // The physical path is kept for the extraction read below.
        let mut unprocessed: Vec<String> = files
            .into_iter()
            .filter(|f| !processed.contains(crate::db::normalize_path(f).as_str()))
            .collect();

        // Gap-fill: append event clips that cover RecentClips holes. They
        // ride the same extraction loop (throttle, progress, resume via
        // processed_files) and land keyed at their real event-folder path.
        match self.scan_event_gap_fill(&recent_ts, &processed) {
            Ok(gap_fill) if !gap_fill.is_empty() => {
                info!(
                    "gap-fill: {} event clip(s) cover RecentClips holes",
                    gap_fill.len()
                );
                unprocessed.extend(gap_fill);
            }
            Ok(_) => {}
            Err(e) => warn!("gap-fill event scan failed: {}", e),
        }

        let total = unprocessed.len();
        let mut routes_found: usize = 0;
        let mut files_with_gps: usize = 0;
        let mut gap_fill_parked_skipped: usize = 0;
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

        // Reused across iterations to avoid one String alloc per clip.
        // Cap matches typical relative path lengths so most clips don't
        // trigger a realloc inside the push loop.
        let mut full_path = String::with_capacity(self.clip_dir.len() + 128);

        for (i, file) in unprocessed.iter().enumerate() {
            {
                let mut status = self.status.lock().await;
                status.current_file = Some(file.clone());
                status.processed_files = i;
            }

            // Build the full path into the reused buffer.
            full_path.clear();
            full_path.push_str(&self.clip_dir);
            full_path.push('/');
            full_path.push_str(file);

            // `add_route` accepts `date_dir: &str` — no need to materialize
            // an owned String just to take a slice of it.
            let date: &str = file.split('/').next().unwrap_or("");
            match extract::extract_gps_from_file(&full_path) {
                // Driving gate for gap-fill event clips: the
                // pre-extraction scan is timestamp-only, so a parked
                // car's sentry clips chained after a drive reach here
                // too. A route row under SavedClips/SentryClips would
                // put the clip in the drive list and its telemetry in
                // the drive map — exactly the parked bloat 60c5602
                // removed — so a no-driving event clip is never stored,
                // only marked processed. (Playback continuity is
                // separate: update_gapfill_manifest's ungated interior
                // scan still restores parked minutes that sit strictly
                // inside a RecentClips hole.)
                Ok(gps)
                    if crate::grouper::is_event_folder_path(file)
                        && !crate::grouper::telemetry_has_driving(
                            &gps.gear_runs,
                            &gps.gear_states,
                            &gps.speeds,
                            gps.raw_park_count,
                            gps.raw_frame_count,
                        ) =>
                {
                    gap_fill_parked_skipped += 1;
                    if let Err(me) = self.store.mark_processed(file) {
                        warn!("failed to mark {} processed: {}", file, me);
                    }
                }
                Ok(gps) => {
                    if !gps.points.is_empty() {
                        files_with_gps += 1;
                    }
                    // add_route both marks the file processed AND writes
                    // the route row (with v2 aggregate columns). Single
                    // transaction per clip — durable on return.
                    match self.store.add_route(
                        file,
                        date,
                        &gps.points,
                        &gps.gear_states,
                        &gps.autopilot_states,
                        &gps.speeds,
                        &gps.accel_positions,
                        gps.raw_park_count,
                        gps.raw_frame_count,
                        &gps.gear_runs,
                        &gps.flag_runs,
                        &gps.gear_run_speed_max,
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
                    // retrying won't change that. Tolerate a mark failure
                    // like the save-error path above does: propagating it
                    // aborted the whole run AND left status.running stuck
                    // true (the early return skipped the reset below), so
                    // the UI showed "processing" forever. The unmarked file
                    // is simply retried next cycle.
                    if let Err(me) = self.store.mark_processed(file) {
                        warn!("failed to mark {} processed: {}", file, me);
                    }
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

            // 10 ms throttle so we don't peg a
            // Pi 4 while the car is still recording clips behind us.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Final checkpoint on the way out.
        let _ = self.store.save();

        // Refresh the gap-fill manifest the snapshot builder reads to
        // cross-link hole-filling event clips back into RecentClips for
        // continuous playback. Rebuilt every pass — from the routes table
        // (driving fills) plus an ungated interior-hole scan (parked fills,
        // e.g. a user save of parked footage) — so it self-heals on
        // already-processed devices as well as fresh deploys. Best-effort —
        // a manifest write failure only costs playback continuity, never
        // drive data.
        if let Err(e) = self.update_gapfill_manifest(&recent_ts) {
            warn!("gap-fill manifest update failed: {}", e);
        }

        // Post-process hook (daemon: RecentClips backfill links). Runs
        // AFTER the manifest rewrite so a fresh manifest is what it reads;
        // fs-heavy, so keep it off the async worker thread.
        if let Some(hook) = self.after_process.clone() {
            if let Err(e) = tokio::task::spawn_blocking(move || hook()).await {
                warn!("after-process hook panicked: {}", e);
            }
        }

        // Mirror drive data to a mounted CIFS/NFS archive — the counterpart
        // of post-archive-process.sh's rsync/rclone sync blocks (the Go
        // server did this in SyncToArchive; the call site was lost in the
        // port). Must run BEFORE `running` flips false below: the
        // post-archive script polls /api/drives/status until running=false
        // and archiveloop unmounts /mnt/archive only after that script
        // exits, so this ordering keeps the mount up for the whole copy.
        // No-op when /mnt/archive isn't mounted (rsync/rclone setups,
        // manual runs, away-mode snapshot processing) and when no new
        // routes landed since the last successful sync.
        {
            let store = self.store.clone();
            match tokio::task::spawn_blocking(move || store.sync_to_archive()).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => warn!("drive-data archive sync failed: {}", e),
                Err(e) => warn!("drive-data archive sync task failed: {}", e),
            }
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
            "routes_found": routes_found,
            "files_with_gps": files_with_gps,
            "errorCount": error_count,
            "errors": errors,
        }));

        info!(
            "processing complete: {} files processed, {} routes found, {} with GPS, {} errors",
            total, routes_found, files_with_gps, error_count
        );
        if gap_fill_parked_skipped > 0 {
            info!(
                "gap-fill: {} event clip(s) skipped by the driving gate (parked pre-roll)",
                gap_fill_parked_skipped
            );
        }

        // Wake the cloud-uploader if it's listening. Cheap; idempotent
        // (notify_one with no waiter is a no-op).
        if let Some(n) = &self.on_complete {
            n.notify_one();
        }
        Ok(())
    }

    /// Recursively scan for -front.mp4 files.
    fn scan_dir(&self, dir: &std::path::Path, files: &mut Vec<String>) -> Result<()> {
        self.scan_dir_inner(dir, files, true)
    }

    fn scan_dir_inner(
        &self,
        dir: &std::path::Path,
        files: &mut Vec<String>,
        skip_event_dirs: bool,
    ) -> Result<()> {
        let entries = std::fs::read_dir(dir)?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                // Skip Tesla event folders. SavedClips contains user-saved
                // clips that are byte-identical to RecentClips entries
                // (different paths the grouper's path-based dedup can't
                // catch). SentryClips contains parked Sentry-mode recordings
                // that the gear-state splitter emits as spurious "drives"
                // bordering an actual trip. Matches Sentry-Drive's
                // discoverFrontCameraFiles (process.js:91-94). The gap-fill
                // scan (scan_event_gap_fill) walks these deliberately.
                if skip_event_dirs {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if is_event_folder(name) {
                            continue;
                        }
                    }
                }
                self.scan_dir_inner(&path, files, skip_event_dirs)?;
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with("-front.mp4") {
                    // Gap-fill playback aliases live under RecentClips but
                    // point back into SavedClips/SentryClips. They must not
                    // re-enter the drive timeline as genuine recent clips.
                    if skip_event_dirs && is_event_crosslink(&path) {
                        continue;
                    }
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

    /// Find Saved/Sentry event clips that fill RecentClips gaps: the
    /// continuous recording gapped mid-drive (interior hole), or the drive
    /// ran past its last RecentClips clip / started before its first one
    /// and the footage only made an event folder's ~10-minute pre-roll
    /// (trailing/leading chain). Returns unprocessed relative paths (kept
    /// at their real event-folder location — no copies or symlinks).
    ///
    /// This is a timestamp-bounded SUPERSET of the final fill set: without
    /// SEI in hand, a parked car's event clips chained shortly after a
    /// drive can qualify here too. They are extracted once, rejected by
    /// the driving gate in `do_process` (never stored, only marked
    /// processed), and the chain cap (grouper::GAP_FILL_MAX_MS from the
    /// nearest RecentClips clip) bounds that one-time waste to ~30 clips
    /// per drive boundary on a sentry-heavy install. Isolated event
    /// clusters with no adjacent continuous footage never qualify at all.
    /// One clip per missing timestamp (grouper::select_gap_fill_events
    /// picks the winner).
    fn scan_event_gap_fill(
        &self,
        recent_sorted_ts: &[chrono::NaiveDateTime],
        processed: &std::collections::HashSet<String>,
    ) -> Result<Vec<String>> {
        let event_files = self.scan_event_files()?;

        let cands: Vec<(chrono::NaiveDateTime, &str)> = event_files
            .iter()
            .filter(|f| !processed.contains(crate::db::normalize_path(f).as_str()))
            .filter_map(|f| {
                crate::grouper::parse_clip_timestamp(f).map(|ts| (ts, f.as_str()))
            })
            .collect();
        Ok(crate::grouper::select_gap_fill_events(recent_sorted_ts, &cands)
            .into_iter()
            .map(|i| cands[i].1.to_string())
            .collect())
    }

    /// Walk `SavedClips/` and `SentryClips/` under the clip dir and return
    /// every clip file (relative paths, sorted). Shared by the gap-fill
    /// candidate scan and the playback-manifest interior scan — the latter
    /// must see ALL event clips, including already-processed ones.
    fn scan_event_files(&self) -> Result<Vec<String>> {
        let mut event_files: Vec<String> = Vec::new();
        for sub in ["SavedClips", "SentryClips"] {
            let dir = std::path::Path::new(&self.clip_dir).join(sub);
            if dir.is_dir() {
                self.scan_dir_inner(&dir, &mut event_files, false)?;
            }
        }
        event_files.sort();
        Ok(event_files)
    }

    /// Rewrite `<clip_dir>/../.gapfill_recent_links` — the manifest of
    /// event-clip timestamps the snapshot builder cross-links back into
    /// RecentClips for continuous drive playback. Union of two sources:
    ///
    /// * the routes table (every Saved/Sentry route file is, by
    ///   construction, a driving hole-fill — interior, trailing or
    ///   leading), which converges regardless of when the fill happened;
    /// * a fresh event-folder scan selecting clips strictly INSIDE a
    ///   RecentClips hole, WITHOUT the driving gate: a user save moves
    ///   parked pre-roll minutes out of RecentClips too, and those must
    ///   still play back even though they never become routes. Scanned
    ///   without the processed filter — gate-rejected clips are already
    ///   marked processed and would otherwise never be reconsidered.
    ///
    /// Skips the write when unchanged to avoid churn.
    fn update_gapfill_manifest(&self, recent_sorted_ts: &[chrono::NaiveDateTime]) -> Result<()> {
        let manifest = match std::path::Path::new(&self.clip_dir).parent() {
            Some(p) => p.join(".gapfill_recent_links"),
            None => return Ok(()),
        };

        // Reduce each gap-fill file to its YYYY-MM-DD_HH-MM-SS stamp
        // (shared by every camera of that minute), sorted + deduped.
        fn stamp_of(f: &str) -> Option<String> {
            f.rsplit('/')
                .next()
                .filter(|b| b.len() >= 19)
                .map(|b| b[..19].to_string())
        }
        let mut stamps: Vec<String> = self
            .store
            .gap_fill_files()?
            .iter()
            .filter_map(|f| stamp_of(f))
            .collect();

        let event_files = self.scan_event_files()?;
        let cands: Vec<(chrono::NaiveDateTime, &str)> = event_files
            .iter()
            .filter_map(|f| {
                crate::grouper::parse_clip_timestamp(f).map(|ts| (ts, f.as_str()))
            })
            .collect();
        stamps.extend(
            crate::grouper::select_interior_fill(recent_sorted_ts, &cands)
                .into_iter()
                .filter_map(|i| stamp_of(cands[i].1)),
        );

        stamps.sort();
        stamps.dedup();

        let body = if stamps.is_empty() {
            String::new()
        } else {
            format!("{}\n", stamps.join("\n"))
        };

        // No-op when the content is identical (the common case once the
        // gap is filled) so we don't rewrite the file every archive cycle.
        if std::fs::read_to_string(&manifest).unwrap_or_default() == body {
            return Ok(());
        }
        std::fs::write(&manifest, body)?;
        info!(
            "gap-fill manifest: {} clip timestamp(s) flagged for RecentClips playback",
            stamps.len()
        );
        Ok(())
    }
}

fn is_event_crosslink(path: &std::path::Path) -> bool {
    let Ok(target) = std::fs::read_link(path) else {
        return false;
    };
    target.components().any(|component| {
        let name = component.as_os_str();
        name == "SavedClips" || name == "SentryClips"
    })
}

/// Directory names that hold Tesla event clips (Sentry triggers + user
/// saves). Excluded from drive discovery to keep parked recordings and
/// duplicate-of-RecentClips entries out of the grouper.
pub(crate) fn is_event_folder(name: &str) -> bool {
    name == "SavedClips" || name == "SentryClips"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn recent_scan_excludes_event_gapfill_crosslinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::TempDir::new().unwrap();
        let clip_dir = root.path().join("TeslaCam");
        let recents = clip_dir.join("RecentClips/2026-07-15");
        let saved = clip_dir.join("SavedClips/2026-07-15_04-59-30");
        std::fs::create_dir_all(&recents).unwrap();
        std::fs::create_dir_all(&saved).unwrap();

        let real_recent = root.path().join("real-recent-front.mp4");
        let event_clip = saved.join("2026-07-15_04-50-00-front.mp4");
        std::fs::write(&real_recent, b"recent").unwrap();
        std::fs::write(&event_clip, b"event").unwrap();
        symlink(
            &real_recent,
            recents.join("2026-07-15_04-49-00-front.mp4"),
        )
        .unwrap();
        symlink(
            &event_clip,
            recents.join("2026-07-15_04-50-00-front.mp4"),
        )
        .unwrap();

        let processor = Processor::with_clip_dir_for_test(
            Arc::new(crate::db::DriveStore::open_memory().unwrap()),
            clip_dir.to_string_lossy().to_string(),
        );
        let mut files = Vec::new();
        processor.scan_dir(&clip_dir, &mut files).unwrap();
        files.sort();

        assert_eq!(
            files,
            vec!["RecentClips/2026-07-15/2026-07-15_04-49-00-front.mp4"],
            "event-targeting playback aliases must not become timeline anchors or routes"
        );
    }

    /// A user save moves parked minutes out of RecentClips into
    /// SavedClips. Those clips never pass the driving gate (no routes),
    /// and may already be marked processed from a previous rejected pass —
    /// the manifest must still list them so playback stays continuous.
    #[test]
    fn manifest_includes_parked_saved_clips_in_interior_hole() {
        let root = tempfile::TempDir::new().unwrap();
        let clip_dir = root.path().join("TeslaCam");
        let event_dir = clip_dir.join("SavedClips/2026-07-15_04-59-30");
        std::fs::create_dir_all(&event_dir).unwrap();
        // The moved minutes (interior hole 04:49 → 05:00) …
        for name in [
            "2026-07-15_04-50-00-front.mp4",
            "2026-07-15_04-55-00-front.mp4",
        ] {
            std::fs::write(event_dir.join(name), b"x").unwrap();
        }
        // … plus an event clip duplicating an occupied recent slot.
        std::fs::write(event_dir.join("2026-07-15_05-00-03-front.mp4"), b"x").unwrap();

        let store = Arc::new(crate::db::DriveStore::open_memory().unwrap());
        // One clip already marked processed (rejected by the driving gate
        // on an earlier pass) — must not keep it out of the manifest.
        store
            .mark_processed("SavedClips/2026-07-15_04-59-30/2026-07-15_04-50-00-front.mp4")
            .unwrap();

        let processor = Processor::with_clip_dir_for_test(
            store,
            clip_dir.to_string_lossy().to_string(),
        );

        let recent_ts = vec![
            chrono::NaiveDateTime::parse_from_str("2026-07-15 04:49:09", "%Y-%m-%d %H:%M:%S")
                .unwrap(),
            chrono::NaiveDateTime::parse_from_str("2026-07-15 05:00:03", "%Y-%m-%d %H:%M:%S")
                .unwrap(),
        ];
        processor.update_gapfill_manifest(&recent_ts).unwrap();

        let manifest =
            std::fs::read_to_string(root.path().join(".gapfill_recent_links")).unwrap();
        let stamps: Vec<&str> = manifest.lines().collect();
        assert_eq!(
            stamps,
            vec!["2026-07-15_04-50-00", "2026-07-15_04-55-00"],
            "interior parked fills listed; the occupied-slot twin excluded"
        );
    }

    /// The after-process hook is the daemon's only path to the RecentClips
    /// backfill (device snapshots run the bash script, not the Rust
    /// snapshot code) — it must fire at the tail of every process pass,
    /// even a pass that found nothing to extract.
    #[tokio::test]
    async fn after_process_hook_fires_on_process_pass() {
        let root = tempfile::TempDir::new().unwrap();
        let clip_dir = root.path().join("TeslaCam");
        std::fs::create_dir_all(&clip_dir).unwrap();

        let store = Arc::new(crate::db::DriveStore::open_memory().unwrap());
        let mut processor = Processor::with_clip_dir_for_test(
            store,
            clip_dir.to_string_lossy().to_string(),
        );
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = fired.clone();
        processor.set_after_process(Arc::new(move || {
            flag.store(true, Ordering::SeqCst);
        }));

        processor.process_new().await.unwrap();
        assert!(fired.load(Ordering::SeqCst), "hook must run after the pass");
    }

    #[test]
    fn test_is_event_folder() {
        assert!(is_event_folder("SavedClips"));
        assert!(is_event_folder("SentryClips"));
        assert!(!is_event_folder("RecentClips"));
        assert!(!is_event_folder("2026-05-17"));
        assert!(!is_event_folder("2026-05-17_18-47-59"));
        assert!(!is_event_folder(""));
        // Case sensitive — Tesla's folder names are exact.
        assert!(!is_event_folder("savedclips"));
        assert!(!is_event_folder("SAVEDCLIPS"));
    }
}
