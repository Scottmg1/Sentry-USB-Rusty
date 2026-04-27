//! Per-clip aggregate computation — port of Go `server/drives/aggregate.go`.
//!
//! `compute_route_aggregates` is the **single source of truth** for per-clip
//! scalars: AddRoute calls it on insert, the one-shot v2 backfill calls it
//! for pre-v2 rows, and the refactored summary endpoints read the stored
//! scalars instead of re-deriving them.
//!
//! Semantics match `ComputeAggregateStatsFromRoutes`'s per-route inner loop
//! exactly (null-island filter + GPS-teleport guard, no group-level median):
//!   * Null-island points (|lat| < 1 && |lon| < 1) are excluded from the pair
//!     loop.
//!   * When no SEI speeds are present we use a per-pair GPS derivation d/dt
//!     and drop teleport pairs where d/dt > 70 m/s.
//!   * FSD disengagement uses a 2-second Park grace.
//!   * FSD accel-push detection uses a 3-second engagement grace.
//!
//! `clip_duration_ms` is hard-coded to 60000 (one minute) to match every
//! other consumer in this package — the recorder splits all clips at
//! one-minute boundaries.

use crate::types::{
    Route, RouteAggregates, AUTOPILOT_AUTOSTEER, AUTOPILOT_FSD, AUTOPILOT_OFF, AUTOPILOT_TACC,
    GEAR_PARK,
};

/// Earth radius in meters — used by [`haversine_m`].
const EARTH_RADIUS_M: f64 = 6371000.0;

/// Great-circle distance between two GPS coordinates in meters.
/// Mirrors the `haversineM` Go helper used elsewhere in this crate and
/// the grouper's own inline version.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let to_rad = std::f64::consts::PI / 180.0;
    let dlat = (lat2 - lat1) * to_rad;
    let dlon = (lon2 - lon1) * to_rad;
    let a = (dlat / 2.0).sin().powi(2)
        + (lat1 * to_rad).cos() * (lat2 * to_rad).cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    EARTH_RADIUS_M * c
}

fn is_null_island(p: &[f64; 2]) -> bool {
    p[0].abs() < 1.0 && p[1].abs() < 1.0
}

