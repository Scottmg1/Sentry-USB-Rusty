//! Best-effort XFS FIEMAP accounting for reflinked snapshot images.
//!
//! Results describe currently unshared data-fork extents, not guaranteed
//! `statvfs` gain after unlink. Hard links are rejected; open handles delay
//! release; self-reflinks undercount; COW staging and concurrent refcount
//! changes are outside the walk. Measurements belong in background work.

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
    /// Physical byte offset used to correlate sharing across files.
    pub physical: u64,
    pub length: u64,
    pub flags: u32,
}

impl MappedExtent {
    /// Creates a logically placed extent for tests.
    pub fn new(logical: u64, length: u64, flags: u32) -> Self {
        Self { logical, physical: 0, length, flags }
    }

    pub fn at(logical: u64, physical: u64, length: u64, flags: u32) -> Self {
        Self { logical, physical, length, flags }
    }
}

/// A half-open physical byte range `[start, start + len)`.
pub type PhysicalRange = (u64, u64);

/// Returns cumulative bytes freed by deleting each oldest-first prefix.
/// Ranges are attributed to their newest snapshot holder and excluded when
/// an external file, such as the live image, still references them.
pub fn cumulative_reclaim(
    ordered: &[Vec<PhysicalRange>],
    external: &[PhysicalRange],
) -> Vec<u64> {
    // Sweep physical intervals, tracking external coverage and newest holder.
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

    // Bytes attributed to each range's last snapshot holder.
    let mut owned_by = vec![0u64; ordered.len()];
    let mut external_depth: i64 = 0;
    // Ordered keys expose the newest active snapshot.
    let mut active: std::collections::BTreeMap<usize, i64> = std::collections::BTreeMap::new();
    let mut prev_pos = events[0].pos;

    let mut i = 0;
    while i < events.len() {
        let pos = events[i].pos;

        // Account for the preceding span before applying boundary events.
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

    // Prefix totals represent deleting snapshots 0..=k.
    let mut out = Vec::with_capacity(ordered.len());
    let mut running = 0u64;
    for bytes in owned_by {
        running = running.saturating_add(bytes);
        out.push(running);
    }
    out
}

/// Validates and accumulates paged FIEMAP records.
#[derive(Debug, Default)]
pub struct Walk {
    total: u64,
    cursor: u64,
    done: bool,
    poisoned: bool,
    ranges: Vec<PhysicalRange>,
}

/// The extent map exceeded the bounded FIEMAP round-trip budget.
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

    /// Folds one page while rejecting overflow and malformed ordering.
    pub fn ingest(&mut self, page: &[MappedExtent]) -> Result<()> {
        if self.poisoned {
            bail!("FIEMAP walk was poisoned by an earlier error");
        }
        if self.done {
            self.poisoned = true;
            bail!("FIEMAP walk already terminated; refusing further records");
        }
        // Any partial ingest error permanently poisons the walk.
        let r = self.ingest_inner(page);
        if r.is_err() {
            self.poisoned = true;
        }
        r
    }

    /// Accepts a zero-record page only for a pristine empty/all-hole file.
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

            // Unwritten extents still hold blocks; holes do not appear in FIEMAP.
            if e.flags & FIEMAP_EXTENT_SHARED == 0 {
                self.total = self
                    .total
                    .checked_add(e.length)
                    .context("FIEMAP exclusive-byte total overflows u64")?;
            }

            self.ranges.push((e.physical, e.length));

            self.cursor = end;
            if e.flags & FIEMAP_EXTENT_LAST != 0 {
                // LAST must terminate the page; trailing records invalidate it.
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

    /// Returns the total only after a terminated walk.
    pub fn finish(self) -> Result<u64> {
        self.check_complete()?;
        Ok(self.total)
    }

    /// Returns physical ranges only after a terminated walk.
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

/// Measures snapshots independently, returning successes and failures separately.
/// Stable device/inode identity prevents stale values after slot reuse.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FileIdentity {
    pub dev: u64,
    pub ino: u64,
}

/// Reads identity without following the final symlink.
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

/// Cache for a complete snapshot set; any set change invalidates every value.
#[derive(Debug, Default)]
pub struct SizeCache {
    entries: std::collections::HashMap<String, (u64, FileIdentity)>,
    computed_at: Option<u64>,
    /// Exact snapshot set measured.
    measured_ids: Vec<String>,
    /// Generation preventing stale in-flight publication.
    generation: u64,
    /// Distinguishes a completed all-failure attempt from pending work.
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

    /// Publishes only if the captured generation remains current.
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

    /// Drops all values after any snapshot-set change.
    pub fn invalidate(&mut self) {
        self.entries.clear();
        self.measured_ids.clear();
        self.computed_at = None;
        self.attempted = false;
        self.generation = self.generation.wrapping_add(1);
    }

    /// Returns a value only when the current file identity still matches.
    pub fn get_if_same(&self, id: &str, now: FileIdentity) -> Option<u64> {
        match self.entries.get(id) {
            Some((bytes, ident)) if *ident == now => Some(*bytes),
            _ => None,
        }
    }

    pub fn computed_at(&self) -> Option<u64> {
        self.computed_at
    }

    /// Checks completion, exact set identity, and age.
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

    /// `_IOWR('f', 11, struct fiemap)`, shared by ARM32 and ARM64.
    const FS_IOC_FIEMAP: libc::c_ulong = 0xC020_660B;

    /// `XFS_SUPER_MAGIC`, valid across `__fsword_t` widths.
    const XFS_SUPER_MAGIC: i64 = 0x5846_5342;

    /// Records per ioctl, approximately 112 KiB.
    const EXTENTS_PER_CALL: u32 = 2048;

    /// Round-trip ceiling preventing pathological fragmentation from pinning a worker.
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

    // Catch FIEMAP ABI drift at compile time.
    const _: () = assert!(std::mem::size_of::<FiemapHeader>() == 32);
    const _: () = assert!(std::mem::size_of::<FiemapExtent>() == 56);
    const _: () = assert!(std::mem::align_of::<FiemapHeader>() == 8);
    const _: () = assert!(std::mem::align_of::<FiemapExtent>() == 8);

    /// Restricts accounting to XFS, whose FIEMAP allocation semantics are assumed.
    fn is_xfs(fd: libc::c_int) -> Result<bool> {
        // SAFETY: `fstatfs` writes only the supplied output struct.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::fstatfs(fd, &mut st) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("fstatfs");
        }
        Ok(st.f_type as i64 == XFS_SUPER_MAGIC)
    }

    /// Totals unshared extents without forcing per-page inode writeback.
    /// Returns physical ranges with the same validation as [`exclusive_bytes`].
    pub fn extent_map(path: &Path) -> Result<Vec<PhysicalRange>> {
        walk_file(path)?.finish_map()
    }

    pub fn exclusive_bytes(path: &Path) -> Result<u64> {
        walk_file(path)?.finish()
    }

    fn walk_file(path: &Path) -> Result<Walk> {
        // O_NOFOLLOW covers only the final component; reject symlinked parents too.
        if let Some(parent) = path.parent() {
            let pmeta = std::fs::symlink_metadata(parent)
                .with_context(|| format!("stat {}", parent.display()))?;
            if pmeta.file_type().is_symlink() {
                bail!("{} is a symlink — refusing to measure through it", parent.display());
            }
        }

        // O_NOFOLLOW prevents measuring a symlink target; O_NONBLOCK prevents
        // a FIFO from wedging the worker before the regular-file check.
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

        // Unlinking one of multiple hard links cannot reclaim the inode.
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
        // u64 backing guarantees 8-byte alignment for headers and records.
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

            // SAFETY: the buffer is header-sized and 8-byte aligned.
            unsafe {
                *buf.as_mut_ptr().cast::<FiemapHeader>() = FiemapHeader {
                    fm_start: start,
                    // Include post-EOF preallocation; the kernel clamps to `s_maxbytes`.
                    fm_length: length,
                    fm_flags: 0,
                    fm_mapped_extents: 0,
                    fm_extent_count: EXTENTS_PER_CALL,
                    fm_reserved: 0,
                };
            }

            loop {
                // SAFETY: the buffer holds exactly `fm_extent_count` records.
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

            // SAFETY: a successful ioctl populated the header.
            let mapped = unsafe { (*buf.as_ptr().cast::<FiemapHeader>()).fm_mapped_extents };
            // Defend against an invalid kernel count before reading records.
            if mapped > EXTENTS_PER_CALL {
                bail!(
                    "FIEMAP on {} reported {} extents for a {}-record buffer",
                    path.display(),
                    mapped,
                    EXTENTS_PER_CALL
                );
            }
            if mapped == 0 {
                // A pristine empty page is empty/all-hole; later empty pages fail closed.
                walk.ingest_empty_page()?;
                break;
            }

            let mut page = Vec::with_capacity(mapped as usize);
            for i in 0..mapped as usize {
                // SAFETY: `i` is within the validated mapped-record count.
                let e = unsafe {
                    let base = buf.as_ptr().cast::<u8>().add(hdr_size);
                    *base.add(i * ext_size).cast::<FiemapExtent>()
                };
                page.push(MappedExtent::at(e.fe_logical, e.fe_physical, e.fe_length, e.fe_flags));
            }
            walk.ingest(&page)?;
        }

        if !walk.is_done() {
            // Classify deterministic fragmentation separately for caller backoff.
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
    const ID: FileIdentity = FileIdentity { dev: 1, ino: 2 };
    const UNWRITTEN: u32 = 0x0800;
    const LAST: u32 = FIEMAP_EXTENT_LAST;

    fn walk_all(page: &[MappedExtent]) -> Result<u64> {
        let mut w = Walk::new();
        w.ingest(page)?;
        w.finish()
    }

    #[test]
    fn shared_extents_are_not_reclaimable() {
        let page = [
            MappedExtent::new(0, 100 * MIB, FIEMAP_EXTENT_SHARED),
            MappedExtent::new(100 * MIB, 5 * MIB, 0),
            MappedExtent::new(105 * MIB, 200 * MIB, FIEMAP_EXTENT_SHARED | LAST),
        ];
        assert_eq!(walk_all(&page).unwrap(), 5 * MIB);
    }

    #[test]
    fn fully_shared_clone_reclaims_nothing() {
        let page = [
            MappedExtent::new(0, 45 * 1024 * MIB, FIEMAP_EXTENT_SHARED),
            MappedExtent::new(45 * 1024 * MIB, 19 * 1024 * MIB, FIEMAP_EXTENT_SHARED | LAST),
        ];
        assert_eq!(walk_all(&page).unwrap(), 0);
    }

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

    #[test]
    fn unterminated_walk_refuses_to_report_a_partial_total() {
        let mut w = Walk::new();
        w.ingest(&[MappedExtent::new(0, 10 * MIB, 0)]).unwrap();
        assert!(!w.is_done());
        let err = w.finish().unwrap_err().to_string();
        assert!(err.contains("partial"), "unexpected error: {}", err);
    }

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

    #[test]
    fn fully_shared_terminated_map_is_zero() {
        assert_eq!(walk_all(&[MappedExtent::new(0, MIB, FIEMAP_EXTENT_SHARED | LAST)]).unwrap(), 0);
    }

    #[test]
    fn empty_map_on_a_pristine_walk_is_a_valid_zero() {
        let mut w = Walk::new();
        w.ingest_empty_page().unwrap();
        assert!(w.is_done());
        assert_eq!(w.finish().unwrap(), 0);
    }

    #[test]
    fn empty_page_after_real_extents_is_an_error() {
        let mut w = Walk::new();
        w.ingest(&[MappedExtent::new(0, MIB, 0)]).unwrap();
        assert!(w.ingest_empty_page().is_err());
    }

    #[test]
    fn records_after_last_within_one_page_are_rejected() {
        let mut w = Walk::new();
        let page = [
            MappedExtent::new(0, MIB, LAST),
            MappedExtent::new(MIB, MIB, FIEMAP_EXTENT_UNKNOWN),
        ];
        assert!(w.ingest(&page).is_err(), "trailing records must not be ignored");
    }

    #[test]
    fn a_failed_ingest_poisons_the_walk() {
        let mut w = Walk::new();
        assert!(w.ingest(&[MappedExtent::new(0, 0, 0)]).is_err());
        assert!(w.ingest(&[MappedExtent::new(0, MIB, LAST)]).is_err(), "must stay poisoned");
        assert!(w.finish().is_err());
    }

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

    #[test]
    fn shared_block_frees_only_when_its_last_holder_is_deleted() {
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

    #[test]
    fn blocks_still_held_by_the_live_disk_are_never_counted() {
        let snaps = vec![vec![(0u64, 8 * MIB)], vec![(0u64, 8 * MIB)]];
        let live = [(0u64, 8 * MIB)];
        assert_eq!(cumulative_reclaim(&snaps, &live), vec![0, 0]);
    }

    #[test]
    fn heavy_sharing_reads_as_zero_per_row_but_reaches_the_full_total() {
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

    #[test]
    fn partially_overlapping_ranges_split_at_the_boundary() {
        let snaps = vec![vec![(0u64, 10 * MIB)], vec![(4 * MIB, 10 * MIB)]];
        let got = cumulative_reclaim(&snaps, &[]);
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

    #[test]
    fn a_stale_generation_cannot_republish_after_invalidation() {
        let mut c = SizeCache::new();
        let set = ids(&["snap-000001", "snap-000002"]);
        let gen_at_start = c.generation();

        c.invalidate();

        let published = c.publish(gen_at_start, set.clone(), vec![("snap-000001".into(), (MIB, ID))], 1_000);
        assert!(!published, "stale result must be discarded");
        assert_eq!(c.get_if_same("snap-000001", ID), None);
        assert!(!c.is_current_for(&set, 1_000, 600));
    }

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
