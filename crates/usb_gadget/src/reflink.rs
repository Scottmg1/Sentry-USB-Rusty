//! Reflink-aware disk accounting for snapshot images (XFS only).
//!
//! `du` and `stat`'s `st_blocks` charge every block a file MAPS to that
//! file. They de-duplicate hard links and understand sparse holes, but
//! they have no notion of XFS reflink sharing: a `cp --reflink` clone is a
//! separate inode whose extents are shared via the refcount B-tree, which
//! `stat(2)` cannot see. So every snapshot reports the full block count of
//! `cam_disk.bin` at the moment it was cloned, and 89 snapshots "sum" to
//! several times the partition size.
//!
//! That figure is not wrong as *bytes referenced* — it is simply not the
//! question anyone asks. `FIEMAP` gets closer: XFS sets
//! `FIEMAP_EXTENT_SHARED` from the refcount tree, so extents without it
//! are mapped by this inode alone.
//!
//! # What this number actually means
//!
//! > Best-effort XFS data-fork bytes currently reported unshared. For a
//! > quiescent, single-link, reflink-only snapshot with no self-reflinks
//! > and no open users, these blocks should *eventually* become free once
//! > the inode is removed. Excludes metadata, COW-fork allocations, and
//! > any concurrent refcount changes.
//!
//! It is emphatically NOT "bytes `statvfs` will gain when you unlink this
//! path". The gap is real and the caller must not paper over it:
//!
//! * Hard links — if `st_nlink > 1`, unlinking this path frees nothing.
//!   [`exclusive_bytes`] refuses rather than report a number.
//! * Open handles — an open descriptor, loop mount or mmap keeps the
//!   blocks until the last reference closes. Ordinary unlink semantics.
//! * Self-reflinks — a file sharing a block with *itself* at another
//!   offset reports SHARED, so this UNDER-counts.
//! * COW-fork staging — if a supposedly immutable snapshot is written,
//!   staging allocations live outside the data fork this walks.
//! * The walk is not atomic across ioctls; refcounts can change mid-walk.
//!
//! Cost: thousands to tens of thousands of records per image is typical
//! and pathological fragmentation reaches millions. This belongs in a
//! background job, never inline in a request handler.

use anyhow::{bail, Context, Result};

/// Extent flags from `linux/fiemap.h` that change the accounting.
const FIEMAP_EXTENT_LAST: u32 = 0x0001;
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0002;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0004;
const FIEMAP_EXTENT_SHARED: u32 = 0x2000;

/// One record as `FIEMAP` returns it, reduced to the fields that matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MappedExtent {
    pub logical: u64,
    /// Physical byte offset on the device. This is what identifies a block
    /// ACROSS files: two snapshots sharing data map the same physical
    /// range, which is how [`cumulative_reclaim`] can tell who else is
    /// holding a block.
    pub physical: u64,
    pub length: u64,
    pub flags: u32,
}

impl MappedExtent {
    /// For callers that only care about sharing, not placement (tests).
    pub fn new(logical: u64, length: u64, flags: u32) -> Self {
        Self { logical, physical: 0, length, flags }
    }

    pub fn at(logical: u64, physical: u64, length: u64, flags: u32) -> Self {
        Self { logical, physical, length, flags }
    }
}

/// A half-open physical byte range `[start, start + len)`.
pub type PhysicalRange = (u64, u64);

/// Bytes freed by deleting each oldest-first PREFIX of the snapshot list.
///
/// The per-snapshot "exclusively held" figure is the wrong question for
/// this workload and measures ~0 for every row: hourly snapshots of a
/// slowly-changing disk share nearly every block with several neighbours,
/// so almost nothing belongs to exactly one. Yet the set collectively
/// holds hundreds of GB. Both facts are true, and only the second is
/// actionable.
///
/// A block is released when its LAST holder goes. Deleting oldest-first,
/// that is the NEWEST snapshot referencing it — so attribute every
/// physical range to its newest holder, then prefix-sum. Result `i` is
/// "delete snapshots 0..=i and this much comes back".
///
/// `external` is every range referenced by something that is not a
/// snapshot (chiefly the live `cam_disk.bin`). Those blocks survive no
/// matter how many snapshots go, so they are excluded entirely.
///
/// `ordered` must be oldest-first; the returned vector matches its order.
pub fn cumulative_reclaim(
    ordered: &[Vec<PhysicalRange>],
    external: &[PhysicalRange],
) -> Vec<u64> {
    // Sweep line over physical space. At each elementary interval we need
    // two things: whether anything external covers it (then it can never
    // be freed), and the highest snapshot index covering it (its last
    // holder).
    #[derive(Clone, Copy)]
    struct Event {
        pos: u64,
        delta: i32,
        owner: Option<usize>,
    }

    let mut events: Vec<Event> = Vec::new();
    let push = |start: u64, len: u64, owner: Option<usize>, ev: &mut Vec<Event>| {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);
        ev.push(Event { pos: start, delta: 1, owner });
        ev.push(Event { pos: end, delta: -1, owner });
    };

    for (idx, ranges) in ordered.iter().enumerate() {
        for &(start, len) in ranges {
            push(start, len, Some(idx), &mut events);
        }
    }
    for &(start, len) in external {
        push(start, len, None, &mut events);
    }

    if events.is_empty() {
        return vec![0; ordered.len()];
    }
    events.sort_by_key(|e| e.pos);

    // Bytes whose last holder is snapshot i.
    let mut owned_by = vec![0u64; ordered.len()];
    let mut external_depth: i64 = 0;
    // Active snapshot indices → coverage count. BTreeMap so the highest
    // active index (the last holder) is the final key.
    let mut active: std::collections::BTreeMap<usize, i64> = std::collections::BTreeMap::new();
    let mut prev_pos = events[0].pos;

    let mut i = 0;
    while i < events.len() {
        let pos = events[i].pos;

        // Account for the span [prev_pos, pos) using the state before this
        // position's events are applied.
        if pos > prev_pos && external_depth == 0 {
            if let Some((&last_holder, _)) = active.iter().next_back() {
                owned_by[last_holder] = owned_by[last_holder].saturating_add(pos - prev_pos);
            }
        }

        while i < events.len() && events[i].pos == pos {
            let e = events[i];
            match e.owner {
                None => external_depth += e.delta as i64,
                Some(idx) => {
                    let c = active.entry(idx).or_insert(0);
                    *c += e.delta as i64;
                    if *c <= 0 {
                        active.remove(&idx);
                    }
                }
            }
            i += 1;
        }
        prev_pos = pos;
    }

    // Prefix sum: deleting 0..=k frees everything whose last holder is <= k.
    let mut out = Vec::with_capacity(ordered.len());
    let mut running = 0u64;
    for bytes in owned_by {
        running = running.saturating_add(bytes);
        out.push(running);
    }
    out
}

