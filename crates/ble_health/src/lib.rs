#[cfg(unix)]
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_HEALTH_PATH: &str = "/mutable/sentryusb-ble-health.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    RepairRequired,
    TransportError,
    ProtocolError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuccessKind {
    Authenticated,
    TransportOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRecord {
    pub fault: FaultKind,
    pub since_ts: i64,
}

pub fn read_at(path: &Path) -> io::Result<Option<HealthRecord>> {
    let body = match std::fs::read(path) {
        Ok(body) => body,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn record_fault_at(path: &Path, fault: FaultKind, now_ts: i64) -> io::Result<()> {
    ensure_parent(path)?;
    with_health_lock(path, || {
        if let Some(existing) = read_at(path)? {
            if existing.fault == FaultKind::RepairRequired && fault != FaultKind::RepairRequired {
                return Ok(());
            }
            if existing.fault == fault {
                return Ok(());
            }
        }

        let tmp = temporary_path(path);
        let body = serde_json::to_vec(&HealthRecord {
            fault,
            since_ts: now_ts,
        })
        .map_err(io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })
}

pub fn record_success_at(path: &Path, success: SuccessKind) -> io::Result<()> {
    match success {
        SuccessKind::Authenticated => clear_all_at(path),
        SuccessKind::TransportOnly => Ok(()),
    }
}

pub fn clear_all_at(path: &Path) -> io::Result<()> {
    ensure_parent(path)?;
    with_health_lock(path, || clear_all_unlocked(path))
}

fn clear_all_unlocked(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn ensure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(unix)]
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".lock");
    path.with_file_name(name)
}

#[cfg(unix)]
fn with_health_lock<T>(path: &Path, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path(path))?;
    lock_exclusive(&lock)?;
    operation()
}

#[cfg(not(unix))]
fn with_health_lock<T>(_path: &Path, operation: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    static PROCESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = PROCESS_LOCK
        .lock()
        .map_err(|_| io::Error::other("BLE health process lock poisoned"))?;
    operation()
}

#[cfg(unix)]
fn lock_exclusive(file: &File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    loop {
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if rc == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Green,
    Yellow,
    Red,
}

#[derive(Debug, Clone, Copy)]
pub struct HealthInput<'a> {
    pub record: Option<HealthRecord>,
    pub last_authenticated_success_ts: i64,
    pub radio_owner: Option<&'a str>,
    pub archiving: bool,
    pub now_ts: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthStatus {
    pub severity: Severity,
    pub code: &'static str,
    pub since_ts: Option<i64>,
    pub label: &'static str,
    pub guidance: &'static str,
}

pub fn classify_health(input: &HealthInput<'_>) -> HealthStatus {
    if let Some(record) = input
        .record
        .filter(|record| record.fault == FaultKind::RepairRequired)
    {
        return HealthStatus {
            severity: Severity::Red,
            code: "repair_required",
            since_ts: Some(record.since_ts),
            label: "Re-pair required",
            guidance:
                "Open Settings, select Re-pair, then tap your key card on the center console.",
        };
    }

    if input.radio_owner == Some("keep_awake") {
        return if input.archiving {
            HealthStatus {
                severity: Severity::Yellow,
                code: "paused_archiving",
                since_ts: None,
                label: "Paused — archiving",
                guidance: "No action needed. Telemetry resumes after archiving.",
            }
        } else {
            HealthStatus {
                severity: Severity::Yellow,
                code: "paused_keep_awake",
                since_ts: None,
                label: "Paused — keep-awake",
                guidance: "No action needed. Telemetry resumes automatically.",
            }
        };
    }

    if let Some(record) = input.record.filter(|record| {
        record.fault != FaultKind::RepairRequired
            && record.since_ts >= input.last_authenticated_success_ts
    }) {
        return HealthStatus {
            severity: Severity::Yellow,
            code: "reconnecting",
            since_ts: Some(record.since_ts),
            label: "Reconnecting",
            guidance:
                "SentryUSB is retrying automatically. Check Bluetooth Logs if this persists.",
        };
    }

    let seconds_ago = (input.last_authenticated_success_ts > 0)
        .then(|| (input.now_ts - input.last_authenticated_success_ts).max(0));

    match seconds_ago {
        Some(age) if age < 60 => HealthStatus {
            severity: Severity::Green,
            code: "connected",
            since_ts: Some(input.last_authenticated_success_ts),
            label: "Connected",
            guidance: "Authenticated vehicle telemetry is flowing.",
        },
        Some(age) if age < 600 => HealthStatus {
            severity: Severity::Yellow,
            code: "delayed",
            since_ts: Some(input.last_authenticated_success_ts),
            label: "Telemetry delayed",
            guidance: "SentryUSB is waiting for the next vehicle response.",
        },
        _ => HealthStatus {
            severity: Severity::Yellow,
            code: "idle",
            since_ts: (input.last_authenticated_success_ts > 0)
                .then_some(input.last_authenticated_success_ts),
            label: "Telemetry idle",
            guidance:
                "The car may be asleep. Wake it, then check Bluetooth Logs if values do not refresh.",
        },
    }
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::time::Duration;

    #[test]
    fn repair_required_cannot_be_overwritten_by_transient_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        record_fault_at(&path, FaultKind::RepairRequired, 100).unwrap();
        record_fault_at(&path, FaultKind::TransportError, 200).unwrap();
        assert_eq!(
            read_at(&path).unwrap().unwrap(),
            HealthRecord {
                fault: FaultKind::RepairRequired,
                since_ts: 100,
            }
        );
    }

    #[test]
    fn transport_only_success_clears_nothing_but_authenticated_success_clears_faults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");

        record_fault_at(&path, FaultKind::TransportError, 100).unwrap();
        record_success_at(&path, SuccessKind::TransportOnly).unwrap();
        assert_eq!(
            read_at(&path).unwrap().unwrap().fault,
            FaultKind::TransportError,
        );
        record_success_at(&path, SuccessKind::Authenticated).unwrap();
        assert!(read_at(&path).unwrap().is_none());

        record_fault_at(&path, FaultKind::RepairRequired, 100).unwrap();
        record_success_at(&path, SuccessKind::TransportOnly).unwrap();
        assert!(read_at(&path).unwrap().is_some());
        record_success_at(&path, SuccessKind::Authenticated).unwrap();
        assert_eq!(read_at(&path).unwrap(), None);
    }

    #[test]
    fn health_updates_are_serialized() {
        let dir = tempfile::tempdir().unwrap();
        let path = Arc::new(dir.path().join("health.json"));
        let barrier = Arc::new(Barrier::new(8));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let mut threads = Vec::new();

        for _ in 0..8 {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let max_active = Arc::clone(&max_active);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                with_health_lock(&path, || {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(5));
                    active.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap();
            }));
        }
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn atomic_write_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.json");
        record_fault_at(&path, FaultKind::ProtocolError, 123).unwrap();
        assert!(path.exists());
        assert!(!dir.path().join("health.json.tmp").exists());
    }

    fn input(
        record: Option<HealthRecord>,
        last_authenticated_success_ts: i64,
        radio_owner: Option<&str>,
        archiving: bool,
        now_ts: i64,
    ) -> HealthInput<'_> {
        HealthInput {
            record,
            last_authenticated_success_ts,
            radio_owner,
            archiving,
            now_ts,
        }
    }

    fn record(fault: FaultKind, since_ts: i64) -> HealthRecord {
        HealthRecord { fault, since_ts }
    }

    #[test]
    fn classifier_uses_required_precedence_and_guidance() {
        let cases = [
            (
                input(
                    Some(record(FaultKind::RepairRequired, 90)),
                    99,
                    None,
                    false,
                    100,
                ),
                Severity::Red,
                "repair_required",
            ),
            (
                input(None, 50, Some("keep_awake"), true, 100),
                Severity::Yellow,
                "paused_archiving",
            ),
            (
                input(
                    Some(record(FaultKind::TransportError, 95)),
                    90,
                    None,
                    false,
                    100,
                ),
                Severity::Yellow,
                "reconnecting",
            ),
            (
                input(None, 70, None, false, 100),
                Severity::Green,
                "connected",
            ),
            (
                input(None, 0, None, false, 100),
                Severity::Yellow,
                "idle",
            ),
        ];

        for (input, severity, code) in cases {
            let got = classify_health(&input);
            assert_eq!(got.severity, severity);
            assert_eq!(got.code, code);
            assert!(!got.guidance.is_empty());
        }
    }

    #[test]
    fn stale_and_radio_busy_are_yellow_not_red() {
        for input in [
            input(None, 500, None, false, 1_200),
            input(None, 1_190, Some("keep_awake"), false, 1_200),
            input(
                Some(record(FaultKind::ProtocolError, 1_195)),
                1_190,
                None,
                false,
                1_200,
            ),
        ] {
            assert_eq!(classify_health(&input).severity, Severity::Yellow);
        }
    }
}
