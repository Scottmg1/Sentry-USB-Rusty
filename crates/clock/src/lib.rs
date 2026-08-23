//! Shared system-clock correction.
//!
//! Callers supply a wall-clock time they trust (a vehicle timestamp over
//! BLE, an HTTP `Date` header) and this crate decides whether the local
//! clock is far enough out to be worth moving. Small differences are left
//! to NTP or the RTC. `clock_settime` does not affect monotonic `Instant`
//! values, so in-flight timers are unaffected.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

/// Differences below five minutes are left to NTP or the RTC.
pub const ADJUSTMENT_THRESHOLD_MS: i64 = 300_000;

/// 2025-01-01 00:00:00 UTC. A clock reading below this on a board that
/// never had a valid time is the filesystem-timestamp fallback, not a
/// real wall clock.
pub const PLAUSIBLE_EPOCH_FLOOR_MS: i64 = 1_735_689_600_000;

/// 2100-01-01 00:00:00 UTC. Guards against a garbage timestamp from an
/// untrusted source pushing the clock somewhere unrecoverable.
pub const PLAUSIBLE_EPOCH_CEILING_MS: i64 = 4_102_444_800_000;

/// Set by systemd-timesyncd once NTP has stepped the clock.
const NTP_SYNC_MARKER: &str = "/run/systemd/timesync/synchronized";

/// Current `CLOCK_REALTIME` in milliseconds since the epoch.
pub fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// True when the board has a real-time clock that survives power loss.
///
/// Pi 5 exposes `/dev/rtc0` only with a battery fitted; earlier boards
/// have no RTC at all and rely on fake-hwclock, which only ever restores
/// the time of the last clean shutdown.
pub fn has_rtc() -> bool {
    rtc_device().is_some()
}

/// First RTC device node (`/dev/rtc0`, `/dev/rtc1`, `/dev/rtc`, …).
fn rtc_device() -> Option<std::path::PathBuf> {
    for name in ["rtc0", "rtc1", "rtc"] {
        let p = Path::new("/dev").join(name);
        if p.exists() {
            return Some(p);
        }
    }
    // A HAT can land on rtcN instead of rtc0.
    let entries = std::fs::read_dir("/dev").ok()?;
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(n) = name.to_str() else { continue };
        if n.starts_with("rtc") {
            return Some(e.path());
        }
    }
    None
}

/// True once systemd-timesyncd has synchronised against NTP.
pub fn ntp_synced() -> bool {
    Path::new(NTP_SYNC_MARKER).exists()
}

/// True when the clock is credible: NTP has stepped it, or it reads at
/// least 2025. Mirrors the check behind `GET /api/system/clock-status`.
pub fn clock_is_credible() -> bool {
    ntp_synced() || now_unix_ms() >= PLAUSIBLE_EPOCH_FLOOR_MS
}

/// Whether `unix_ms` is a wall-clock time worth trusting at all.
pub fn is_plausible_ms(unix_ms: i64) -> bool {
    (PLAUSIBLE_EPOCH_FLOOR_MS..PLAUSIBLE_EPOCH_CEILING_MS).contains(&unix_ms)
}

/// Whether a network time source is worth consulting at all.
///
/// NTP owns the clock once it has synchronised, and a battery-backed RTC
/// that seeded a credible time at boot needs no help either. An RTC node
/// that exists but left the clock at the epoch is not healthy, so it does
/// not count. Public so a caller can skip the network round-trips, not
/// just the resulting step.
pub fn needs_network_time() -> bool {
    decide_needs_network_time(ntp_synced(), has_rtc(), clock_is_credible())
}

/// Split out from [`needs_network_time`] so the policy can be tested
/// without a real `/dev` or `/run`.
fn decide_needs_network_time(ntp_synced: bool, has_rtc: bool, clock_is_credible: bool) -> bool {
    !ntp_synced && !(has_rtc && clock_is_credible)
}

/// Network / update-check path: skips a board whose clock is already
/// looked after, per [`needs_network_time`]. Vehicle BLE must still be
/// able to correct a drifted RTC, so that path calls
/// [`maybe_set_clock_ms`] directly and only the five-minute threshold
/// applies.
pub fn maybe_set_clock_from_network(unix_ms: i64, source: &str) -> bool {
    if !needs_network_time() {
        return false;
    }
    maybe_set_clock_ms(unix_ms, source)
}