/// Accumulates a FIEMAP walk one page of records at a time.
///
/// Split from the ioctl so the paging state machine is testable without a
/// filesystem: feed it synthetic pages to exercise overlap, zero-length,
/// unterminated and out-of-order kernel output.
#[derive(Debug, Default)]
pub struct Walk {
    total: u64,
    cursor: u64,
    done: bool,
    poisoned: bool,
    ranges: Vec<PhysicalRange>,
}

/// The extent map was too fragmented to walk within the round-trip budget.
///
/// Distinct from an ordinary failure so the caller can classify it: this
/// row will fail again immediately, so it deserves backoff and a manual
/// retry rather than being re-attempted on every poll.
#[derive(Debug)]
pub struct FragmentationLimitExceeded {
    pub offset: u64,
    pub records: usize,
}

impl std::fmt::Display for FragmentationLimitExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "extent map too fragmented to measure: still at offset {} after {} records",
            self.offset, self.records
        )
    }
}

impl std::error::Error for FragmentationLimitExceeded {}

impl Walk {
    pub fn new() -> Self {
        Self::default()
    }

    /// Next logical offset to request.
    pub fn cursor(&self) -> u64 {
        self.cursor
    }

    /// True once a record carried `FIEMAP_EXTENT_LAST`.
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Fold one page of records in, validating the kernel's output.
    ///
    /// Every arithmetic step is checked, never saturating: saturation
    /// turns impossible output into a confident lie, which is the exact
    /// failure mode this module exists to remove.
    pub fn ingest(&mut self, page: &[MappedExtent]) -> Result<()> {
        if self.poisoned {
            bail!("FIEMAP walk was poisoned by an earlier error");
        }
        if self.done {
            self.poisoned = true;
            bail!("FIEMAP walk already terminated; refusing further records");
        }
        // Any error below leaves the cursor and total partially advanced,
        // so the walk must never be usable again.
        let r = self.ingest_inner(page);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    /// A page containing zero records.
    ///
    /// On a pristine walk that is a legitimate empty or all-hole file:
    /// XFS's iomap FIEMAP skips holes and only sets `LAST` on a non-hole
    /// mapping, so there is no record for it to flag. After positive
    /// non-`LAST` pages it means the map changed under us, which is an
    /// error under this module's fail-closed contract.
    pub fn ingest_empty_page(&mut self) -> Result<()> {
        if self.poisoned {
            bail!("FIEMAP walk was poisoned by an earlier error");
        }
        if self.cursor == 0 && self.total == 0 {
            self.done = true;
            return Ok(());
        }
        self.poisoned = true;
        bail!(
            "FIEMAP returned no records at offset {} after mapping {} bytes, \
             without ever flagging the last extent — the map changed mid-walk",
            self.cursor,
            self.total
        )
    }

    fn ingest_inner(&mut self, page: &[MappedExtent]) -> Result<()> {
        for (i, e) in page.iter().enumerate() {
            if e.flags & (FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_UNKNOWN) != 0 {
                bail!("extent map incomplete (delalloc/unknown present) — retry later");
            }
            if e.length == 0 {
                bail!("FIEMAP returned a zero-length extent at offset {}", e.logical);
            }
            if e.logical < self.cursor {
                bail!(
                    "FIEMAP returned an out-of-order or overlapping extent: \
                     logical {} precedes cursor {}",
                    e.logical,
                    self.cursor
                );
            }
            let end = e
                .logical
                .checked_add(e.length)
                .context("FIEMAP extent logical+length overflows u64")?;

            // Unwritten (preallocated) extents COUNT: they hold allocated
            // blocks and those blocks are released with the inode. Holes
            // never appear in a FIEMAP map, so there is nothing to skip.
            if e.flags & FIEMAP_EXTENT_SHARED == 0 {
                self.total = self
                    .total
                    .checked_add(e.length)
                    .context("FIEMAP exclusive-byte total overflows u64")?;
            }

            // Physical placement, for cross-file sharing analysis.
            self.ranges.push((e.physical, e.length));

            self.cursor = end;
            if e.flags & FIEMAP_EXTENT_LAST != 0 {
                // LAST must terminate the page. Returning early here used
                // to silently discard the rest of the slice, so trailing
                // allocations — or a DELALLOC record that should have
                // failed the whole walk — were ignored.
                if i + 1 != page.len() {
                    bail!(
                        "FIEMAP flagged the last extent at index {} of a {}-record page; \
                         {} record(s) follow it",
                        i,
                        page.len(),
                        page.len() - i - 1
                    );
                }
                self.done = true;
                return Ok(());
            }
        }
        Ok(())
    }

    /// The total, but only if the walk actually reached the end of the map.
    ///
    /// An unterminated walk is an error, never a number. Returning the
    /// prefix accumulated so far would look entirely plausible and be
    /// silently too small.
    pub fn finish(self) -> Result<u64> {
        self.check_complete()?;
        Ok(self.total)
    }

    /// Every physical range this file maps, once the walk is complete.
    pub fn finish_map(self) -> Result<Vec<PhysicalRange>> {
        self.check_complete()?;
        Ok(self.ranges)
    }

    fn check_complete(&self) -> Result<()> {
        if !self.done {
            bail!(
                "FIEMAP walk did not reach the end of the map (stopped at offset {}, \
                 {} exclusive bytes so far) — refusing to report a partial result",
                self.cursor,
                self.total
            );
        }
        Ok(())
    }
}

/// Compute reclaimable bytes for many snapshots, tolerating individual
/// failures.
///
/// One unreadable or mid-write snapshot must not blank the whole page, so
/// per-snapshot errors are collected and returned alongside the successes
/// rather than aborting. The caller shows "unavailable" for those rows —
/// never `0 B`, which would read as "safe to delete, frees nothing".
///
/// Takes the measuring function so the aggregation is testable off-Linux.
/// Stable identity of the file a measurement was taken from.
///
/// A cache keyed only by snapshot NAME can bind a valid measurement to the
/// wrong inode: measure `snap-000123/snap.bin` at 5 GB, have that
/// directory replaced under the same id, and the 5 GB is still served
/// although unlinking the current path might free nothing. Slot reuse
/// after an abandoned snapshot makes that reachable here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Identity of `path` without following a final symlink.
#[cfg(unix)]
pub fn file_identity(path: &std::path::Path) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(path)?;
    Ok(FileIdentity { dev: md.dev(), ino: md.ino() })
}

