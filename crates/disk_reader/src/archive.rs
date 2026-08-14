//! The SentryUSB footage archive: snapshots on the `backingfiles` XFS
//! partition, merged into one virtual TeslaCam tree.
//!
//! Layout (written by crates/usb_gadget/src/snapshot.rs on the Pi):
//!   /backingfiles/cam_disk.bin                 — live gadget image (newest data)
//!   /backingfiles/snapshots/snap-NNNNNN/snap.bin      — reflinked snapshot
//!   /backingfiles/snapshots/snap-NNNNNN/snap.bin.toc  — "<size> <relpath>" lines
//!
//! Each *.bin is an MBR image whose first partition holds an exFAT/FAT32
//! filesystem with the Tesla-written TeslaCam tree. The merged view is the
//! union of file paths across snapshots, newest snapshot winning.

use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail, Context, Result};

use crate::{
    camfs::{CamEntry, CamFs},
    device::{Disk, Window},
    gpt,
    mbr,
    xfs::fs::{XfsFile, XfsFs},
};

const SNAPSHOTS_DIR: &str = "snapshots";
const LIVE_IMAGE: &str = "cam_disk.bin";

/// A CAM filesystem stacked over the partition window of a snapshot image
/// inside the XFS volume.
type Cam = CamFs<SubFile<XfsFile<Window>>>;

/// SAFETY: `fatfs::FileSystem` is `!Send` only because its `FsOptions`
/// holds `&'static dyn OemCpConverter/TimeProvider` without a `Sync`
/// bound. We construct it exclusively with `FsOptions::new()` defaults,
/// whose implementations (`LossyOemCpConverter`, `DefaultTimeProvider`)
/// are stateless unit structs, so moving the filesystem between threads
/// is sound. Everything else in the stack is naturally `Send`.
struct SendCam(Cam);
unsafe impl Send for SendCam {}

/// One source of clips: a snapshot's snap.bin or the live cam_disk.bin.
/// `ord` sorts oldest-first; the live image is always newest.
#[derive(Clone, Debug)]
pub struct Source {
    /// `snap-NNNNNN` slot number. A deterministic TIE-BREAK only — never
    /// an age signal: numbering restarts beneath stale leftovers after a
    /// reflash. `u64::MAX` marks the live image.
    pub slot: u64,
    /// When this image was frozen (`snap.bin` inode mtime). `None` when
    /// the inode could not be read; such a source sorts before every
    /// known-age one so it can never win a merge conflict, while its
    /// uniquely-recoverable files still surface.
    pub mtime: Option<std::time::SystemTime>,
    /// Path of the image inside the XFS volume, e.g. "snapshots/snap-000001/snap.bin".
    pub image_path: PathBuf,
    pub label: String,
    pub toc: Option<Vec<TocEntry>>,
}

/// Order snapshot sources oldest-first for [`Archive::build_merged_tree`],
/// which overwrites as it walks so the LAST source wins each path.
///
/// Key is `(mtime, slot)`. Rust orders `None < Some`, which is exactly the
/// policy we want: unknown-age snapshots first (among themselves by slot),
/// then known ones by when they were frozen, with the slot breaking exact
/// ties (e.g. a restore that flattened timestamps).
///
/// The live image is NOT sorted here — it is appended afterwards and
/// always wins, because "live" is a semantic role rather than a timestamp
/// claim; a rolled-back clock must not let a frozen snapshot outrank the
/// image the car is currently writing.
///
/// Deliberately does NOT reuse the Pi's highest-slot guard
/// (`space.rs::releasable`): that guard exists so eviction never deletes
/// a past-dated brand-new snapshot, and applying it to merge order would
/// rank stale high-numbered leftovers last — recreating this very bug.
fn sort_snapshot_sources(sources: &mut [Source]) {
    sources.sort_by(|a, b| (a.mtime, a.slot).cmp(&(b.mtime, b.slot)));
}

