//! Corrects large system-clock drift from vehicle timestamps without internet
//! access. Small differences are left to NTP; corrections also update the RTC
//! when available. `clock_settime` does not affect monotonic `Instant` values.

use std::time::Instant;

/// Empirical one-way delay from vehicle timestamping to receipt.
const RESPONSE_LATENCY_COMPENSATION_MS: i64 = 50;

/// Adjusts drift over five minutes and returns whether the clock changed.
pub fn maybe_set_clock_from_vehicle(
    vehicle_ts_ms: i64,
    request_started_at: Instant,
) -> bool {
    // Vehicle timestamps are stamped immediately before transmission.
    let rtt_ms = request_started_at.elapsed().as_millis() as i64;
    let corrected_target_ms = vehicle_ts_ms + RESPONSE_LATENCY_COMPENSATION_MS;
    tracing::debug!("vehicle clock sample: target={corrected_target_ms}ms rtt={rtt_ms}ms");

    sentryusb_clock::maybe_set_clock_ms(corrected_target_ms, "vehicle")
}