#[cfg(not(unix))]
pub fn file_identity(path: &std::path::Path) -> Result<FileIdentity> {
    let _ = std::fs::symlink_metadata(path)?;
    Ok(FileIdentity::default())
}

pub fn measure_all_with<T, F>(
    ids: &[String],
    mut measure: F,
) -> (Vec<(String, T)>, Vec<(String, String)>)
where
    F: FnMut(&str) -> Result<T>,
{
    let mut ok = Vec::new();
    let mut failed = Vec::new();
    for id in ids {
        match measure(id) {
            Ok(bytes) => ok.push((id.clone(), bytes)),
            Err(e) => failed.push((id.clone(), e.to_string())),
        }
    }
    (ok, failed)
}

/// Cached per-snapshot reclaimable sizes.
///
/// Deliberately all-or-nothing on invalidation. Reclaimable size is a
/// property of the whole SET, not of one snapshot: an extent shared only
/// between snapshots A and B is exclusive to neither, but delete A and it
/// becomes exclusive to B. So deleting any one snapshot can change every
/// other row's number, and evicting just the deleted key would leave the
/// rest quietly wrong — the same "confidently incorrect" this change
/// exists to remove.
#[derive(Debug, Default)]
pub struct SizeCache {
    entries: std::collections::HashMap<String, (u64, FileIdentity)>,
    computed_at: Option<u64>,
    /// The set this measurement was taken against, so a result computed
    /// for a different set of snapshots is never shown.
    measured_ids: Vec<String>,
    /// Bumped on every invalidation. A worker captures this at start and
    /// may only publish if it still matches.
    generation: u64,
    /// True once a measurement completed, even if every row failed. A
    /// completed all-failure attempt is a FINISHED state — without this,
    /// "no usable entries" looked identical to "still running" and the UI
    /// re-triggered a full scan on every poll, forever.
    attempted: bool,
}