#[derive(Clone, Debug)]
pub struct TocEntry {
    pub size: u64,
    /// Relative to the CAM filesystem root, e.g. "TeslaCam/SavedClips/…/x.mp4".
    pub path: String,
}

#[derive(Clone, Debug)]
pub struct FileMeta {
    /// Index into `Archive::sources`.
    pub source: usize,
    pub size: u64,
    /// For a gap-fill alias (an event clip surfaced under a virtual
    /// `RecentClips/` path): the clip's REAL path inside the source's CAM
    /// filesystem. `None` for ordinary entries, whose merged-tree key IS
    /// the CAM path.
    pub real: Option<String>,
}

pub struct Archive {
    xfs: Arc<Mutex<XfsFs<Window>>>,
    pub sources: Vec<Source>,
    /// Merged virtual tree: CAM-relative path -> winning source + size.
    files: BTreeMap<String, FileMeta>,
    /// Lazily opened CAM filesystems, keyed by source index. The inner
    /// Mutex makes the fatfs variant (RefCell inside) shareable across the
    /// HTTP server's threads.
    open_cams: Mutex<HashMap<usize, Arc<Mutex<SendCam>>>>,
    pub warnings: Vec<String>,
}

pub struct ProbeResult {
    pub backingfiles: gpt::GptPartition,
}

/// Check whether `disk` looks like a SentryUSB SSD and return the
/// backingfiles partition if so.
pub fn probe(disk: &Disk) -> Result<ProbeResult> {
    let mut whole = disk.whole();
    let parts = gpt::read_partitions(&mut whole).context("reading GPT")?;
    let backingfiles = parts
        .iter()
        .find(|p| {
            if p.name.eq_ignore_ascii_case("backingfiles") {
                return true;
            }
            // Label lives in the XFS superblock too; GPT name is what
            // parted set. Fall back to sniffing XFS magic on p2.
            let mut w = disk.window(p.start, p.len.min(4096));
            let mut magic = [0u8; 4];
            use std::io::{Seek, SeekFrom};
            w.seek(SeekFrom::Start(0)).ok();
            w.read_exact(&mut magic).ok();
            &magic == b"XFSB" && p.index == 2
        })
        .cloned()
        .ok_or_else(|| anyhow!("no backingfiles/XFS partition found — not a SentryUSB disk"))?;
    Ok(ProbeResult { backingfiles })
}

impl Archive {
    pub fn open(disk: &Disk) -> Result<Self> {
        Self::open_with_progress(disk, &|_| {})
    }

