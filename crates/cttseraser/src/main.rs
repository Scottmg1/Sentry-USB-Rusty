//! Read-only FUSE filesystem that erases the `ctts` FourCC tag inside MP4
//! files on the fly so Chromium-based browsers can play Tesla recordings.
//! Port of fuse/cttseraser.cpp (MIT, Marco Nelissen, 2021).

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("cttseraser only builds on Linux (requires FUSE)");
    std::process::exit(1);
}

#[cfg(target_os = "linux")]
fn main() {
    linux_impl::run();
}

#[cfg(target_os = "linux")]
mod linux_impl {
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::{Duration, UNIX_EPOCH};

    use fuser::{FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData,
                 ReplyDirectory, ReplyEntry, ReplyOpen, Request};

    const TTL: Duration = Duration::from_secs(1);

    struct Opened {
        file: std::fs::File,
        ctts_offset: u64, // absolute file offset of the ctts chunk header, 0 if none
    }

    pub struct CttsFs {
        source: PathBuf,
        inodes: Mutex<InodeTable>,
        open_handles: Mutex<HashMap<u64, Opened>>,
        next_fh: std::sync::atomic::AtomicU64,
    }

    struct InodeTable {
        by_ino: HashMap<u64, PathBuf>,
        by_path: HashMap<PathBuf, u64>,
        next_ino: u64,
    }

    impl InodeTable {
        fn new(root: PathBuf) -> Self {
            let mut by_ino = HashMap::new();
            let mut by_path = HashMap::new();
            by_ino.insert(1, root.clone());
            by_path.insert(root, 1);
            Self { by_ino, by_path, next_ino: 2 }
        }

        fn get_or_insert(&mut self, path: PathBuf) -> u64 {
            if let Some(ino) = self.by_path.get(&path) {
                return *ino;
            }
            let ino = self.next_ino;
            self.next_ino += 1;
            self.by_ino.insert(ino, path.clone());
            self.by_path.insert(path, ino);
            ino
        }

        fn path(&self, ino: u64) -> Option<PathBuf> {
            self.by_ino.get(&ino).cloned()
        }
    }

    impl CttsFs {
        fn new(source: PathBuf) -> Self {
            Self {
                source: source.clone(),
                inodes: Mutex::new(InodeTable::new(source)),
                open_handles: Mutex::new(HashMap::new()),
                next_fh: std::sync::atomic::AtomicU64::new(1),
            }
        }

        fn stat_to_attr(ino: u64, md: &std::fs::Metadata) -> FileAttr {
            let kind = if md.is_dir() {
                FileType::Directory
            } else if md.file_type().is_symlink() {
                FileType::Symlink
            } else {
                FileType::RegularFile
            };
            FileAttr {
                ino,
                size: md.len(),
                blocks: md.blocks(),
                atime: UNIX_EPOCH + Duration::from_secs(md.atime() as u64),
                mtime: UNIX_EPOCH + Duration::from_secs(md.mtime() as u64),
                ctime: UNIX_EPOCH + Duration::from_secs(md.ctime() as u64),
                crtime: UNIX_EPOCH,
                kind,
                perm: (md.mode() & 0o7777) as u16,
                nlink: md.nlink() as u32,
                uid: md.uid(),
                gid: md.gid(),
                rdev: md.rdev() as u32,
                flags: 0,
                blksize: md.blksize() as u32,
            }
        }
    }

    /// Read a big-endian u32 from `file` at `offset`. Returns None on short read.
    fn read_be_u32_at(file: &std::fs::File, offset: u64) -> Option<u32> {
        use std::os::unix::fs::FileExt;
        let mut buf = [0u8; 4];
        file.read_exact_at(&mut buf, offset).ok()?;
        Some(u32::from_be_bytes(buf))
    }

    /// Read an MP4 chunk header (size, fourcc) at `offset`.
    fn get_chunk(file: &std::fs::File, offset: u64) -> Option<(i32, u32)> {
        let size = read_be_u32_at(file, offset)? as i32;
        let fourcc = read_be_u32_at(file, offset + 4)?;
        Some((size, fourcc))
    }

