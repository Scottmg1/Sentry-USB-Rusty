//! Cross-process serialization of USB-gadget disable/enable cycles.
//!
//! Hold the shared archiveloop flock across the complete disable/work/enable
//! window. Interleaving can expose a mounted image to the car and corrupt it.
//!
//! Do not acquire it inside [`crate::enable`] or [`crate::disable`]: shell
//! shims call those endpoints while already holding the same non-reentrant
//! lock. Only complete-cycle callers acquire it.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Must match `GADGET_CYCLE_LOCK` in `run/archiveloop`.
pub const GADGET_CYCLE_LOCK_PATH: &str = "/tmp/sentryusb_gadget_cycle.lock";

/// Exclusive gadget-cycle lock, released when its file handle is dropped.
#[derive(Debug)]
pub struct CycleGuard {
    _file: File,
}

/// Acquire the flock within `timeout` by polling `LOCK_NB`.
/// This is a blocking call and must run on a blocking thread.
pub fn acquire(timeout: Duration) -> io::Result<CycleGuard> {
    acquire_path(Path::new(GADGET_CYCLE_LOCK_PATH), timeout)
}

fn acquire_path(path: &Path, timeout: Duration) -> io::Result<CycleGuard> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path)?;
    let deadline = Instant::now() + timeout;
    loop {
        if try_flock_exclusive(&file)? {
            return Ok(CycleGuard { _file: file });
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "gadget cycle lock held elsewhere for over {}s (archive sync or stall recovery in progress)",
                    timeout.as_secs()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(unix)]
pub(crate) fn try_flock_exclusive(file: &File) -> io::Result<bool> {
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
pub(crate) fn try_flock_exclusive(_file: &File) -> io::Result<bool> {
    Ok(true) // no archiveloop to race on non-unix dev hosts
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_times_out_while_held_then_succeeds_after_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle.lock");
        let g1 = acquire_path(&path, Duration::from_millis(0)).unwrap();
        let err = acquire_path(&path, Duration::from_millis(300)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(g1);
        let _g2 = acquire_path(&path, Duration::from_millis(0)).unwrap();
    }

    #[test]
    fn acquire_waits_for_release() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cycle.lock");
        let g1 = acquire_path(&path, Duration::from_millis(0)).unwrap();
        let p2 = path.clone();
        let waiter =
            std::thread::spawn(move || acquire_path(&p2, Duration::from_secs(10)).is_ok());
        std::thread::sleep(Duration::from_millis(400));
        drop(g1);
        assert!(waiter.join().unwrap());
    }
}
