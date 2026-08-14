//! Cross-process BLE radio ownership lock shared with `run/awake_start`.
//! The file contains `<owner>\n<unix_seconds>\n`; locks older than 24 hours
//! may be reclaimed.

use std::fs;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use tracing::{info, warn};

/// Must match `BLE_LOCK` in `run/awake_start`.
pub const LOCK_PATH: &str = "/tmp/ble_radio_owner";

/// Must match `BLE_LOCK_MAX_AGE` in `run/awake_start`.
const STALE_AFTER_SECS: i64 = 86_400;

/// Grace period for `archiveloop` to publish status before orphan detection.
const KEEP_AWAKE_ORPHAN_GRACE_SECS: i64 = 60;

/// Freshness rule must match `drives_handler::read_archive_status`.
const ARCHIVE_STATUS_PATH: &str = "/tmp/archive_status.json";
const ARCHIVE_STATUS_FRESH_SECS: u64 = 120;

/// PID file written by the Case-3 keep-awake nudge loop in awake_start.
const NUDGE_PID_FILE: &str = "/tmp/keep_awake_nudge_pid";

/// Must match `KEEP_AWAKE_WANTED_FLAG` in `drives_handler.rs`.
const WEBUI_WANTED_FLAG: &str = "/tmp/keep_awake_webui_wanted";

/// Attempts ownership. The stale check and write are not one atomic operation.
pub fn try_acquire(owner: &str) -> Result<bool> {
    try_acquire_at(
        Path::new(LOCK_PATH),
        Path::new(ARCHIVE_STATUS_PATH),
        Path::new(NUDGE_PID_FILE),
        owner,
    )
}

/// Path-parameterized core so tests cannot touch the production lock.
fn try_acquire_at(
    lock_path: &Path,
    archive_status: &Path,
    nudge_pid: &Path,
    owner: &str,
) -> Result<bool> {
    let now = now_secs();

    if lock_path.exists() {
        match read_lock(lock_path) {
            Ok((existing_owner, ts)) => {
                if existing_owner == owner {
                    // Refresh long-held ownership.
                    write_lock(lock_path, owner, now)?;
                    return Ok(true);
                }
                let age = now - ts;
                if age > STALE_AFTER_SECS {
                    warn!(
                        "BLE radio lock held by '{}' for {}s — assuming crashed, taking over",
                        existing_owner, age
                    );
                    write_lock(lock_path, owner, now)?;
                    return Ok(true);
                }
                // Reclaim abandoned keep-awake locks after the startup grace.
                if existing_owner == "keep_awake"
                    && age >= KEEP_AWAKE_ORPHAN_GRACE_SECS
                    && !is_archive_active_at(archive_status)
                    && !is_nudge_alive_at(nudge_pid)
                {
                    warn!(
                        "BLE radio lock owned by keep_awake but no active archive/nudge ({}s old) — orphan, taking over",
                        age
                    );
                    write_lock(lock_path, owner, now)?;
                    return Ok(true);
                }
                return Ok(false);
            }
            Err(e) => {
                warn!("BLE radio lock file unreadable ({}) — overwriting", e);
                write_lock(lock_path, owner, now)?;
                return Ok(true);
            }
        }
    }

    write_lock(lock_path, owner, now)?;
    info!("BLE radio lock acquired by '{}'", owner);
    Ok(true)
}

/// Releases the lock only when `owner` still holds it.
pub fn release(owner: &str) -> Result<()> {
    release_at(Path::new(LOCK_PATH), owner)
}

fn release_at(lock_path: &Path, owner: &str) -> Result<()> {
    if !lock_path.exists() {
        return Ok(());
    }
    match read_lock(lock_path) {
        Ok((existing_owner, _)) if existing_owner == owner => {
            fs::remove_file(lock_path)
                .with_context(|| format!("failed to remove {}", lock_path.display()))?;
            info!("BLE radio lock released by '{}'", owner);
        }
        Ok((other, _)) => {
            warn!(
                "BLE radio lock owned by '{}', not us ('{}') — not releasing",
                other, owner
            );
        }
        Err(e) => {
            warn!("BLE radio lock unreadable on release ({}) — leaving alone", e);
        }
    }
    Ok(())
}

/// Diagnostic owner lookup; acquisition decisions must use [`try_acquire`].
pub fn current_owner() -> Option<String> {
    current_owner_at(Path::new(LOCK_PATH))
}

fn current_owner_at(lock_path: &Path) -> Option<String> {
    read_lock(lock_path).ok().map(|(owner, _)| owner)
}

fn read_lock(lock_path: &Path) -> Result<(String, i64)> {
    let contents = fs::read_to_string(lock_path)
        .with_context(|| format!("failed to read {}", lock_path.display()))?;
    let mut lines = contents.lines();
    let owner = lines
        .next()
        .ok_or_else(|| anyhow!("lock file empty"))?
        .trim()
        .to_string();
    let ts = lines
        .next()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    Ok((owner, ts))
}

fn write_lock(lock_path: &Path, owner: &str, ts: i64) -> Result<()> {
    // Replace the file through a temporary path.
    let tmp = lock_path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        writeln!(f, "{}", owner)?;
        writeln!(f, "{}", ts)?;
    }
    fs::rename(&tmp, lock_path).with_context(|| {
        format!("failed to rename {} -> {}", tmp.display(), lock_path.display())
    })?;
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Reports whether archive status is fresh.
pub fn is_archive_active() -> bool {
    is_archive_active_at(Path::new(ARCHIVE_STATUS_PATH))
}