    /// Like [`Archive::open`], reporting human-readable progress as it
    /// enumerates and indexes snapshots (opening a large archive over USB
    /// takes a while; callers surface these messages in their UI).
    pub fn open_with_progress(disk: &Disk, progress: &dyn Fn(String)) -> Result<Self> {
        progress("Reading partition table".to_string());
        let probe = probe(disk)?;
        let part = &probe.backingfiles;
        let window = disk.window(part.start, part.len);
        let len = window.len();
        progress("Opening the backingfiles filesystem".to_string());
        let xfs = XfsFs::open(window, len).context("opening backingfiles XFS")?;
        let xfs = Arc::new(Mutex::new(xfs));
        let mut warnings = Vec::new();

        // Enumerate sources: snapshots oldest-first BY IMAGE MTIME (slot
        // numbers are not time-monotonic — see sort_snapshot_sources),
        // then the live image last.
        let mut sources = Vec::new();
        {
            let mut fs = xfs.lock().unwrap();
            let snapdirs = match fs.read_dir(Path::new(SNAPSHOTS_DIR)) {
                Ok(entries) => entries,
                Err(e) => {
                    warnings.push(format!("no snapshots directory readable: {e:#}"));
                    Vec::new()
                }
            };
            for d in snapdirs {
                let Some(numpart) = d.name.strip_prefix("snap-") else {
                    continue;
                };
                let Ok(num) = numpart.parse::<u64>() else {
                    continue;
                };
                let dir = PathBuf::from(SNAPSHOTS_DIR).join(&d.name);
                let image_path = dir.join("snap.bin");
                if fs.file_size(&image_path).is_err() {
                    warnings.push(format!("{}: no snap.bin, skipping", d.name));
                    continue;
                }
                // Age comes from the image's mtime, not the slot number.
                let mtime = match fs.file_mtime(&image_path) {
                    Ok(t) => Some(t),
                    Err(e) => {
                        warnings.push(format!(
                            "{}: unreadable snap.bin timestamp ({e:#}); treating as oldest so it \
                             cannot override newer snapshots",
                            d.name
                        ));
                        None
                    }
                };
                sources.push(Source {
                    slot: num,
                    mtime,
                    label: d.name.clone(),
                    toc: None,
                    image_path,
                });
            }
            sort_snapshot_sources(&mut sources);
            // The live image is the newest source of all — appended after
            // the sort so it always wins, whatever the clock said.
            if fs.file_size(Path::new(LIVE_IMAGE)).is_ok() {
                sources.push(Source {
                    slot: u64::MAX,
                    mtime: None,
                    label: "live".to_string(),
                    toc: None,
                    image_path: PathBuf::from(LIVE_IMAGE),
                });
            }
            if sources.is_empty() {
                bail!("no snapshots and no cam_disk.bin on backingfiles — nothing to browse");
            }
        }

        let mut archive = Archive {
            xfs,
            sources,
            files: BTreeMap::new(),
            open_cams: Mutex::new(HashMap::new()),
            warnings,
        };
        archive.load_tocs(progress);
        progress("Merging snapshots".to_string());
        archive.build_merged_tree()?;
        let aliased = alias_gapfill_recents(&mut archive.files);
        if aliased > 0 {
            progress(format!(
                "Restored {aliased} clip(s) into RecentClips from event folders"
            ));
        }
        Ok(archive)
    }

    fn load_tocs(&mut self, progress: &dyn Fn(String)) {
        let total = self.sources.len();
        for (i, s) in self.sources.iter_mut().enumerate() {
            progress(format!("Indexing snapshot {} of {total}", i + 1));
            let toc_path = PathBuf::from(format!("{}.toc", s.image_path.display()));
            match XfsFile::open(&self.xfs, &toc_path) {
                Ok(mut f) => {
                    let mut text = String::new();
                    if f.read_to_string(&mut text).is_ok() {
                        s.toc = Some(parse_toc(&text));
                    }
                }
                Err(_) => {
                    // Live image and interrupted snapshots have no TOC.
                }
            }
        }
    }