    const fn fourcc(tag: &[u8; 4]) -> u32 {
        ((tag[0] as u32) << 24) | ((tag[1] as u32) << 16) | ((tag[2] as u32) << 8) | (tag[3] as u32)
    }

    fn parse_chunks(file: &std::fs::File, start: u64, end: u64) -> Option<u64> {
        let mut cur = start;
        while cur + 8 <= end {
            let (size, tag) = get_chunk(file, cur)?;
            if size <= 0 {
                return Some(0);
            }
            match tag {
                t if t == fourcc(b"ctts") => return Some(cur),
                t if t == fourcc(b"moov")
                    || t == fourcc(b"trak")
                    || t == fourcc(b"mdia")
                    || t == fourcc(b"minf")
                    || t == fourcc(b"stbl") =>
                {
                    if let Some(off) = parse_chunks(file, cur + 8, cur + size as u64) {
                        if off > 0 {
                            return Some(off);
                        }
                    } else {
                        return None;
                    }
                }
                _ => {}
            }
            cur = cur.saturating_add(size as u64);
        }
        Some(0)
    }

    fn find_ctts(file: &std::fs::File) -> u64 {
        let (size, tag) = match get_chunk(file, 0) {
            Some(v) => v,
            None => return 0,
        };
        if tag != fourcc(b"ftyp") || size <= 0 {
            return 0;
        }
        parse_chunks(file, size as u64, u64::MAX / 2).unwrap_or(0)
    }

