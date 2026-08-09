//! Path-based read-only API over the vendored XFS parser.
//!
//! This replaces upstream xfuse's FUSE `Filesystem` impl (`volume.rs`) with
//! plain functions: `read_dir`, `stat`, and an `XfsFile` that implements
//! `Read + Seek` so higher layers (MBR/exFAT parsing of `snap.bin`) can
//! stack on top of it.

use std::{
    ffi::OsStr,
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path},
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, bail, Context, Result};

use super::{
    dinode::Dinode,
    dir3::Dir3,
    reader::BlockReader,
    sb::Sb,
    shim::FileType,
    volume::SUPERBLOCK,
};

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub name: String,
    pub kind: Option<FileType>,
    pub ino: u64,
}

pub struct XfsFs<R: Read + Seek> {
    device: BlockReader<R>,
    sb: Sb,
}

impl<R: Read + Seek> XfsFs<R> {
    /// Open an XFS volume from any `Read + Seek` (image file or partition
    /// window). `size` is the volume length in bytes.
    ///
    /// Note: the vendored parser keeps the superblock in a process-wide
    /// global, so only one XFS volume may be open per process. A second
    /// open of a *different* volume fails.
    pub fn open(inner: R, size: u64) -> Result<Self> {
        let mut device = BlockReader::new(inner, size);
        // Sb::from panics on bad magic/CRC/unknown incompat features. Catch
        // it so callers get a proper error for non-XFS partitions. The
        // parser holds no state that could be corrupted by the unwind.
        let sb = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Sb::from(&mut device)
        }))
        .map_err(|p| {
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown parser panic".into());
            anyhow!("not a readable XFS volume: {msg}")
        })?;

        if let Some(existing) = SUPERBLOCK.get() {
            if existing.sb_uuid != sb.sb_uuid {
                bail!("a different XFS volume is already open in this process");
            }
        } else {
            let _ = SUPERBLOCK.set(sb);
        }
        Ok(Self { device, sb })
    }

    pub fn superblock(&self) -> &Sb {
        &self.sb
    }

    /// Walk `path` (absolute or relative to the root) to its inode.
    fn dinode_at(&mut self, path: &Path) -> Result<(u64, Dinode)> {
        let mut ino = self.sb.sb_rootino;
        let mut dinode = Dinode::from(&mut self.device, &self.sb, ino);
        for comp in path.components() {
            let name: &OsStr = match comp {
                Component::RootDir => continue,
                Component::CurDir => continue,
                Component::Normal(n) => n,
                other => bail!("unsupported path component {other:?}"),
            };
            let dirsize = (self.sb.sb_blocksize << self.sb.sb_dirblklog) as usize;
            self.device.set_bufsize(dirsize);
            let dir = dinode.get_dir(&mut self.device, &self.sb);
            self.device.set_bufsize(self.sb.inode_size());
            ino = dir
                .lookup(&mut self.device, &self.sb, name)
                .map_err(|e| anyhow!("lookup {name:?} in {path:?}: errno {e}"))?;
            dinode = Dinode::from(&mut self.device, &self.sb, ino);
        }
        Ok((ino, dinode))
    }

    pub fn read_dir(&mut self, path: &Path) -> Result<Vec<DirEntry>> {
        let (_ino, mut dinode) = self.dinode_at(path)?;
        let dirsize = (self.sb.sb_blocksize << self.sb.sb_dirblklog) as usize;
        self.device.set_bufsize(dirsize);
        let dir = dinode.get_dir(&mut self.device, &self.sb);

        let mut entries = Vec::new();
        let mut offset = 0i64;
        loop {
            match dir.next(&mut self.device, &self.sb, offset) {
                Ok((ino, next_offset, kind, name)) => {
                    offset = next_offset;
                    let name = name.to_string_lossy().into_owned();
                    if name == "." || name == ".." {
                        continue;
                    }
                    entries.push(DirEntry { name, kind, ino });
                }
                // Short-form dirs end with -1, block/leaf dirs with ENOENT.
                Err(e) if e == super::shim::ENOENT || e == -1 => break,
                Err(e) => bail!("readdir {path:?}: errno {e}"),
            }
        }
        Ok(entries)
    }

    /// File size in bytes.
    pub fn file_size(&mut self, path: &Path) -> Result<u64> {
        let (_ino, mut dinode) = self.dinode_at(path)?;
        Ok(dinode.fsize() as u64)
    }

    /// Inode mtime. Used to order snapshots by when their image was
    /// actually frozen: `snap-NNNNNN` slot numbers are not
    /// time-monotonic in the field (a reflash can leave a stale
    /// high-numbered snapshot above a restarted sequence).
    pub fn file_mtime(&mut self, path: &Path) -> Result<std::time::SystemTime> {
        let (ino, dinode) = self.dinode_at(path)?;
        let attr = dinode
            .di_core
            .stat(ino)
            .map_err(|e| anyhow!("stat {path:?}: errno {e}"))?;
        Ok(attr.mtime)
    }

    fn read_at(&mut self, dinode: &mut Dinode, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.device.set_bufsize(self.sb.sb_blocksize as usize);
        let (buf, ignore) = dinode
            .read(
                &mut self.device,
                None,
                i64::try_from(offset).context("offset overflow")?,
                u32::try_from(len).context("read too large")?,
            )
            .map_err(|e| anyhow!("read at {offset}: errno {e}"))?;
        Ok(buf[ignore..].to_vec())
    }
}

/// A `Read + Seek` view of one regular file inside the XFS volume, sharing
/// the volume through a mutex so several files (snapshots) can be open at
/// once.
pub struct XfsFile<R: Read + Seek> {
    fs: Arc<Mutex<XfsFs<R>>>,
    dinode: Dinode,
    size: u64,
    pos: u64,
}

impl<R: Read + Seek> XfsFile<R> {
    pub fn open(fs: &Arc<Mutex<XfsFs<R>>>, path: &Path) -> Result<Self> {
        let mut guard = fs.lock().unwrap();
        let (_ino, mut dinode) = guard.dinode_at(path)?;
        if dinode.is_realtime() {
            bail!("realtime files are not supported");
        }
        let size = dinode.fsize() as u64;
        drop(guard);
        Ok(Self {
            fs: Arc::clone(fs),
            dinode,
            size,
            pos: 0,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }
}

impl<R: Read + Seek> Read for XfsFile<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size {
            return Ok(0);
        }
        let want = buf.len().min((self.size - self.pos) as usize);
        // Cap single reads; dinode.read allocates the whole range.
        let want = want.min(4 << 20);
        let data = self
            .fs
            .lock()
            .unwrap()
            .read_at(&mut self.dinode, self.pos, want)
            .map_err(|e| io::Error::other(format!("{e:#}")))?;
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for XfsFile<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let newpos = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(off) => self.size.checked_add_signed(off),
            SeekFrom::Current(off) => self.pos.checked_add_signed(off),
        };
        match newpos {
            Some(p) => {
                self.pos = p;
                Ok(p)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek out of range",
            )),
        }
    }
}