/// Steps `CLOCK_REALTIME` to `unix_ms` when the local clock is more than
/// [`ADJUSTMENT_THRESHOLD_MS`] away, and returns whether it moved.
///
/// `source` names the time source for the log line. Failure is reported
/// as `false`, never as a panic or an error the caller has to handle:
/// every call site is best-effort.
pub fn maybe_set_clock_ms(unix_ms: i64, source: &str) -> bool {
    if !is_plausible_ms(unix_ms) {
        warn!("refusing to set clock from {source}: {unix_ms}ms is outside the plausible window");
        return false;
    }

    let local_ms = now_unix_ms();
    let delta_ms = unix_ms - local_ms;
    if delta_ms.abs() < ADJUSTMENT_THRESHOLD_MS {
        // Avoid fighting healthy NTP or RTC correction.
        return false;
    }

    info!(
        "system clock differs from {} by {}ms (local={}ms, {}={}ms); adjusting",
        source, delta_ms, local_ms, source, unix_ms
    );

    set_clock_ms(unix_ms)
}

/// Steps `CLOCK_REALTIME` unconditionally and persists to the RTC when
/// one is present. Returns whether the step succeeded.
pub fn set_clock_ms(unix_ms: i64) -> bool {
    // Requires CAP_SYS_TIME.
    let secs = unix_ms.div_euclid(1000);
    let ms_remainder = unix_ms.rem_euclid(1000);
    // `tv_nsec` width differs by architecture.
    let ts = libc::timespec {
        tv_sec: secs as libc::time_t,
        tv_nsec: (ms_remainder * 1_000_000) as libc::c_long,
    };
    let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        warn!(
            "clock_settime failed: {} (errno={})",
            err,
            err.raw_os_error().unwrap_or(0)
        );
        return false;
    }

    persist_to_rtc();
    true
}

/// Writes system time back to the RTC when one exists. Failure does not
/// undo the system-time step.
fn persist_to_rtc() {
    let Some(dev) = rtc_device() else {
        return;
    };
    match std::process::Command::new("hwclock")
        .args(["-w", "-f"])
        .arg(&dev)
        .output()
    {
        Ok(out) if out.status.success() => {
            info!("wrote corrected time to RTC (hwclock -w -f {})", dev.display());
        }
        Ok(out) => {
            warn!(
                "hwclock -w -f {} returned {}: {}",
                dev.display(),
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Err(e) => warn!("hwclock -w failed to run: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausibility_window_rejects_unset_and_absurd_clocks() {
        // fake-hwclock restoring a 2024 image timestamp.
        assert!(!is_plausible_ms(1_704_067_200_000));
        assert!(!is_plausible_ms(0));
        assert!(!is_plausible_ms(-1));
        // 2026-08-23.
        assert!(is_plausible_ms(1_787_443_200_000));
        assert!(!is_plausible_ms(PLAUSIBLE_EPOCH_CEILING_MS));
    }

    #[test]
    fn healthy_ntp_or_rtc_needs_no_network_time() {
        // NTP has stepped the clock; nothing else may interfere.
        assert!(!decide_needs_network_time(true, false, true));
        assert!(!decide_needs_network_time(true, true, true));
        // Battery-backed RTC seeded a credible time at boot.
        assert!(!decide_needs_network_time(false, true, true));
    }

    #[test]
    fn rtcless_and_stuck_boards_want_network_time() {
        // Pi 4: no RTC, fake-hwclock restored a stale but plausible time.
        assert!(decide_needs_network_time(false, false, true));
        // Same board, clock still sitting at the epoch.
        assert!(decide_needs_network_time(false, false, false));
        // RTC node present but it left the clock at the epoch: not healthy.
        assert!(decide_needs_network_time(false, true, false));
    }

    #[test]
    fn implausible_sources_are_refused_before_any_syscall() {
        assert!(!maybe_set_clock_ms(0, "test"));
        assert!(!maybe_set_clock_ms(PLAUSIBLE_EPOCH_FLOOR_MS - 1, "test"));
    }

    #[test]
    fn small_drift_is_left_to_ntp() {
        // Under the threshold the clock is never touched, so this is safe
        // to assert without CAP_SYS_TIME.
        let near = now_unix_ms() + ADJUSTMENT_THRESHOLD_MS - 1_000;
        assert!(!maybe_set_clock_ms(near, "test"));
    }
}
