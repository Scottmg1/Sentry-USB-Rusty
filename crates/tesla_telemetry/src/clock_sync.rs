//! Corrects large system-clock drift from vehicle timestamps without internet
//! access. Small differences are left to NTP; corrections also update the RTC
//! when available. `clock_settime` does not affect monotonic `Instant` values.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tracing::{info, warn};

/// Differences below five minutes are left to NTP or the RTC.
const ADJUSTMENT_THRESHOLD_MS: i64 = 300_000;

/// Empirical one-way delay from vehicle timestamping to receipt.
const RESPONSE_LATENCY_COMPENSATION_MS: i64 = 50;

/// Adjusts drift over five minutes and returns whether the clock changed.
pub fn maybe_set_clock_from_vehicle(
    vehicle_ts_ms: i64,
    request_started_at: Instant,
) -> bool {
    let local_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    // Vehicle timestamps are stamped immediately before transmission.
    let rtt_ms = request_started_at.elapsed().as_millis() as i64;
    let corrected_target_ms = vehicle_ts_ms + RESPONSE_LATENCY_COMPENSATION_MS;

    let delta_ms = corrected_target_ms - local_ms;
    if delta_ms.abs() < ADJUSTMENT_THRESHOLD_MS {
        // Avoid fighting healthy NTP or RTC correction.
        return false;
    }

    info!(
        "system clock differs from vehicle by {}ms (local={}ms, vehicle={}ms, rtt={}ms); \
         adjusting to vehicle time",
        delta_ms, local_ms, corrected_target_ms, rtt_ms
    );

    // Requires CAP_SYS_TIME.
    let secs = corrected_target_ms / 1000;
    let ms_remainder = corrected_target_ms % 1000;
    // `tv_nsec` width differs by architecture.
    let ts = libc::timespec {
        tv_sec: secs as libc::time_t,
        tv_nsec: (ms_remainder * 1_000_000) as libc::c_long,
    };
    let rc = unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &ts) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        warn!("clock_settime failed: {} (errno={})", err, err.raw_os_error().unwrap_or(0));
        return false;
    }

    // Persist to an RTC when available; failure does not undo system time.
    if std::path::Path::new("/dev/rtc0").exists() {
        match std::process::Command::new("hwclock").args(["-w"]).output() {
            Ok(out) if out.status.success() => {
                info!("wrote corrected time to RTC (hwclock -w)");
            }
            Ok(out) => {
                warn!(
                    "hwclock -w returned {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Err(e) => warn!("hwclock -w failed to run: {e}"),
        }
    }

    true
}