/// Compute every scalar the Drives-page summary endpoints need for a
/// single clip. Input slices may be mismatched lengths (older imports
/// may omit AutopilotStates/Speeds/AccelPositions); presence is detected
/// by comparing lengths against `Points`.
pub fn compute_route_aggregates(r: &Route) -> RouteAggregates {
    let mut agg = RouteAggregates::default();

    let n = r.points.len();
    if n == 0 {
        return agg;
    }

    let has_ap = r.autopilot_states.len() == n;
    let has_gears = r.gear_states.len() == n;
    let has_accel = r.accel_positions.len() == n;
    let has_sei_speeds = r.speeds.len() == n && r.speeds.iter().any(|&sp| sp > 0.0);

    // Start/End points: first and last non-null-island Points. Tracked
    // independently of the pair loop so single-point clips still report
    // sensible endpoints.
    for p in &r.points {
        if !is_null_island(p) {
            agg.start_lat = Some(p[0]);
            agg.start_lng = Some(p[1]);
            break;
        }
    }
    for p in r.points.iter().rev() {
        if !is_null_island(p) {
            agg.end_lat = Some(p[0]);
            agg.end_lng = Some(p[1]);
            break;
        }
    }

    // ValidPointCount is the count of non-null-island points.
    agg.valid_point_count = r.points.iter().filter(|p| !is_null_island(p)).count() as i64;

    if n < 2 {
        return agg;
    }

    let clip_duration_ms = 60_000.0;
    let dt_ms = clip_duration_ms / (n as f64 - 1.0);
    let dt_sec = dt_ms / 1000.0;

    // Autopilot event tracking — reset per-clip; matches GroupSummaries' inner
    // loop, which also resets these between clips.
    let mut in_accel_press = false;
    let mut fsd_engage_idx: isize = -1;
    let mut pending_disengage = false;
    let mut pending_disengage_idx: usize = 0;

    let mut speed_sum = 0.0f64;

    for i in 1..n {
        let prev = r.points[i - 1];
        let cur = r.points[i];
        if is_null_island(&prev) || is_null_island(&cur) {
            continue;
        }

        let d = haversine_m(prev[0], prev[1], cur[0], cur[1]);

        // GPS-teleport guard when no SEI speeds are available.
        if !has_sei_speeds && dt_sec > 0.0 && d / dt_sec > 70.0 {
            continue;
        }

        agg.distance_m += d;

        // Speed accounting.
        if has_sei_speeds {
            let speed = r.speeds[i] as f64;
            if (0.0..100.0).contains(&speed) {
                speed_sum += speed;
                agg.speed_sample_count += 1;
                if speed > agg.max_speed_mps {
                    agg.max_speed_mps = speed;
                }
            }
        } else if dt_sec > 0.0 {
            let speed = d / dt_sec;
            if speed < 70.0 {
                speed_sum += speed;
                agg.speed_sample_count += 1;
                if speed > agg.max_speed_mps {
                    agg.max_speed_mps = speed;
                }
            }
        }

        // Autopilot accounting.
        if has_ap {
            let cur_ap = r.autopilot_states[i];
            let prev_ap = r.autopilot_states[i - 1];

            if cur_ap != AUTOPILOT_OFF {
                agg.assisted_distance_m += d;
                match cur_ap {
                    AUTOPILOT_FSD => {
                        agg.fsd_engaged_ms += dt_ms as i64;
                        agg.fsd_distance_m += d;
                    }
                    AUTOPILOT_AUTOSTEER => {
                        agg.autosteer_engaged_ms += dt_ms as i64;
                        agg.autosteer_distance_m += d;
                    }
                    AUTOPILOT_TACC => {
                        agg.tacc_engaged_ms += dt_ms as i64;
                        agg.tacc_distance_m += d;
                    }
                    _ => {}
                }
            }

            // Track FSD engagement start (for the 3s accel grace).
            if prev_ap != AUTOPILOT_FSD && cur_ap == AUTOPILOT_FSD {
                fsd_engage_idx = i as isize;
                in_accel_press = false;
            }

            // Resolve any pending FSD disengagement.
            if pending_disengage {
                let time_since_ms = (i - pending_disengage_idx) as f64 * dt_ms;
                if has_gears && r.gear_states[i] == GEAR_PARK && time_since_ms <= 2000.0 {
                    pending_disengage = false;
                } else if time_since_ms > 2000.0 || cur_ap == AUTOPILOT_FSD {
                    agg.fsd_disengagements += 1;
                    pending_disengage = false;
                }
            }

            // Defer FSD disengagement for the Park grace check.
            if prev_ap == AUTOPILOT_FSD && cur_ap != AUTOPILOT_FSD {
                pending_disengage = true;
                pending_disengage_idx = i;
                in_accel_press = false;
            }

            // Accel-push detection (FSD only).
            if cur_ap == AUTOPILOT_FSD && has_accel {
                let mut accel_pct = r.accel_positions[i] as f64;
                if accel_pct <= 1.0 {
                    accel_pct *= 100.0;
                }
                let time_since_engage_ms = if fsd_engage_idx >= 0 {
                    (i as isize - fsd_engage_idx) as f64 * dt_ms
                } else {
                    0.0
                };
                if !in_accel_press && accel_pct > 1.0 && time_since_engage_ms >= 3000.0 {
                    in_accel_press = true;
                } else if in_accel_press && accel_pct <= 0.0 {
                    agg.fsd_accel_pushes += 1;
                    in_accel_press = false;
                }
            } else if cur_ap != AUTOPILOT_FSD {
                in_accel_press = false;
            }
        }
    }

    // Flush pending disengagement at end of clip (match GroupSummaries).
    if pending_disengage {
        let last_is_park = has_gears && r.gear_states[n - 1] == GEAR_PARK;
        if !last_is_park {
            agg.fsd_disengagements += 1;
        }
    }

    if agg.speed_sample_count > 0 {
        agg.avg_speed_mps = speed_sum / agg.speed_sample_count as f64;
    }

    agg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{GearRun, Route};

    fn route_with(points: Vec<[f64; 2]>) -> Route {
        Route {
            file: "test.mp4".to_string(),
            date: "2025-01-01".to_string(),
            points,
            gear_states: vec![],
            autopilot_states: vec![],
            speeds: vec![],
            accel_positions: vec![],
            raw_park_count: 0,
            raw_frame_count: 0,
            gear_runs: vec![],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
        }
    }

    #[test]
    fn empty_route_is_empty_aggregate() {
        let agg = compute_route_aggregates(&route_with(vec![]));
        assert_eq!(agg.distance_m, 0.0);
        assert_eq!(agg.valid_point_count, 0);
        assert!(agg.start_lat.is_none());
        assert!(agg.end_lat.is_none());
    }

    #[test]
    fn null_island_points_excluded_from_start_end() {
        // First valid point is third in the list; last is second-to-last.
        let pts = vec![
            [0.0, 0.0],           // null island
            [37.7749, -122.4194], // valid
            [37.7750, -122.4195], // valid
            [0.0, 0.0],           // null island — should be ignored for end
        ];
        let agg = compute_route_aggregates(&route_with(pts));
        assert_eq!(agg.start_lat, Some(37.7749));
        assert_eq!(agg.end_lat, Some(37.7750));
        assert_eq!(agg.valid_point_count, 2);
    }

    #[test]
    fn distance_is_accumulated_across_valid_pairs() {
        // Two points ~15 meters apart in SF (0.0001 degrees ≈ 11m).
        let pts = vec![[37.7749, -122.4194], [37.7750, -122.4194]];
        let agg = compute_route_aggregates(&route_with(pts));
        assert!(agg.distance_m > 10.0 && agg.distance_m < 15.0);
    }

    #[test]
    fn fsd_disengagement_counted_after_grace() {
        // 61 points — dt_ms = 1000 ms. Engage FSD on frame 1, disengage on
        // frame 20, remain non-FSD for >2s (>20 frames). Should register 1
        // disengagement (no Park grace fires because gears are all "drive").
        let mut points = Vec::with_capacity(61);
        let mut ap = Vec::with_capacity(61);
        let mut gears = Vec::with_capacity(61);
        for i in 0..61 {
            let lat = 37.7749 + (i as f64) * 0.00001;
            points.push([lat, -122.4194]);
            // FSD engaged from frame 1 through 19 inclusive, off thereafter.
            ap.push(if (1..20).contains(&i) { AUTOPILOT_FSD } else { AUTOPILOT_OFF });
            gears.push(4); // arbitrary non-Park gear
        }
        let r = Route {
            file: "test.mp4".to_string(),
            date: "2025-01-01".to_string(),
            points,
            gear_states: gears,
            autopilot_states: ap,
            speeds: vec![],
            accel_positions: vec![],
            raw_park_count: 0,
            raw_frame_count: 61,
            gear_runs: vec![GearRun { gear: 4, frames: 61 }],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
        };
        let agg = compute_route_aggregates(&r);
        assert_eq!(agg.fsd_disengagements, 1);
    }

    #[test]
    fn fsd_disengagement_suppressed_by_park_grace() {
        // Same as above but Park gear applies within the 2s grace window.
        let mut points = Vec::with_capacity(61);
        let mut ap = Vec::with_capacity(61);
        let mut gears = Vec::with_capacity(61);
        for i in 0..61 {
            let lat = 37.7749 + (i as f64) * 0.00001;
            points.push([lat, -122.4194]);
            ap.push(if (1..20).contains(&i) { AUTOPILOT_FSD } else { AUTOPILOT_OFF });
            gears.push(if i >= 20 && i < 25 { GEAR_PARK } else { 4 });
        }
        let r = Route {
            file: "test.mp4".to_string(),
            date: "2025-01-01".to_string(),
            points,
            gear_states: gears,
            autopilot_states: ap,
            speeds: vec![],
            accel_positions: vec![],
            raw_park_count: 1,
            raw_frame_count: 61,
            gear_runs: vec![],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
        };
        let agg = compute_route_aggregates(&r);
        assert_eq!(agg.fsd_disengagements, 0);
    }

    #[test]
    fn haversine_known_distance_sf_to_nyc() {
        // Rough order-of-magnitude: SF→NYC is ~4100 km.
        let d = haversine_m(37.7749, -122.4194, 40.7128, -74.0060);
        assert!(d > 4_000_000.0 && d < 4_200_000.0);
    }
}
