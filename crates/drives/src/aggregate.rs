//! Per-clip aggregate computation.
//!
//! `compute_route_aggregates` is the **single source of truth** for per-clip
//! scalars: AddRoute calls it on insert, the one-shot v2 backfill calls it
//! for pre-v2 rows, and the refactored summary endpoints read the stored
//! scalars instead of re-deriving them.
//!
//! Semantics (null-island filter + GPS-teleport guard, no group-level
//! median — matches the Go implementation this was ported from):
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

use crate::calc;

/// Array-form Null Island check over a `[lat, lon]` point — thin wrapper
/// over [`calc::is_null_island`] for this module's `[f64; 2]` points.
fn is_null_island(p: &[f64; 2]) -> bool {
    calc::is_null_island(p[0], p[1])
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
    // Channel presence vs usefulness: `has_speeds_channel` = the clip
    // carries an SEI speed array at all (feeds `sei_speed_abs_max`, which
    // is NULL without the channel); `has_sei_speeds` additionally requires
    // a positive sample — the locked stats fall back to GPS-derived speed
    // when SEI reads all-zero/negative, and that behavior must not shift.
    let has_speeds_channel = r.speeds.len() == n;
    let has_sei_speeds = has_speeds_channel && r.speeds.iter().any(|&sp| sp > 0.0);

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
    //
    // Engaged-at-start clips anchor the engage timestamp to the clip
    // start: for a continuation clip the true engagement is even older
    // (grace long satisfied), and for a drive's first clip this matches
    // the merged-walk behavior of measuring from the first sample. Pushes
    // that begin inside the 3s window of an engaged-from-start clip are
    // ambiguous in isolation and export via `fsd_accel_pushes_early`.
    let engaged_from_start = has_ap && r.autopilot_states[0] == AUTOPILOT_FSD;
    let mut in_accel_press = false;
    let mut accel_press_early = false;
    let mut fsd_engage_idx: isize = if engaged_from_start { 0 } else { -1 };
    let mut pending_disengage = false;
    let mut pending_disengage_idx: usize = 0;

    // Park intervals in REAL clip time from gear_runs, exactly like the
    // cloud summary v4 producer (cloud_uploader/encrypt.rs). The per-point
    // gear array is aligned to DEDUPLICATED GPS points under a uniform-dt
    // assumption, which distorts worst exactly when the car is stopping to
    // park — points collapse while real seconds keep passing — so the 2s
    // Park grace misses cancellations that Sentry-Drive (real per-point
    // timestamps) catches. gear_runs are frame-domain (real time); when a
    // clip predates gear_runs, fall back to the point-domain gear array.
    let mut park_iv: Vec<(f64, f64)> = Vec::new();
    {
        let gear_total: i64 = r.gear_runs.iter().map(|run| run.frames as i64).sum();
        if gear_total > 0 {
            let per_gear_ms = 60_000.0 / gear_total as f64;
            let mut acc: i64 = 0;
            for run in &r.gear_runs {
                let frames = run.frames as i64;
                if run.gear == GEAR_PARK && frames > 0 {
                    park_iv.push((acc as f64 * per_gear_ms, (acc + frames) as f64 * per_gear_ms));
                }
                acc += frames;
            }
        } else if has_gears {
            // Fallback: point-domain runs of Park under uniform dt.
            let mut run_start: Option<usize> = None;
            for i in 0..n {
                let is_park = r.gear_states[i] == GEAR_PARK;
                match (is_park, run_start) {
                    (true, None) => run_start = Some(i),
                    (false, Some(s)) => {
                        park_iv.push((s as f64 * dt_ms, i as f64 * dt_ms));
                        run_start = None;
                    }
                    _ => {}
                }
            }
            if let Some(s) = run_start {
                park_iv.push((s as f64 * dt_ms, 60_000.0));
            }
        }
    }
    let is_park_at = |ms: f64| park_iv.iter().any(|iv| ms >= iv.0 && ms < iv.1);

    // v15 boundary state for the drive grouper (see RouteAggregates).
    agg.fsd_at_end = has_ap && r.autopilot_states[n - 1] == AUTOPILOT_FSD;
    agg.ap_at_start = if has_ap {
        Some(r.autopilot_states[0] as i32)
    } else {
        None
    };
    if let Some(iv) = park_iv.iter().find(|iv| iv.0 < 2000.0) {
        agg.park_ms_start = Some(iv.0);
    }

    let mut speed_sum = 0.0f64;
    let mut sei_abs_max = 0.0f64;

    for i in 1..n {
        let prev = r.points[i - 1];
        let cur = r.points[i];
        if is_null_island(&prev) || is_null_island(&cur) {
            continue;
        }

        let d = calc::geodesic_m(prev[0], prev[1], cur[0], cur[1]);

        // GPS-teleport guard when no SEI speeds are available.
        if !has_sei_speeds && dt_sec > 0.0 && d / dt_sec > 70.0 {
            continue;
        }

        agg.distance_m += d;

        // v16 summon evidence: max |SEI speed| regardless of sign. SEI
        // speed is SIGNED (negative in Reverse); the locked stats below
        // drop negative samples, but the summon detector must see a
        // reverse-only crawl as movement. Same 100 m/s sanity cap.
        if has_speeds_channel {
            let abs_speed = (r.speeds[i] as f64).abs();
            if abs_speed < 100.0 && abs_speed > sei_abs_max {
                sei_abs_max = abs_speed;
            }
        }

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

            // Resolve any pending FSD disengagement. Park is checked
            // against the real-time park intervals (gear_runs domain) at
            // this sample's nominal time, matching the cloud v4 rule.
            if pending_disengage {
                let t_ms = i as f64 * dt_ms;
                let time_since_ms = (i - pending_disengage_idx) as f64 * dt_ms;
                if is_park_at(t_ms) && time_since_ms <= 2000.0 {
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
                if !in_accel_press && accel_pct > 1.0 {
                    if time_since_engage_ms >= 3000.0 {
                        in_accel_press = true;
                        accel_press_early = false;
                    } else if engaged_from_start && fsd_engage_idx == 0 {
                        // Within the start-anchored grace of an
                        // engaged-from-start clip — real iff the previous
                        // clip ended engaged, which only the grouper knows.
                        in_accel_press = true;
                        accel_press_early = true;
                    }
                } else if in_accel_press && accel_pct <= 0.0 {
                    if accel_press_early {
                        agg.fsd_accel_pushes_early += 1;
                    } else {
                        agg.fsd_accel_pushes += 1;
                    }
                    in_accel_press = false;
                }
            } else if cur_ap != AUTOPILOT_FSD {
                in_accel_press = false;
            }
        }
    }

    // Pending disengagement still open at clip end: the 2s Park grace
    // spans the clip seam, so export the elapsed window for the grouper
    // to resolve against the NEXT clip (mirrors the cloud summary v4
    // `dPnd` rule). Expiry inside the clip was already counted in-loop;
    // a Park inside the window already canceled it there too.
    if pending_disengage {
        let elapsed_ms = (n - 1 - pending_disengage_idx) as f64 * dt_ms;
        if elapsed_ms > 2000.0 {
            agg.fsd_disengagements += 1;
        } else {
            agg.fsd_pend_ms_end = Some(elapsed_ms);
        }
    }

    if agg.speed_sample_count > 0 {
        agg.avg_speed_mps = speed_sum / agg.speed_sample_count as f64;
    }

    // Some(0.0) means "the clip HAS an SEI speed channel but never moved";
    // None means "no channel at all" — the summon detector treats both as
    // not-SEI-verified movement, but the distinction keeps the column
    // honest for future consumers.
    if has_speeds_channel {
        agg.sei_speed_abs_max = Some(sei_abs_max);
    }

    // v18 Safety Score scalars. Always Some once computed — a clip
    // without SEI speeds stores zeros ("no safety data"), while NULL
    // columns mean "row predates v18, not yet backfilled".
    let cs = crate::safety::compute_clip_safety(r);
    agg.safety_hard_brake_ms = Some(cs.hard_brake_ms);
    agg.safety_hard_brake_events = Some(cs.hard_brake_events as i64);
    agg.safety_aggr_turn_ms = Some(cs.aggr_turn_ms);
    agg.safety_aggr_turn_events = Some(cs.aggr_turn_events as i64);
    agg.safety_speeding_ms = Some(cs.speeding_ms);
    agg.safety_moving_ms = Some(cs.moving_ms);
    agg.safety_manual_moving_ms = Some(cs.manual_moving_ms);
    agg.safety_brake_any_ms = Some(cs.brake_any_ms);
    agg.safety_turn_any_ms = Some(cs.turn_any_ms);
    agg.ap_at_end = cs.ap_at_end.map(i32::from);
    agg.safety_imu_moving_ms = Some(cs.imu_moving_ms);
    agg.safety_night_ms = Some(cs.night_ms);
    agg.safety_night_weighted_ms = Some(cs.night_weighted_ms);
    agg.safety_grace_ms_end = Some(cs.grace_ms_end);
    agg.safety_grace_prefix = Some(cs.grace_prefix);

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
            ..Default::default()
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
            ..Default::default()
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
            ..Default::default()
        };
        let agg = compute_route_aggregates(&r);
        assert_eq!(agg.fsd_disengagements, 0);
    }

    /// 61-point route (dt_ms = 1000) with the given per-point AP/gear/accel
    /// arrays. Pads/truncates inputs to 61.
    fn boundary_route(ap: Vec<u8>, gears: Vec<u8>, accel: Vec<f32>) -> Route {
        let n = 61;
        let mut points = Vec::with_capacity(n);
        for i in 0..n {
            points.push([37.7749 + (i as f64) * 0.00001, -122.4194]);
        }
        let fit_u8 = |mut v: Vec<u8>, fill: u8| {
            v.resize(n, fill);
            v
        };
        let mut accel = accel;
        if !accel.is_empty() {
            accel.resize(n, 0.0);
        }
        Route {
            file: "test.mp4".to_string(),
            date: "2025-01-01".to_string(),
            points,
            gear_states: if gears.is_empty() { vec![] } else { fit_u8(gears, 4) },
            autopilot_states: if ap.is_empty() { vec![] } else { fit_u8(ap, AUTOPILOT_OFF) },
            speeds: vec![],
            accel_positions: accel,
            raw_park_count: 0,
            raw_frame_count: 61,
            gear_runs: vec![],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
            ..Default::default()
        }
    }

    #[test]
    fn disengage_near_clip_end_exports_pending_instead_of_counting() {
        // FSD through frame 58, off at 59-60 — the 2s Park grace can't
        // resolve before the clip ends. Pre-v15 this flushed as a counted
        // disengagement (over-counting at every clip seam where the Park
        // arrives in the NEXT clip); now it must export the pending state
        // for the grouper to resolve.
        let ap: Vec<u8> = (0..61)
            .map(|i| if i < 59 { AUTOPILOT_FSD } else { AUTOPILOT_OFF })
            .collect();
        let agg = compute_route_aggregates(&boundary_route(ap, vec![4; 61], vec![]));
        assert_eq!(agg.fsd_disengagements, 0, "pending must not count at clip end");
        let pend = agg.fsd_pend_ms_end.expect("pending state must export");
        assert!((pend - 1000.0).abs() < 1.0, "elapsed should be ~1000ms, got {pend}");
        assert!(!agg.fsd_at_end, "clip ends disengaged");
    }

    #[test]
    fn park_ms_start_and_fsd_at_end_export() {
        // Park on frames 0-1 then Drive; FSD engaged on the final frame.
        let mut gears = vec![4u8; 61];
        gears[0] = GEAR_PARK;
        gears[1] = GEAR_PARK;
        let mut ap = vec![AUTOPILOT_OFF; 61];
        ap[59] = AUTOPILOT_FSD;
        ap[60] = AUTOPILOT_FSD;
        let agg = compute_route_aggregates(&boundary_route(ap, gears, vec![]));
        assert_eq!(agg.park_ms_start, Some(0.0));
        assert!(agg.fsd_at_end);
        // Park later than 2s from clip start must NOT export.
        let mut gears_late = vec![4u8; 61];
        gears_late[30] = GEAR_PARK;
        let agg2 =
            compute_route_aggregates(&boundary_route(vec![AUTOPILOT_OFF; 61], gears_late, vec![]));
        assert_eq!(agg2.park_ms_start, None);
    }

    #[test]
    fn accel_push_counts_when_fsd_engaged_at_clip_start() {
        // FSD engaged from frame 0 (continuation clip). Push at 30s,
        // released at 33s — well past any grace. Pre-v15 the engage
        // timestamp was unset for engaged-at-start clips, the 3s grace
        // never elapsed, and every such push was silently dropped.
        let mut accel = vec![0.0f32; 61];
        for i in 30..33 {
            accel[i] = 30.0;
        }
        let agg =
            compute_route_aggregates(&boundary_route(vec![AUTOPILOT_FSD; 61], vec![4; 61], accel));
        assert_eq!(agg.fsd_accel_pushes, 1);
        assert_eq!(agg.fsd_accel_pushes_early, 0);
    }

    #[test]
    fn early_accel_push_lands_in_early_counter() {
        // Engaged from frame 0, push begins 1s in — within the 3s engage
        // grace measured from clip start. Whether it's a real push depends
        // on the previous clip (engaged continuation or fresh engagement),
        // which only the grouper knows — so it exports separately.
        let mut accel = vec![0.0f32; 61];
        accel[1] = 30.0;
        accel[2] = 30.0;
        let agg =
            compute_route_aggregates(&boundary_route(vec![AUTOPILOT_FSD; 61], vec![4; 61], accel));
        assert_eq!(agg.fsd_accel_pushes, 0);
        assert_eq!(agg.fsd_accel_pushes_early, 1);
    }

    #[test]
    fn ap_at_start_exports_mode_at_first_sample() {
        let agg = compute_route_aggregates(&boundary_route(
            vec![AUTOPILOT_FSD; 61],
            vec![4; 61],
            vec![],
        ));
        assert_eq!(agg.ap_at_start, Some(AUTOPILOT_FSD as i32));
        let agg2 = compute_route_aggregates(&boundary_route(vec![], vec![4; 61], vec![]));
        assert_eq!(agg2.ap_at_start, None, "no AP data → no start mode");
    }

    #[test]
    fn sei_speed_abs_max_sees_reverse_and_channel_absence() {
        // Reverse-only crawl: SEI speeds all negative. The locked
        // max_speed_mps drops them (stays 0), but the summon-evidence
        // column must carry the magnitude.
        let mut r = route_with(
            (0..10)
                .map(|i| [37.7749 + i as f64 * 0.00001, -122.4194])
                .collect(),
        );
        r.speeds = vec![-1.5; 10];
        let agg = compute_route_aggregates(&r);
        // Locked stats drop the negative SEI samples and fall back to
        // GPS-derived speed (~0.17 m/s for these points) — the 1.5 m/s
        // SEI magnitude must never leak into max_speed_mps.
        assert!(
            agg.max_speed_mps < 1.0,
            "locked column must stay GPS-derived, got {}",
            agg.max_speed_mps
        );
        let abs_max = agg.sei_speed_abs_max.expect("channel present");
        assert!((abs_max - 1.5).abs() < 1e-9, "got {abs_max}");

        // No SEI speed channel at all → None, not Some(0.0).
        let r2 = route_with(vec![[37.7749, -122.4194], [37.7750, -122.4194]]);
        let agg2 = compute_route_aggregates(&r2);
        assert!(agg2.sei_speed_abs_max.is_none());

        // Channel present but the car never moved → Some(0.0).
        let mut r3 = route_with(vec![[37.7749, -122.4194], [37.7750, -122.4194]]);
        r3.speeds = vec![0.0; 2];
        let agg3 = compute_route_aggregates(&r3);
        assert_eq!(agg3.sei_speed_abs_max, Some(0.0));
    }

    #[test]
    fn geodesic_known_distance_sf_to_nyc() {
        // SF→NYC is ~4139 km via WGS-84 geodesic.
        let d = calc::geodesic_m(37.7749, -122.4194, 40.7128, -74.0060);
        assert!(d > 4_000_000.0 && d < 4_200_000.0);
    }
}