impl SizeCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generation to capture before starting a measurement.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Publish a measurement, but only if nothing invalidated the cache
    /// while it ran.
    ///
    /// Without this check a delete could be "resurrected": a refresh
    /// starts against {A,B}, A is deleted, the delete invalidates, then
    /// the in-flight refresh finishes and republishes the pre-delete
    /// numbers as fresh. Returns false when the result was discarded.
    pub fn publish(
        &mut self,
        generation: u64,
        measured_ids: Vec<String>,
        entries: Vec<(String, (u64, FileIdentity))>,
        now: u64,
    ) -> bool {
        if generation != self.generation {
            return false;
        }
        self.entries = entries.into_iter().collect();
        self.measured_ids = measured_ids;
        self.computed_at = Some(now);
        self.attempted = true;
        true
    }

    /// Drop everything — call after ANY snapshot is created or deleted.
    pub fn invalidate(&mut self) {
        self.entries.clear();
        self.measured_ids.clear();
        self.computed_at = None;
        self.attempted = false;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Value for `id`, but only if the file still has the identity it had
    /// when measured. A replaced inode under the same name yields `None`
    /// rather than a stale figure.
    pub fn get_if_same(&self, id: &str, now: FileIdentity) -> Option<u64> {
        match self.entries.get(id) {
            Some((bytes, ident)) if *ident == now => Some(*bytes),
            _ => None,
        }
    }

    pub fn computed_at(&self) -> Option<u64> {
        self.computed_at
    }

    /// True when a measurement has completed against exactly `ids` and is
    /// recent enough.
    ///
    /// Compares the SET, not just emptiness. Snapshots are created hourly
    /// and deleted by the runtime, neither of which can call
    /// [`Self::invalidate`] on this process's cache, so age alone would
    /// let a measurement taken against a different set look valid.
    pub fn is_current_for(&self, ids: &[String], now: u64, max_age_secs: u64) -> bool {
        if !self.attempted {
            return false;
        }
        match self.computed_at {
            Some(at) if now.saturating_sub(at) <= max_age_secs => self.measured_ids == ids,
            _ => false,
        }
    }
}