    fn build_merged_tree(&mut self) -> Result<()> {
        // Oldest to newest; later inserts overwrite, so newest wins.
        let source_data: Vec<(usize, Option<Vec<TocEntry>>)> = self
            .sources
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.toc.clone()))
            .collect();
        for (idx, toc) in source_data {
            let entries = match toc {
                Some(toc) => toc,
                None => match self.walk_cam(idx) {
                    Ok(entries) => entries,
                    Err(e) => {
                        self.warnings.push(format!(
                            "{}: unreadable CAM filesystem ({e:#}), skipping",
                            self.sources[idx].label
                        ));
                        continue;
                    }
                },
            };
            for e in entries {
                self.files.insert(
                    e.path,
                    FileMeta {
                        source: idx,
                        size: e.size,
                        real: None,
                    },
                );
            }
        }
        if self.files.is_empty() {
            bail!("archive contains no files");
        }
        Ok(())
    }

    /// Recursively list every file of a source's CAM filesystem (fallback
    /// when there is no .toc).
    fn walk_cam(&self, source: usize) -> Result<Vec<TocEntry>> {
        let cam = self.cam(source)?;
        let cam = cam.lock().unwrap();
        let mut out = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(dir) = stack.pop() {
            for entry in cam.0.read_dir(&dir)? {
                let path = if dir.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{dir}/{}", entry.name)
                };
                if entry.is_dir {
                    stack.push(path);
                } else {
                    out.push(TocEntry {
                        size: entry.size,
                        path,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Lazily open (and cache) the CAM filesystem of a source.
    fn cam(&self, source: usize) -> Result<Arc<Mutex<SendCam>>> {
        if let Some(c) = self.open_cams.lock().unwrap().get(&source) {
            return Ok(Arc::clone(c));
        }
        let s = &self.sources[source];
        let mut image = XfsFile::open(&self.xfs, &s.image_path)
            .with_context(|| format!("open {}", s.image_path.display()))?;
        let part = mbr::first_partition(&mut image)
            .with_context(|| format!("{}: bad image MBR", s.label))?;
        // Re-open so CamFs owns a fresh file positioned via its own window.
        let image = XfsFile::open(&self.xfs, &s.image_path)?;
        let part_view = SubFile::new(image, part.start, part.len);
        let cam = Arc::new(Mutex::new(SendCam(
            CamFs::open(part_view).with_context(|| format!("{}: CAM fs", s.label))?,
        )));
        self.open_cams
            .lock()
            .unwrap()
            .insert(source, Arc::clone(&cam));
        Ok(cam)
    }

    /// List a directory of the merged virtual tree ("" = root).
    pub fn read_dir(&self, dir: &str) -> Vec<CamEntry> {
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{}/", dir.trim_matches('/'))
        };
        let mut dirs = BTreeMap::new();
        let mut files = Vec::new();
        for (path, meta) in self.files.range(prefix.clone()..) {
            let Some(rest) = path.strip_prefix(&prefix) else {
                break;
            };
            match rest.split_once('/') {
                Some((child, _)) => {
                    dirs.entry(child.to_string()).or_insert(());
                }
                None => files.push(CamEntry {
                    name: rest.to_string(),
                    is_dir: false,
                    size: meta.size,
                }),
            }
        }
        let mut out: Vec<CamEntry> = dirs
            .into_keys()
            .map(|name| CamEntry {
                name,
                is_dir: true,
                size: 0,
            })
            .collect();
        out.extend(files);
        out
    }

    pub fn stat(&self, path: &str) -> Option<&FileMeta> {
        self.files.get(path.trim_matches('/'))
    }

    /// Read a byte range of a file in the merged tree.
    pub fn read_range(&self, path: &str, offset: u64, len: usize) -> Result<Vec<u8>> {
        let path = path.trim_matches('/');
        let meta = self
            .files
            .get(path)
            .ok_or_else(|| anyhow!("no such file: {path}"))?;
        // Gap-fill aliases live at a virtual RecentClips path; the bytes
        // are read from the clip's real event-folder location.
        let cam_path = meta.real.as_deref().unwrap_or(path);
        let cam = self.cam(meta.source)?;
        let result = cam.lock().unwrap().0.read_file_range(cam_path, offset, len);
        result
    }

    /// Read a whole (small) file.
    pub fn read_file(&self, path: &str) -> Result<Vec<u8>> {
        let path = path.trim_matches('/');
        let meta = self
            .files
            .get(path)
            .ok_or_else(|| anyhow!("no such file: {path}"))?;
        let cam_path = meta.real.as_deref().unwrap_or(path);
        let cam = self.cam(meta.source)?;
        let result = cam.lock().unwrap().0.read_file(cam_path);
        result
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    fn source_by_label(&self, label: &str) -> Result<&Source> {
        self.sources
            .iter()
            .find(|s| s.label == label)
            .ok_or_else(|| anyhow!("no source labeled {label} (see probe output)"))
    }

    /// Debug: the raw exFAT/FAT32 boot sector of a source's CAM partition,
    /// plus the MBR partition type.
    pub fn debug_cam_boot(&self, label: &str) -> Result<(Vec<u8>, u8)> {
        let s = self.source_by_label(label)?;
        let mut image = XfsFile::open(&self.xfs, &s.image_path)?;
        let part = mbr::first_partition(&mut image)?;
        let mut view = SubFile::new(XfsFile::open(&self.xfs, &s.image_path)?, part.start, part.len);
        let mut boot = vec![0u8; 512];
        use std::io::Read as _;
        view.read_exact(&mut boot)?;
        Ok((boot, part.part_type))
    }

    /// Debug: the entry-type byte of the first `n` root-directory entries
    /// of a source's exFAT CAM filesystem (assumes root dir is contiguous
    /// from its first cluster — true for freshly-mkfs'd images).
    pub fn debug_root_entry_types(&self, label: &str, n: usize) -> Result<Vec<u8>> {
        let (boot, _) = self.debug_cam_boot(label)?;
        let s = self.source_by_label(label)?;
        let bps = 1u64 << boot[108];
        let spc = 1u64 << boot[109];
        let heap = u64::from(u32::from_le_bytes(boot[88..92].try_into().unwrap()));
        let root = u64::from(u32::from_le_bytes(boot[96..100].try_into().unwrap()));
        let root_off = (heap + (root - 2) * spc) * bps;

        let mut image = XfsFile::open(&self.xfs, &s.image_path)?;
        let part = mbr::first_partition(&mut image)?;
        let mut view = SubFile::new(XfsFile::open(&self.xfs, &s.image_path)?, part.start, part.len);
        use std::io::{Read as _, Seek as _, SeekFrom};
        view.seek(SeekFrom::Start(root_off))?;
        let mut buf = vec![0u8; 32 * n];
        view.read_exact(&mut buf)?;
        Ok(buf.chunks(32).map(|e| e[0]).collect())
    }
}

// ---------------------------------------------------------------------------
// Gap-fill aliasing: a Tesla save/event MOVES continuous-recording minutes
// out of RecentClips into SavedClips/SentryClips. When that happens before
// any snapshot captured the minute at its RecentClips path, the merged tree
// has a hole in the 24/7 timeline even though the footage is right there in
// an event folder. Mirror of the Pi's playback gap-fill (the interior-hole
// rules of crates/drives/src/grouper.rs — same constants, kept in sync by
// comment because the drives crate carries Pi-only deps): surface event
// clips whose minute is MISSING from RecentClips under a virtual
// RecentClips/<basename> path. Occupied minutes are never touched, so
// parked sentry sessions and event twins of existing footage stay out of
// RecentClips exactly as before.
// ---------------------------------------------------------------------------

const RECENTS_PREFIX: &str = "TeslaCam/RecentClips/";
const EVENT_PREFIXES: [&str; 2] = ["TeslaCam/SavedClips/", "TeslaCam/SentryClips/"];
/// A hole must be wider than one missing minute-clip…
const GAP_FILL_MIN_S: i64 = 90;
/// …but within the fill cap: longer gaps are park/drive boundaries where
/// recording genuinely stopped (grouper::GAP_FILL_MAX_MS).
const GAP_FILL_MAX_S: i64 = 30 * 60;
/// Same-recorder-segment window: an event clip stamped ≤30 s after an
/// occupied RecentClips slot is Tesla's twin of that segment, not new
/// footage (grouper::GAP_FILL_DUP_MS).
const GAP_FILL_DUP_S: i64 = 30;

/// The `YYYY-MM-DD_HH-MM-SS` timestamp of a clip path's FILENAME component
/// (event paths embed the event-folder timestamp earlier in the path).
fn clip_ts(path: &str) -> Option<chrono::NaiveDateTime> {
    let base = path.rsplit('/').next()?;
    let stamp = base.get(..19)?;
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d_%H-%M-%S").ok()
}

/// Insert virtual `RecentClips/` entries for event clips that fill holes
/// in the continuous-recording timeline. Returns how many were added.
fn alias_gapfill_recents(files: &mut BTreeMap<String, FileMeta>) -> usize {
    // Continuous timeline: every RecentClips clip minute, sorted + deduped.
    let mut recent_ts: Vec<chrono::NaiveDateTime> = files
        .range(RECENTS_PREFIX.to_string()..)
        .take_while(|(p, _)| p.starts_with(RECENTS_PREFIX))
        .filter_map(|(p, _)| clip_ts(p))
        .collect();
    recent_ts.sort();
    recent_ts.dedup();
    if recent_ts.is_empty() {
        return 0;
    }

    // Event candidates, in BTreeMap (path) order so SavedClips precedes
    // SentryClips — lowest path wins the per-minute admission below.
    let cands: Vec<(chrono::NaiveDateTime, String)> = EVENT_PREFIXES
        .iter()
        .flat_map(|prefix| {
            files
                .range(prefix.to_string()..)
                .take_while(move |(p, _)| p.starts_with(prefix))
                .filter_map(|(p, _)| clip_ts(p).map(|ts| (ts, p.clone())))
        })
        .collect();
    if cands.is_empty() {
        return 0;
    }

    let holes: Vec<(chrono::NaiveDateTime, chrono::NaiveDateTime)> = recent_ts
        .windows(2)
        .filter(|w| {
            let gap = (w[1] - w[0]).num_seconds();
            gap > GAP_FILL_MIN_S && gap <= GAP_FILL_MAX_S
        })
        .map(|w| (w[0], w[1]))
        .collect();
    if holes.is_empty() {
        return 0;
    }
    let in_hole = |ts: chrono::NaiveDateTime| -> bool {
        let i = holes.partition_point(|h| h.0 < ts);
        i > 0 && ts < holes[i - 1].1
    };
    let dup_of_recent = |ts: chrono::NaiveDateTime| -> bool {
        let i = recent_ts.partition_point(|&r| r <= ts);
        i > 0 && (ts - recent_ts[i - 1]).num_seconds() <= GAP_FILL_DUP_S
    };

    // Admit per unique minute STAMP (all camera angles of a minute share
    // it): interior hole, not a twin of an occupied slot, and not within
    // the twin window of an already-admitted minute (drops the 0-1 s-later
    // duplicate the other event folder may hold).
    let mut stamps: Vec<chrono::NaiveDateTime> =
        cands.iter().map(|(ts, _)| *ts).collect();
    stamps.sort();
    stamps.dedup();
    let mut admitted: Vec<chrono::NaiveDateTime> = Vec::new();
    for ts in stamps {
        if !in_hole(ts) || dup_of_recent(ts) {
            continue;
        }
        if admitted
            .last()
            .is_some_and(|&last| (ts - last).num_seconds() <= GAP_FILL_DUP_S)
        {
            continue;
        }
        admitted.push(ts);
    }
    if admitted.is_empty() {
        return 0;
    }

    let mut added = 0usize;
    for (ts, path) in cands {
        if admitted.binary_search(&ts).is_err() {
            continue;
        }
        let Some(base) = path.rsplit('/').next() else {
            continue;
        };
        let alias = format!("{RECENTS_PREFIX}{base}");
        if files.contains_key(&alias) {
            continue;
        }
        let meta = files[&path].clone();
        files.insert(
            alias,
            FileMeta {
                source: meta.source,
                size: meta.size,
                real: Some(path),
            },
        );
        added += 1;
    }
    added
}

fn parse_toc(text: &str) -> Vec<TocEntry> {
    text.lines()
        .filter_map(|line| {
            let (size, path) = line.split_once(' ')?;
            Some(TocEntry {
                size: size.parse().ok()?,
                path: path.trim_start_matches('/').to_string(),
            })
        })
        .collect()
}

/// `Read + Seek` over a byte range of an inner `Read + Seek` (the CAM
/// partition inside a snap.bin).
pub struct SubFile<T> {
    inner: T,
    start: u64,
    len: u64,
    pos: u64,
}

impl<T: std::io::Read + std::io::Seek> SubFile<T> {
    pub fn new(inner: T, start: u64, len: u64) -> Self {
        Self {
            inner,
            start,
            len,
            pos: 0,
        }
    }
}

impl<T: std::io::Read + std::io::Seek> std::io::Read for SubFile<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::io::SeekFrom;
        if self.pos >= self.len {
            return Ok(0);
        }
        let want = buf.len().min((self.len - self.pos) as usize);
        self.inner.seek(SeekFrom::Start(self.start + self.pos))?;
        let n = self.inner.read(&mut buf[..want])?;
        self.pos += n as u64;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(source: usize) -> FileMeta {
        FileMeta {
            source,
            size: 1,
            real: None,
        }
    }

    fn tree(paths: &[&str]) -> BTreeMap<String, FileMeta> {
        paths.iter().map(|p| (p.to_string(), meta(0))).collect()
    }

    fn snap(slot: u64, mtime_secs: Option<u64>) -> Source {
        Source {
            slot,
            mtime: mtime_secs
                .map(|s| std::time::UNIX_EPOCH + std::time::Duration::from_secs(s)),
            label: format!("snap-{slot:06}"),
            toc: None,
            image_path: PathBuf::from(format!("snapshots/snap-{slot:06}/snap.bin")),
        }
    }

    /// Slot numbers are NOT time-monotonic in the field: a reflash can
    /// leave a stale high-numbered snapshot above a restarted sequence
    /// (verified device: snap-000414 frozen Jul 9 sitting above
    /// snap-000413 frozen Aug 8). `build_merged_tree` overwrites as it
    /// walks sources, so ordering by slot let the STALE snapshot win and
    /// serve its older file data as authoritative.
    #[test]
    fn sources_order_by_mtime_not_slot_number() {
        let mut sources = vec![
            snap(414, Some(1_784_600_000)), // stale leftover: highest slot, OLDEST
            snap(0, Some(1_785_300_000)),
            snap(413, Some(1_786_200_000)), // genuinely newest
        ];
        sort_snapshot_sources(&mut sources);
        let order: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(order, vec!["snap-000414", "snap-000000", "snap-000413"]);
    }

    /// An unreadable inode must never win a merge conflict, but its
    /// uniquely-recoverable files should still surface — so it sorts
    /// before every known-age snapshot, with slot breaking ties among
    /// unknowns.
    #[test]
    fn sources_with_unknown_mtime_sort_first_by_slot() {
        let mut sources = vec![
            snap(50, Some(1_700_000_000)),
            snap(9, None),
            snap(7, None),
        ];
        sort_snapshot_sources(&mut sources);
        let order: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(order, vec!["snap-000007", "snap-000009", "snap-000050"]);
    }

    /// Equal mtimes (restore that flattened timestamps) fall back to slot
    /// order, which is the best remaining signal.
    #[test]
    fn sources_with_equal_mtime_fall_back_to_slot() {
        let mut sources = vec![
            snap(12, Some(1_786_200_000)),
            snap(3, Some(1_786_200_000)),
        ];
        sort_snapshot_sources(&mut sources);
        let order: Vec<&str> = sources.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(order, vec!["snap-000003", "snap-000012"]);
    }

    #[test]
    fn clip_ts_parses_filename_not_event_folder() {
        let ts = clip_ts("TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_04-50-00-front.mp4")
            .unwrap();
        assert_eq!(ts.to_string(), "2026-07-15 04:50:00");
        assert!(clip_ts("TeslaCam/RecentClips/thumb.png").is_none());
    }

    /// The user-save scenario: parked minutes moved into SavedClips before
    /// any snapshot caught them in RecentClips. They must surface at
    /// virtual RecentClips paths; the twin of an occupied slot, the
    /// SentryClips duplicate of an admitted minute, and out-of-hole event
    /// clips must not.
    #[test]
    fn alias_gapfill_fills_interior_holes_only() {
        let mut files = tree(&[
            // Continuous timeline with a 04:49 → 05:00 hole.
            "TeslaCam/RecentClips/2026-07-15_04-49-09-front.mp4",
            "TeslaCam/RecentClips/2026-07-15_05-00-03-front.mp4",
            // Missing minutes, all cameras (moved by the save).
            "TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_04-50-00-front.mp4",
            "TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_04-50-00-back.mp4",
            "TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_04-55-00-front.mp4",
            // SentryClips twin of an admitted minute, stamped 1 s later → dropped.
            "TeslaCam/SentryClips/2026-07-15_04-58-00/2026-07-15_04-55-01-front.mp4",
            // Twin of the occupied 05:00:03 slot → dropped.
            "TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_05-00-04-front.mp4",
            // Parked sentry session hours away, outside any hole → dropped.
            "TeslaCam/SentryClips/2026-07-15_01-00-00/2026-07-15_00-59-00-front.mp4",
        ]);
        let added = alias_gapfill_recents(&mut files);
        assert_eq!(added, 3);

        let aliases: Vec<&str> = files
            .iter()
            .filter(|(_, m)| m.real.is_some())
            .map(|(p, _)| p.as_str())
            .collect();
        assert_eq!(
            aliases,
            vec![
                "TeslaCam/RecentClips/2026-07-15_04-50-00-back.mp4",
                "TeslaCam/RecentClips/2026-07-15_04-50-00-front.mp4",
                "TeslaCam/RecentClips/2026-07-15_04-55-00-front.mp4",
            ]
        );
        // Aliases read from the clip's real event-folder path.
        assert_eq!(
            files["TeslaCam/RecentClips/2026-07-15_04-50-00-front.mp4"]
                .real
                .as_deref(),
            Some("TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_04-50-00-front.mp4"),
        );
    }

    /// A minute the union already holds at its RecentClips path (an older
    /// snapshot caught it before Tesla moved it) is never overwritten,
    /// and a boundary gap wider than the fill cap admits nothing.
    #[test]
    fn alias_gapfill_never_clobbers_and_respects_cap() {
        let mut files = tree(&[
            "TeslaCam/RecentClips/2026-07-15_04-49-00-front.mp4",
            "TeslaCam/RecentClips/2026-07-15_04-52-00-front.mp4",
            // Same minute exists BOTH places (snapshot caught the move
            // mid-flight) → the RecentClips entry wins, no alias.
            "TeslaCam/RecentClips/2026-07-15_04-50-00-front.mp4",
            "TeslaCam/SavedClips/2026-07-15_04-51-00/2026-07-15_04-50-00-front.mp4",
            // 45-min park boundary later that day → not a fillable hole.
            "TeslaCam/RecentClips/2026-07-15_06-00-00-front.mp4",
            "TeslaCam/SentryClips/2026-07-15_05-30-00/2026-07-15_05-29-00-front.mp4",
        ]);
        assert_eq!(alias_gapfill_recents(&mut files), 0);
        assert!(files["TeslaCam/RecentClips/2026-07-15_04-50-00-front.mp4"]
            .real
            .is_none());
    }

    #[test]
    fn alias_gapfill_empty_inputs() {
        // No RecentClips at all → nothing to anchor holes.
        let mut only_events =
            tree(&["TeslaCam/SavedClips/2026-07-15_04-59-30/2026-07-15_04-50-00-front.mp4"]);
        assert_eq!(alias_gapfill_recents(&mut only_events), 0);
        // No event folders → nothing to fill with.
        let mut only_recents = tree(&[
            "TeslaCam/RecentClips/2026-07-15_04-49-09-front.mp4",
            "TeslaCam/RecentClips/2026-07-15_05-00-03-front.mp4",
        ]);
        assert_eq!(alias_gapfill_recents(&mut only_recents), 0);
    }
}

impl<T: std::io::Read + std::io::Seek> std::io::Seek for SubFile<T> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        use std::io::SeekFrom;
        let newpos = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(off) => self.len.checked_add_signed(off),
            SeekFrom::Current(off) => self.pos.checked_add_signed(off),
        };
        match newpos {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "seek out of range",
            )),
        }
    }
}