fn is_archive_active_at(status_path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(status_path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|d| d.as_secs() < ARCHIVE_STATUS_FRESH_SECS)
        .unwrap_or(false)
}

/// Checks nudge liveness via `/proc/<pid>`; PID reuse fails safely by retaining the lock.
fn is_nudge_alive() -> bool {
    is_nudge_alive_at(Path::new(NUDGE_PID_FILE))
}

fn is_nudge_alive_at(pid_file: &Path) -> bool {
    let Ok(pid_str) = std::fs::read_to_string(pid_file) else {
        return false;
    };
    let Ok(pid) = pid_str.trim().parse::<u32>() else {
        return false;
    };
    Path::new(&format!("/proc/{pid}")).exists()
}

/// Checks the presence-only in-process keep-awake flag.
fn is_webui_keep_awake_wanted_at(flag_path: &Path) -> bool {
    flag_path.exists()
}

/// Combines archive, nudge-loop, and in-process keep-awake signals.
pub fn keep_awake_requested() -> bool {
    is_archive_active()
        || is_nudge_alive()
        || is_webui_keep_awake_wanted_at(Path::new(WEBUI_WANTED_FLAG))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test paths isolated from the production lock.
    struct Env {
        _dir: tempfile::TempDir,
        lock: std::path::PathBuf,
        archive: std::path::PathBuf,
        nudge: std::path::PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            Env {
                lock: dir.path().join("ble_radio_owner"),
                archive: dir.path().join("archive_status.json"),
                nudge: dir.path().join("keep_awake_nudge_pid"),
                _dir: dir,
            }
        }
        fn acquire(&self, owner: &str) -> bool {
            try_acquire_at(&self.lock, &self.archive, &self.nudge, owner).unwrap()
        }
        fn owner(&self) -> Option<String> {
            current_owner_at(&self.lock)
        }
    }

    #[test]
    fn acquire_when_unheld_succeeds() {
        let env = Env::new();
        assert!(env.acquire("telemetry"));
        assert_eq!(env.owner().as_deref(), Some("telemetry"));
    }

    #[test]
    fn acquire_when_we_already_own_succeeds() {
        let env = Env::new();
        env.acquire("telemetry");
        assert!(env.acquire("telemetry"), "self-reacquire should succeed");
    }

    #[test]
    fn acquire_when_other_owner_fresh_fails() {
        let env = Env::new();
        env.acquire("keep_awake");
        assert!(!env.acquire("telemetry"));
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn acquire_steals_stale_lock() {
        let env = Env::new();
        write_lock(&env.lock, "keep_awake", now_secs() - STALE_AFTER_SECS - 1).unwrap();
        assert!(env.acquire("telemetry"), "stale lock should be stealable");
        assert_eq!(env.owner().as_deref(), Some("telemetry"));
    }

    #[test]
    fn acquire_steals_orphaned_keep_awake_lock() {
        let env = Env::new();
        // No archive status or nudge PID remains after the grace window.
        write_lock(
            &env.lock,
            "keep_awake",
            now_secs() - KEEP_AWAKE_ORPHAN_GRACE_SECS - 5,
        )
        .unwrap();
        assert!(
            env.acquire("telemetry"),
            "orphaned keep_awake lock should be stealable after grace",
        );
        assert_eq!(env.owner().as_deref(), Some("telemetry"));
    }

    #[test]
    fn acquire_waits_for_fresh_archive_even_past_grace() {
        let env = Env::new();
        // Fresh archive status protects the lock after the grace window.
        write_lock(
            &env.lock,
            "keep_awake",
            now_secs() - KEEP_AWAKE_ORPHAN_GRACE_SECS - 5,
        )
        .unwrap();
        std::fs::write(&env.archive, b"{}").unwrap();
        assert!(
            !env.acquire("telemetry"),
            "keep_awake lock with a live archive must not be stolen",
        );
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn acquire_respects_grace_window_on_keep_awake_lock() {
        let env = Env::new();
        // Do not race archiveloop before it publishes status.
        write_lock(&env.lock, "keep_awake", now_secs()).unwrap();
        assert!(
            !env.acquire("telemetry"),
            "fresh keep_awake lock should NOT be stolen during grace",
        );
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn release_removes_when_we_own() {
        let env = Env::new();
        env.acquire("telemetry");
        release_at(&env.lock, "telemetry").unwrap();
        assert!(env.owner().is_none());
    }

    #[test]
    fn release_noop_when_other_owns() {
        let env = Env::new();
        env.acquire("keep_awake");
        release_at(&env.lock, "telemetry").unwrap();
        assert_eq!(env.owner().as_deref(), Some("keep_awake"));
    }

    #[test]
    fn webui_wanted_flag_presence_detected() {
        let dir = tempfile::tempdir().unwrap();
        let flag = dir.path().join("keep_awake_webui_wanted");
        assert!(!is_webui_keep_awake_wanted_at(&flag));
        std::fs::write(&flag, b"").unwrap();
        assert!(is_webui_keep_awake_wanted_at(&flag));
        std::fs::remove_file(&flag).unwrap();
        assert!(!is_webui_keep_awake_wanted_at(&flag));
    }
}