#[cfg(target_os = "linux")]
pub use self::linux::{exclusive_bytes, extent_map};

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;
    use std::path::Path;

    /// `_IOWR('f', 11, struct fiemap)`. Same encoding on ARM32 and ARM64.
    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;

    /// `XFS_SUPER_MAGIC`. Fits in an i32, so the comparison is valid on
    /// both `__fsword_t` widths.
    const XFS_SUPER_MAGIC: i64 = 0x5846_5342;

    /// Records requested per ioctl (56 bytes each, so ~112 KiB).
    const EXTENTS_PER_CALL: u32 = 2048;

    /// Ceiling on ioctl round trips so a pathologically fragmented image
    /// cannot pin the background worker forever. Exhausting it is an
    /// ERROR, never a total — see [`Walk::finish`].
    const MAX_ROUND_TRIPS: usize = 4096;

    #[repr(C)]
    #[derive(Default)]
    pub(super) struct FiemapHeader {
        fm_start: u64,
        fm_length: u64,
        fm_flags: u32,
        fm_mapped_extents: u32,
        fm_extent_count: u32,
        fm_reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub(super) struct FiemapExtent {
        fe_logical: u64,
        fe_physical: u64,
        fe_length: u64,
        fe_reserved64: [u64; 2],
        fe_flags: u32,
        fe_reserved: [u32; 3],
    }

    // Catch an ABI drift at compile time rather than by reading garbage
    // flags out of the buffer at runtime.
    const _: () = assert!(std::mem::size_of::<FiemapHeader>() == 32);
    const _: () = assert!(std::mem::size_of::<FiemapExtent>() == 56);
    const _: () = assert!(std::mem::align_of::<FiemapHeader>() == 8);
    const _: () = assert!(std::mem::align_of::<FiemapExtent>() == 8);

    /// True when `fd` lives on XFS.
    ///
    /// This module's reasoning is XFS-specific: elsewhere a logical
    /// `fe_length` need not correspond to reclaimable allocation at all
    /// (compression, inline data, tail packing), and shared-extent
    /// reporting varies.
    fn is_xfs(fd: libc::c_int) -> Result<bool> {
        // SAFETY: fstatfs only writes the out-param we provide.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatfs(fd, &mut st) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("fstatfs");
        }
        Ok(st.f_type as i64 == XFS_SUPER_MAGIC)
    }

    /// Walk `path`'s extent map and total the bytes it alone maps.
    ///
    /// Deliberately does NOT set `FIEMAP_FLAG_SYNC`: it only forces
    /// writeback on this inode, which is pointless for a snapshot that is
    /// immutable once published, does nothing about refcount changes
    /// involving other files, and would repeat whole-inode writeback on
    /// every page. Callers that hit an incomplete map retry later.
    /// Every physical range `path` maps, for cross-file sharing analysis.
    ///
    /// Same validation and safety checks as [`exclusive_bytes`]; only the
    /// result differs.
    pub fn extent_map(path: &Path) -> Result<Vec<PhysicalRange>> {
        walk_file(path)?.finish_map()
    }

    pub fn exclusive_bytes(path: &Path) -> Result<u64> {
        walk_file(path)?.finish()
    }

    fn walk_file(path: &Path) -> Result<Walk> {
        // O_NOFOLLOW guards only the FINAL component, so check the parent
        // too: a symlinked `snap-NNNNNN` directory would otherwise be
        // followed and we would measure some other regular XFS file while
        // deletion removed the alias instead of that inode.
        if let Some(parent) = path.parent() {
            let pmeta = std::fs::symlink_metadata(parent)
                .with_context(|| format!("stat {}", parent.display()))?;
            if pmeta.file_type().is_symlink() {
                bail!("{} is a symlink — refusing to measure through it", parent.display());
            }
        }

        // O_NOFOLLOW: a `snap.bin` that is a SYMLINK to cam_disk.bin would
        // otherwise be opened as its target, pass the XFS and nlink
        // checks, and report the LIVE image's exclusive extents as this
        // snapshot's reclaimable size — a huge number, where deleting the
        // snapshot removes only the symlink and frees nothing. A worse lie
        // than the `du` figure being replaced.
        //
        // O_NONBLOCK: opening a FIFO read-only blocks until a writer
        // appears, which would hang before the regular-file check and wedge
        // the single measurement worker forever. It has no effect on
        // regular files.
        let file = std::fs::File::options()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)
            .with_context(|| {
                format!("opening {} (must be a regular file, not a symlink)", path.display())
            })?;
        let fd = file.as_raw_fd();

        let meta = file.metadata()?;
        if !meta.is_file() {
            bail!("{} is not a regular file", path.display());
        }

        if !is_xfs(fd)? {
            bail!(
                "{} is not on XFS — unshared-extent accounting is not meaningful here",
                path.display()
            );
        }

        // A second link means unlinking this path frees nothing at all, so
        // any "reclaimable" figure would be pure fiction.
        let nlink = meta.nlink();
        if nlink != 1 {
            bail!(
                "{} has {} links — deleting this path would free nothing",
                path.display(),
                nlink
            );
        }

        let hdr_size = std::mem::size_of::<FiemapHeader>();
        let ext_size = std::mem::size_of::<FiemapExtent>();
        let buf_bytes = hdr_size + ext_size * EXTENTS_PER_CALL as usize;
        // u64-backed so the header/record reads below are 8-byte aligned.
        let mut buf = vec![0u64; buf_bytes.div_ceil(8)];

        let mut walk = Walk::new();

        for _ in 0..MAX_ROUND_TRIPS {
            if walk.is_done() {
                break;
            }
            let start = walk.cursor();
            let length = u64::MAX
                .checked_sub(start)
                .context("FIEMAP cursor overflow")?;

            // SAFETY: buf is >= hdr_size bytes and u64-aligned.
            unsafe {
                *buf.as_mut_ptr().cast::<FiemapHeader>() = FiemapHeader {
                    fm_start: start,
                    // To the maximum offset so post-EOF preallocation is
                    // included; the kernel clamps to s_maxbytes.
                    fm_length: length,
                    fm_flags: 0,
                    fm_mapped_extents: 0,
                    fm_extent_count: EXTENTS_PER_CALL,
                    fm_reserved: 0,
                };
            }

            loop {
                // SAFETY: the ioctl writes at most fm_extent_count records
                // into the buffer we sized for exactly that many.
                let rc = unsafe { libc::ioctl(fd, FS_IOC_FIEMAP, buf.as_mut_ptr().cast::<libc::c_void>()) };
                if rc == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err).with_context(|| format!("FIEMAP on {}", path.display()));
            }

            // SAFETY: the ioctl succeeded, so the header is populated.
            let mapped = unsafe { (*buf.as_ptr().cast::<FiemapHeader>()).fm_mapped_extents };
            // Mainline enforces this, but an out-of-tree filesystem or a
            // kernel bug would otherwise turn into an out-of-bounds read.
            if mapped > EXTENTS_PER_CALL {
                bail!(
                    "FIEMAP on {} reported {} extents for a {}-record buffer",
                    path.display(),
                    mapped,
                    EXTENTS_PER_CALL
                );
            }
            if mapped == 0 {
                // Legitimate on a pristine walk (empty or all-hole file:
                // XFS skips holes and has no record to flag LAST on);
                // an error after real records — see `ingest_empty_page`.
                walk.ingest_empty_page()?;
                break;
            }

            let mut page = Vec::with_capacity(mapped as usize);
            for i in 0..mapped as usize {
                // SAFETY: i < mapped <= EXTENTS_PER_CALL, all within buf.
                let e = unsafe {
                    let base = buf.as_ptr().cast::<u8>().add(hdr_size);
                    *base.add(i * ext_size).cast::<FiemapExtent>()
                };
                page.push(MappedExtent::at(e.fe_logical, e.fe_physical, e.fe_length, e.fe_flags));
            }
            walk.ingest(&page)?;
        }

        if !walk.is_done() {
            // Distinguish "too fragmented for the budget" from a malformed
            // map: this one will fail identically on every retry, so the
            // caller backs it off instead of rescanning on each poll.
            return Err(FragmentationLimitExceeded {
                offset: walk.cursor(),
                records: MAX_ROUND_TRIPS * EXTENTS_PER_CALL as usize,
            }
            .into());
        }
        Ok(walk)
    }
}

#[cfg(not(target_os = "linux"))]
pub fn exclusive_bytes(_path: &std::path::Path) -> Result<u64> {
    bail!("FIEMAP extent accounting is Linux-only")
}

