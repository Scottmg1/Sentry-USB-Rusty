//! Cross-process ownership of the `/mnt/archive` network mount.
//!
//! Archiveloop and on-demand backups share this flock so neither unmounts the
//! archive share during the other's mount transition or write transaction.
//! Keep [`ARCHIVE_MOUNT_LOCK_PATH`] synchronized with the CIFS/NFS scripts.
//!
//! Archiveloop holds it only for mount/unmount transitions, not the full cycle,
//! because its post-archive phase may invoke the backup API.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Must match `ARCHIVE_MOUNT_LOCK` in run/{cifs,nfs}_archive/
/// connect-archive.sh and disconnect-archive.sh.
pub const ARCHIVE_MOUNT_LOCK_PATH: &str = "/tmp/sentryusb_archive_mount.lock";

/// Exclusive archive-mount lock, released when its file handle is dropped.
#[derive(Debug)]
pub struct ArchiveMountGuard {
    _file: File,
}

/// Acquire the flock within `timeout` by polling `LOCK_NB`.
/// This is a blocking call and must run on a blocking thread.
pub fn acquire(timeout: Duration) -> io::Result<ArchiveMountGuard> {
    acquire_path(Path::new(ARCHIVE_MOUNT_LOCK_PATH), timeout)
}

fn acquire_path(path: &Path, timeout: Duration) -> io::Result<ArchiveMountGuard> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false) // content is irrelevant; the flock is the point
        .open(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        if try_flock_exclusive(&file)? {
            return Ok(ArchiveMountGuard { _file: file });
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "archive mount lock held elsewhere for over {}s (archive connect/disconnect in progress)",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(unix)]
fn try_flock_exclusive(file: &File) -> io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    // The open-file-description lock excludes processes and sibling threads.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EWOULDBLOCK) | Some(libc::EINTR) => Ok(false),
        _ => Err(err),
    }
}

#[cfg(not(unix))]
fn try_flock_exclusive(_file: &File) -> io::Result<bool> {
    Ok(true) // no archiveloop to race on non-unix dev hosts
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_times_out_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.lock");
        let g1 = acquire_path(&path, Duration::from_millis(0)).unwrap();
        let err = acquire_path(&path, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(g1);
        let _g2 = acquire_path(&path, Duration::from_millis(0)).unwrap();
    }
}