    impl Filesystem for CttsFs {
        fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
            let parent_path = match self.inodes.lock().unwrap().path(parent) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            };
            let full = parent_path.join(name);
            let md = match std::fs::symlink_metadata(&full) {
                Ok(m) => m,
                Err(_) => return reply.error(libc::ENOENT),
            };
            let ino = self.inodes.lock().unwrap().get_or_insert(full);
            reply.entry(&TTL, &Self::stat_to_attr(ino, &md), 0);
        }

        fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
            let path = match self.inodes.lock().unwrap().path(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            };
            let md = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => return reply.error(libc::ENOENT),
            };
            reply.attr(&TTL, &Self::stat_to_attr(ino, &md));
        }

        fn opendir(&mut self, _req: &Request, ino: u64, _flags: i32, reply: ReplyOpen) {
            if self.inodes.lock().unwrap().path(ino).is_none() {
                return reply.error(libc::ENOENT);
            }
            reply.opened(0, 0);
        }

        fn readdir(
            &mut self,
            _req: &Request,
            ino: u64,
            _fh: u64,
            offset: i64,
            mut reply: ReplyDirectory,
        ) {
            let path = match self.inodes.lock().unwrap().path(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            };

            let mut entries: Vec<(OsString, FileType, PathBuf)> = Vec::new();
            entries.push((OsString::from("."), FileType::Directory, path.clone()));
            let parent = path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| path.clone());
            entries.push((OsString::from(".."), FileType::Directory, parent));

            let iter = match std::fs::read_dir(&path) {
                Ok(i) => i,
                Err(_) => return reply.error(libc::EIO),
            };
            for ent in iter.flatten() {
                let ft = ent.file_type().ok();
                let kind = match ft {
                    Some(t) if t.is_dir() => FileType::Directory,
                    Some(t) if t.is_symlink() => FileType::Symlink,
                    _ => FileType::RegularFile,
                };
                entries.push((ent.file_name(), kind, ent.path()));
            }

            let mut table = self.inodes.lock().unwrap();
            for (i, (name, kind, full)) in entries.into_iter().enumerate().skip(offset as usize) {
                let child_ino = table.get_or_insert(full);
                if reply.add(child_ino, (i + 1) as i64, kind, &name) {
                    break;
                }
            }
            reply.ok();
        }

        fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
            if (flags & (libc::O_WRONLY | libc::O_RDWR)) != 0 {
                return reply.error(libc::EACCES);
            }
            let path = match self.inodes.lock().unwrap().path(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            };
            let file = match std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_RDONLY)
                .open(&path)
            {
                Ok(f) => f,
                Err(e) => {
                    let ec = e.raw_os_error().unwrap_or(libc::EIO);
                    return reply.error(ec);
                }
            };
            let ctts = find_ctts(&file);
            let fh = self
                .next_fh
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.open_handles.lock().unwrap().insert(
                fh,
                Opened { file, ctts_offset: ctts },
            );
            reply.opened(fh, 0);
        }

        fn read(
            &mut self,
            _req: &Request,
            _ino: u64,
            fh: u64,
            offset: i64,
            size: u32,
            _flags: i32,
            _lock_owner: Option<u64>,
            reply: ReplyData,
        ) {
            use std::os::unix::fs::FileExt;
            let handles = self.open_handles.lock().unwrap();
            let opened = match handles.get(&fh) {
                Some(o) => o,
                None => return reply.error(libc::EBADF),
            };
            let mut buf = vec![0u8; size as usize];
            let n = match opened.file.read_at(&mut buf, offset as u64) {
                Ok(n) => n,
                Err(e) => return reply.error(e.raw_os_error().unwrap_or(libc::EIO)),
            };
            buf.truncate(n);

            // Erase the ctts FourCC if it overlaps this read range.
            let ctts = opened.ctts_offset;
            if ctts > 0 {
                let tag_start = ctts + 4;
                let tag_end = tag_start + 4;
                let s = offset.max(tag_start as i64) as u64;
                let e = ((offset + n as i64) as u64).min(tag_end);
                if e > s {
                    let begin = (s - offset as u64) as usize;
                    let end = (e - offset as u64) as usize;
                    for b in &mut buf[begin..end] {
                        *b = b'@';
                    }
                }
            }

            reply.data(&buf);
        }

        fn release(
            &mut self,
            _req: &Request,
            _ino: u64,
            fh: u64,
            _flags: i32,
            _lock_owner: Option<u64>,
            _flush: bool,
            reply: fuser::ReplyEmpty,
        ) {
            self.open_handles.lock().unwrap().remove(&fh);
            reply.ok();
        }
    }

    pub fn run() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "cttseraser=info".into()),
            )
            .init();

        let mut args = std::env::args_os().skip(1);
        let source = match args.next() {
            Some(p) => PathBuf::from(p),
            None => {
                eprintln!("usage: cttseraser <sourcedir> <mountpoint> [FUSE options]");
                std::process::exit(1);
            }
        };
        let mountpoint = match args.next() {
            Some(p) => PathBuf::from(p),
            None => {
                eprintln!("usage: cttseraser <sourcedir> <mountpoint> [FUSE options]");
                std::process::exit(1);
            }
        };

        if std::fs::symlink_metadata(&source).is_err() {
            eprintln!("source does not exist: {}", source.display());
            std::process::exit(1);
        }

        let mut options = vec![
            MountOption::FSName("cttseraser".to_string()),
            MountOption::Subtype("cttseraser".to_string()),
            MountOption::RO,
            MountOption::AllowOther,
            MountOption::DefaultPermissions,
        ];
        // Pass through any remaining CLI flags as generic FUSE options (e.g. `-f`).
        for arg in args {
            let s = arg.to_string_lossy().to_string();
            if s == "-f" || s == "--foreground" {
                continue; // fuser runs in foreground by default
            }
            // Accept `-o key=val` style from callers.
            if s.starts_with("-o") {
                continue;
            }
            options.push(MountOption::CUSTOM(s));
        }

        let fs = CttsFs::new(source);
        if let Err(e) = fuser::mount2(fs, &mountpoint, &options) {
            eprintln!("mount failed: {}", e);
            std::process::exit(1);
        }
    }

    // Prevents "unused" warning on non-Linux builds of this helper.
    #[allow(dead_code)]
    fn _silence_unused(p: &Path) -> &OsStr {
        p.as_os_str()
    }
}