#[cfg(not(target_os = "linux"))]
pub fn extent_map(_path: &std::path::Path) -> Result<Vec<PhysicalRange>> {
    bail!("FIEMAP extent accounting is Linux-only")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    /// Stand-in file identity for cache tests.
    const ID: FileIdentity = FileIdentity { dev: 1, ino: 2 };
    const UNWRITTEN: u32 = 0x0800;
    const LAST: u32 = FIEMAP_EXTENT_LAST;

    fn walk_all(page: &[MappedExtent]) -> Result<u64> {
        let mut w = Walk::new();
        w.ingest(page)?;
        w.finish()
    }

    /// The whole point: extents shared with the live image or another
    /// snapshot are not reclaimed by deleting this one. This is precisely
    /// what `du` gets wrong.
    #[test]
    fn shared_extents_are_not_reclaimable() {
        let page = [
            MappedExtent::new(0, 100 * MIB, FIEMAP_EXTENT_SHARED),
            MappedExtent::new(100 * MIB, 5 * MIB, 0),
            MappedExtent::new(105 * MIB, 200 * MIB, FIEMAP_EXTENT_SHARED | LAST),
        ];
        assert_eq!(walk_all(&page).unwrap(), 5 * MIB);
    }

    /// A freshly cloned snapshot shares everything, so deleting it returns
    /// nothing — the 45 GB row that frees 0.
    #[test]
    fn fully_shared_clone_reclaims_nothing() {
        let page = [
            MappedExtent::new(0, 45 * 1024 * MIB, FIEMAP_EXTENT_SHARED),
            MappedExtent::new(45 * 1024 * MIB, 19 * 1024 * MIB, FIEMAP_EXTENT_SHARED | LAST),
        ];
        assert_eq!(walk_all(&page).unwrap(), 0);
    }

    /// Unwritten extents hold allocated blocks released on unlink.
    #[test]
    fn unwritten_extents_are_reclaimable() {
        let page = [MappedExtent::new(0, 8 * MIB, UNWRITTEN | LAST)];
        assert_eq!(walk_all(&page).unwrap(), 8 * MIB);
    }

    #[test]
    fn shared_wins_over_unwritten() {
        let page = [MappedExtent::new(0, 8 * MIB, UNWRITTEN | FIEMAP_EXTENT_SHARED | LAST)];
        assert_eq!(walk_all(&page).unwrap(), 0);
    }

    /// Delayed-allocation and unknown extents mean the kernel could not
    /// say where the data is. A confidently smaller number is how we got
    /// here; refuse instead.
    #[test]
    fn incomplete_maps_are_refused_not_guessed() {
        assert!(walk_all(&[MappedExtent::new(0, MIB, FIEMAP_EXTENT_DELALLOC | LAST)]).is_err());
        assert!(walk_all(&[MappedExtent::new(0, MIB, FIEMAP_EXTENT_UNKNOWN | LAST)]).is_err());
        let mixed = [
            MappedExtent::new(0, MIB, 0),
            MappedExtent::new(MIB, MIB, FIEMAP_EXTENT_DELALLOC | LAST),
        ];
        assert!(walk_all(&mixed).is_err());
    }

    /// THE ship blocker from review: a walk that never saw LAST must not
    /// report the prefix it happened to accumulate. That number looks
    /// entirely plausible and is silently too small.
    #[test]
    fn unterminated_walk_refuses_to_report_a_partial_total() {
        let mut w = Walk::new();
        w.ingest(&[MappedExtent::new(0, 10 * MIB, 0)]).unwrap();
        assert!(!w.is_done());
        let err = w.finish().unwrap_err().to_string();
        assert!(err.contains("partial"), "unexpected error: {}", err);
    }

    /// Multi-page walks accumulate across ioctls and terminate on LAST.
    #[test]
    fn walk_accumulates_across_pages_and_advances_the_cursor() {
        let mut w = Walk::new();
        w.ingest(&[
            MappedExtent::new(0, 4 * MIB, 0),
            MappedExtent::new(4 * MIB, 4 * MIB, FIEMAP_EXTENT_SHARED),
        ])
        .unwrap();
        assert_eq!(w.cursor(), 8 * MIB);
        assert!(!w.is_done());
        w.ingest(&[MappedExtent::new(8 * MIB, 2 * MIB, LAST)]).unwrap();
        assert!(w.is_done());
        assert_eq!(w.finish().unwrap(), 6 * MIB);
    }

    /// Records that go backwards would double-count. Reject them.
    #[test]
    fn overlapping_or_out_of_order_extents_are_rejected() {
        let mut w = Walk::new();
        w.ingest(&[MappedExtent::new(0, 10 * MIB, 0)]).unwrap();
        assert!(w.ingest(&[MappedExtent::new(5 * MIB, MIB, LAST)]).is_err());
    }

    #[test]
    fn zero_length_extents_are_rejected() {
        assert!(walk_all(&[MappedExtent::new(0, 0, LAST)]).is_err());
    }

    /// Arithmetic is checked, not saturating: impossible kernel output
    /// must surface as an error, not a plausible number.
    #[test]
    fn overflowing_extent_is_an_error_not_a_saturated_total() {
        assert!(walk_all(&[MappedExtent::new(u64::MAX - 1, 8, LAST)]).is_err());
    }

    #[test]
    fn records_after_last_are_rejected() {
        let mut w = Walk::new();
        w.ingest(&[MappedExtent::new(0, MIB, LAST)]).unwrap();
        assert!(w.ingest(&[MappedExtent::new(MIB, MIB, 0)]).is_err());
    }

    /// A wholly-shared map is zero reclaimable.
    #[test]
    fn fully_shared_terminated_map_is_zero() {
        assert_eq!(walk_all(&[MappedExtent::new(0, MIB, FIEMAP_EXTENT_SHARED | LAST)]).unwrap(), 0);
    }

    /// A genuinely EMPTY map — zero records on a pristine walk. XFS skips
    /// holes and only flags LAST on a non-hole mapping, so an empty or
    /// all-hole file has no record to carry the flag. That is a valid
    /// zero, not a failure.
    #[test]
    fn empty_map_on_a_pristine_walk_is_a_valid_zero() {
        let mut w = Walk::new();
        w.ingest_empty_page().unwrap();
        assert!(w.is_done());
        assert_eq!(w.finish().unwrap(), 0);
    }

    /// Zero records AFTER real records means the map changed under us.
    #[test]
    fn empty_page_after_real_extents_is_an_error() {
        let mut w = Walk::new();
        w.ingest(&[MappedExtent::new(0, MIB, 0)]).unwrap();
        assert!(w.ingest_empty_page().is_err());
    }

    /// LAST must end the page. Returning early on it would silently
    /// discard the rest of the slice, including a DELALLOC record that
    /// should fail the entire walk.
    #[test]
    fn records_after_last_within_one_page_are_rejected() {
        let mut w = Walk::new();
        let page = [
            MappedExtent::new(0, MIB, LAST),
            MappedExtent::new(MIB, MIB, FIEMAP_EXTENT_UNKNOWN),
        ];
        assert!(w.ingest(&page).is_err(), "trailing records must not be ignored");
    }

    /// A failed ingest leaves cursor/total partially advanced, so the walk
    /// must be unusable afterwards rather than silently continuing from a
    /// half-applied state.
    #[test]
    fn a_failed_ingest_poisons_the_walk() {
        let mut w = Walk::new();
        assert!(w.ingest(&[MappedExtent::new(0, 0, 0)]).is_err());
        assert!(w.ingest(&[MappedExtent::new(0, MIB, LAST)]).is_err(), "must stay poisoned");
        assert!(w.finish().is_err());
    }

    /// One bad snapshot must not blank the whole page.
    #[test]
    fn measure_all_reports_failures_without_losing_successes() {
        let ids = vec![
            "snap-000001".to_string(),
            "snap-000002".to_string(),
            "snap-000003".to_string(),
        ];
        let (ok, failed) = measure_all_with(&ids, |id| {
            if id == "snap-000002" {
                bail!("extent map incomplete")
            } else {
                Ok(4 * MIB)
            }
        });
        assert_eq!(
            ok,
            vec![
                ("snap-000001".to_string(), 4 * MIB),
                ("snap-000003".to_string(), 4 * MIB)
            ]
        );
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].0, "snap-000002");
    }

    /// Deleting ONE snapshot can change every other row's number, because
    /// an extent shared only with the deleted snapshot becomes exclusive
    /// to its remaining neighbour. Invalidation must therefore be total.
    /// A block held by several snapshots is freed only when the LAST one
    /// goes. Deleting oldest-first, that means it lands entirely on the
    /// newest holder — nothing before it, everything from it onward.
    #[test]
    fn shared_block_frees_only_when_its_last_holder_is_deleted() {
        // Three snapshots all holding the same 10 MiB block.
        let snaps = vec![
            vec![(0u64, 10 * MIB)],
            vec![(0u64, 10 * MIB)],
            vec![(0u64, 10 * MIB)],
        ];
        let got = cumulative_reclaim(&snaps, &[]);
        assert_eq!(
            got,
            vec![0, 0, 10 * MIB],
            "deleting the first two frees nothing; the third releases it",
        );
    }

    /// Blocks the live dashcam drive still references never come back,
    /// however many snapshots are deleted.
    #[test]
    fn blocks_still_held_by_the_live_disk_are_never_counted() {
        let snaps = vec![vec![(0u64, 8 * MIB)], vec![(0u64, 8 * MIB)]];
        let live = [(0u64, 8 * MIB)];
        assert_eq!(cumulative_reclaim(&snaps, &live), vec![0, 0]);
    }

    /// The real shape on Scott's device: 99.5% of extents shared, so
    /// per-snapshot exclusivity is ~0 while the set collectively holds a
    /// large amount. The cumulative curve must still reach that total —
    /// this is exactly the case the old per-row metric reported as
    /// kilobytes.
    #[test]
    fn heavy_sharing_reads_as_zero_per_row_but_reaches_the_full_total() {
        // One 100 MiB block held by all four; each also has a 1 MiB
        // sliver of its own.
        let shared = (0u64, 100 * MIB);
        let snaps = vec![
            vec![shared, (1000 * MIB, MIB)],
            vec![shared, (1001 * MIB, MIB)],
            vec![shared, (1002 * MIB, MIB)],
            vec![shared, (1003 * MIB, MIB)],
        ];
        let got = cumulative_reclaim(&snaps, &[]);
        assert_eq!(got[0], MIB, "deleting just the oldest frees only its sliver");
        assert_eq!(got[1], 2 * MIB);
        assert_eq!(got[2], 3 * MIB);
        assert_eq!(
            got[3],
            104 * MIB,
            "deleting all of them finally releases the shared block too",
        );
    }

    /// Partial overlaps: snapshots can share part of a range, not all of
    /// it. The sweep must split at the boundary rather than assigning the
    /// whole extent to one holder.
    #[test]
    fn partially_overlapping_ranges_split_at_the_boundary() {
        let snaps = vec![vec![(0u64, 10 * MIB)], vec![(4 * MIB, 10 * MIB)]];
        let got = cumulative_reclaim(&snaps, &[]);
        // 0-4 MiB belongs to snap 0 alone; 4-14 MiB's last holder is snap 1.
        assert_eq!(got, vec![4 * MIB, 14 * MIB]);
    }

    #[test]
    fn cumulative_is_monotonic_and_zero_for_no_snapshots() {
        assert!(cumulative_reclaim(&[], &[]).is_empty());
        let snaps = vec![vec![(0u64, MIB)], vec![(MIB, MIB)], vec![(2 * MIB, MIB)]];
        let got = cumulative_reclaim(&snaps, &[]);
        assert_eq!(got, vec![MIB, 2 * MIB, 3 * MIB]);
        assert!(got.windows(2).all(|w| w[1] >= w[0]), "must never decrease");
    }

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn invalidate_clears_every_entry_not_just_one() {
        let mut c = SizeCache::new();
        let set = ids(&["snap-000001", "snap-000002"]);
        c.publish(
            c.generation(),
            set.clone(),
            vec![("snap-000001".into(), (MIB, ID)), ("snap-000002".into(), (2 * MIB, ID))],
            1_000,
        );
        assert_eq!(c.get_if_same("snap-000001", ID), Some(MIB));
        c.invalidate();
        assert_eq!(c.get_if_same("snap-000001", ID), None);
        assert_eq!(c.get_if_same("snap-000002", ID), None);
        assert_eq!(c.computed_at(), None);
    }

    /// A refresh that started before a delete must not republish the
    /// pre-delete numbers afterwards — that would resurrect an invalidated
    /// cache and hold it "fresh" for the full max age.
    #[test]
    fn a_stale_generation_cannot_republish_after_invalidation() {
        let mut c = SizeCache::new();
        let set = ids(&["snap-000001", "snap-000002"]);
        let gen_at_start = c.generation();

        c.invalidate(); // a delete lands while the refresh is running

        let published = c.publish(gen_at_start, set.clone(), vec![("snap-000001".into(), (MIB, ID))], 1_000);
        assert!(!published, "stale result must be discarded");
        assert_eq!(c.get_if_same("snap-000001", ID), None);
        assert!(!c.is_current_for(&set, 1_000, 600));
    }

    /// Measuring the wrong set must not count as current. Snapshots are
    /// created hourly and deleted by the runtime, and neither can
    /// invalidate this process's cache.
    #[test]
    fn a_measurement_of_a_different_set_is_not_current() {
        let mut c = SizeCache::new();
        let measured = ids(&["snap-000001"]);
        c.publish(c.generation(), measured.clone(), vec![("snap-000001".into(), (MIB, ID))], 1_000);
        assert!(c.is_current_for(&measured, 1_000, 600));

        let now_there_are_two = ids(&["snap-000001", "snap-000002"]);
        assert!(
            !c.is_current_for(&now_there_are_two, 1_000, 600),
            "a new hourly snapshot must invalidate the measurement",
        );
    }

    /// An attempt where EVERY row failed is a completed state, not a
    /// pending one. Treating it as pending made the UI re-trigger a full
    /// extent-map scan on every poll, forever.
    #[test]
    fn an_all_failure_attempt_is_complete_not_pending() {
        let mut c = SizeCache::new();
        let set = ids(&["snap-000001"]);
        c.publish(c.generation(), set.clone(), vec![], 1_000);
        assert!(
            c.is_current_for(&set, 1_000, 600),
            "all rows failing is a finished measurement, not a reason to rescan",
        );
        assert_eq!(c.get_if_same("snap-000001", ID), None, "but the row still has no number");
    }

    /// A measurement is bound to an INODE, not a name. If the directory
    /// is replaced under the same snapshot id (slot reuse after an
    /// abandoned snapshot makes this reachable), the old figure must not
    /// be served for the new file — it could overstate enormously.
    #[test]
    fn a_replaced_inode_invalidates_its_cached_size() {
        let mut c = SizeCache::new();
        let set = ids(&["snap-000001"]);
        c.publish(c.generation(), set, vec![("snap-000001".into(), (5 * MIB, ID))], 1_000);
        assert_eq!(c.get_if_same("snap-000001", ID), Some(5 * MIB));

        let replaced = FileIdentity { dev: 1, ino: 999 };
        assert_eq!(
            c.get_if_same("snap-000001", replaced),
            None,
            "a different inode under the same name must not serve the old size",
        );
    }

    #[test]
    fn cache_expires_with_age() {
        let mut c = SizeCache::new();
        let set = ids(&["snap-000001"]);
        assert!(!c.is_current_for(&set, 1_000, 3_600), "never measured");
        c.publish(c.generation(), set.clone(), vec![("snap-000001".into(), (MIB, ID))], 1_000);
        assert!(c.is_current_for(&set, 1_600, 600), "exactly at the age limit");
        assert!(!c.is_current_for(&set, 1_601, 600), "past the age limit");
    }
}
