//! Safety Score — per-clip driving-behavior metrics and the 0–100 score.
//!
//! Modeled on Tesla's Safety Score factors, restricted to what the SEI
//! stream can actually observe: hard braking, aggressive turning,
//! excessive speeding, late-night driving, and FSD/Autopilot share.
//! Forward-collision warnings, unsafe following, and seatbelt state are
//! not present in dashcam SEI and are deliberately absent here.
//!
//! Two halves:
//!   * [`compute_clip_safety`] — the per-clip walk. Called from
//!     `compute_route_aggregates` so the scalars persist as columns and
//!     the BLOB-free summary path can sum them (the same pattern every
//!     other drive metric follows).
//!   * [`compute_safety_score`] — maps window totals (one drive, or a
//!     rolling period of drives) to the 0–100 score with a per-factor
//!     penalty breakdown.
//!
//! Derived G-force caveat: the car's IMU is not in the SEI stream, so
//! longitudinal acceleration is differentiated from SEI speed between
//! GPS-deduplicated samples, and lateral acceleration is `v²·k` with the
//! curvature `k` taken from the GPS track. Both are ~1 Hz estimates of a
//! peak signal — they systematically read LOW versus a real
//! accelerometer, which the thresholds below already account for; do not
//! "correct" them upward without re-tuning against known clips.

use crate::calc;
use crate::types::{FlagRun, Route, AUTOPILOT_OFF, FLAG_BRAKE};

// ---------------------------------------------------------------------------
// Tunables. Every threshold the feature uses lives here.
// ---------------------------------------------------------------------------

/// Hard-braking threshold, m/s² (0.30 g — matches Tesla's factor).
pub const HARD_BRAKE_MPS2: f64 = 0.30 * 9.80665;

/// Any-braking threshold, m/s² (0.10 g). Denominator of the hard-braking
/// ratio, mirroring Tesla's conditional definition: time above 0.3g
/// RELATIVE to time braking above 0.1g — not to all driving time.
pub const ANY_BRAKE_MPS2: f64 = 0.10 * 9.80665;

/// Aggressive-turning lateral threshold, m/s² (0.40 g — matches Tesla).
pub const AGGR_TURN_MPS2: f64 = 0.40 * 9.80665;

/// Any-turning threshold, m/s² (0.20 g). Denominator of the aggressive-
/// turning ratio (Tesla: >0.4g relative to turning time >0.2g).
pub const ANY_TURN_MPS2: f64 = 0.20 * 9.80665;

/// Excessive-speeding threshold, m/s (85 mph, Tesla's cutoff).
pub const SPEEDING_MPS: f64 = 38.0;

/// A sample counts as "moving" at or above this speed (≈1 mph). Moving
/// time is the denominator for every time-proportion rate.
pub const MOVING_MPS: f64 = 0.45;

/// Lateral-acceleration checks are suppressed below this speed (15 mph):
/// GPS bearing noise dominates the curvature estimate at parking-lot
/// speeds and would manufacture phantom aggressive turns.
pub const TURN_MIN_MPS: f64 = 6.7;

/// Minimum GPS hop (m) for a bearing to be trustworthy. Sub-2 m hops are
/// within fix jitter and produce garbage curvature.
pub const TURN_MIN_HOP_M: f64 = 2.0;

/// A braking pair is only evaluated when the EARLIER sample is at least
/// this fast (m/s) — decel derived below it is dominated by quantization.
pub const BRAKE_MIN_MPS: f64 = 2.0;

/// Real inter-sample time is estimated as `distance / avg speed` (the
/// uniform-dt assumption stretches when stationary GPS points collapse);
/// the estimate is clamped to this range of seconds.
pub const PAIR_DT_MIN_S: f64 = 0.25;
pub const PAIR_DT_MAX_S: f64 = 4.0;

/// Brake-pedal confirmation window around a decel sample, clip-ms. The
/// pedal press leads the measured speed drop, hence the asymmetry.
pub const BRAKE_GATE_BEFORE_MS: f64 = 2000.0;
pub const BRAKE_GATE_AFTER_MS: f64 = 500.0;

/// Late-night window (local wall clock), inclusive start / exclusive end.
pub const NIGHT_START_HOUR: u32 = 22;
pub const NIGHT_END_HOUR: u32 = 4;

/// A drive (or window) below either floor gets no score — a single brake
/// tap in a driveway shuffle is noise, not behavior.
pub const MIN_SCORED_MOVING_MS: i64 = 60_000;
pub const MIN_SCORED_MILES: f64 = 0.5;

// Score formula: score = 100 − Σ weight_i · curve(rate_i / cap_i), where
// curve(x) = min(x,1)^CURVE_GAMMA. Caps are the rate at which a factor's
// full weight is charged; the sub-linear gamma makes the first
// occurrences cost the most, mirroring the shape of Tesla's factors.

/// Factor weights (sum = 100). Braking dominates and turning is light,
/// matching the ~3:1 impact ratio in Tesla's published v1 PCF model —
/// and turning is also our noisiest derived metric.
pub const WEIGHT_HARD_BRAKE: f64 = 45.0;
pub const WEIGHT_AGGR_TURN: f64 = 15.0;
pub const WEIGHT_SPEEDING: f64 = 25.0;
pub const WEIGHT_NIGHT: f64 = 15.0;

/// Rate caps at which a factor saturates. Braking/turning are Tesla's
/// own published caps for the SAME conditional ratios (v2.2: hard
/// braking capped at 5.2% of braking time; aggressive turning at 17.1%
/// of turning time). Speeding/night are ours (absolute proportions).
pub const CAP_HARD_BRAKE: f64 = 0.052;
pub const CAP_AGGR_TURN: f64 = 0.171;
pub const CAP_SPEEDING: f64 = 0.05;
pub const CAP_NIGHT: f64 = 0.30;

/// Conditional-ratio denominator floor, ms. A short drive may contain
/// seconds of >0.1g braking; one hard stop would then read as a huge
/// ratio. Flooring the denominator at one minute blends toward the
/// absolute rate exactly when the conditional one is under-sampled.
pub const MIN_RATE_DENOM_MS: i64 = 60_000;

/// Sub-linear penalty exponent.
pub const CURVE_GAMMA: f64 = 0.6;

/// FSD relief: behavior penalties (braking, turning) are scaled by
/// `1 − FSD_RELIEF_MAX · fsd_share`. Events under FSD/Autopilot never
/// count at all; this additionally credits high assisted share on the
/// remaining manual-driving penalties — capped at half so a wild manual
/// driver can't hide behind FSD mileage.
pub const FSD_RELIEF_MAX: f64 = 0.5;

/// Late-night risk weight per local wall-clock hour, mirroring Tesla's
/// v2 change ("impact reduced earlier in the night and increased later").
/// Applied to night MILES when computing the penalty; the displayed
/// night share stays unweighted. Hours outside 10pm–4am weigh 0.
pub fn night_weight(hour: u32) -> f64 {
    match hour {
        22 => 0.5,
        23 => 0.75,
        0 => 1.0,
        1 => 1.25,
        2 | 3 => 1.5,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Per-clip metrics
// ---------------------------------------------------------------------------

/// Safety scalars for one clip. All zeros when the clip has no usable
/// SEI speed channel (imported/GPS-only clips carry no safety data).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClipSafety {
    pub hard_brake_ms: i64,
    pub hard_brake_events: i32,
    pub aggr_turn_ms: i64,
    pub aggr_turn_events: i32,
    pub speeding_ms: i64,
    pub moving_ms: i64,
    pub manual_moving_ms: i64,
    /// Conditional-ratio denominators: manual time decelerating above
    /// 0.1g / turning above 0.2g (same gating as their numerators).
    pub brake_any_ms: i64,
    pub turn_any_ms: i64,
}

/// Frame-domain brake-pedal intervals in clip-ms, built from `flag_runs`
/// exactly like the Park intervals in `compute_route_aggregates`.
/// Empty when the clip predates flag runs — callers treat that as
/// "gate disabled", not "never braked".
fn brake_intervals(runs: &[FlagRun]) -> Vec<(f64, f64)> {
    let total: i64 = runs.iter().map(|r| r.frames as i64).sum();
    if total <= 0 {
        return Vec::new();
    }
    let per_frame_ms = 60_000.0 / total as f64;
    let mut out = Vec::new();
    let mut acc: i64 = 0;
    for run in runs {
        let frames = run.frames as i64;
        if run.flags & FLAG_BRAKE != 0 && frames > 0 {
            out.push((acc as f64 * per_frame_ms, (acc + frames) as f64 * per_frame_ms));
        }
        acc += frames;
    }
    out
}

/// Planar Menger curvature (1/m) of the triangle p0→p1→p2, using a local
/// equirectangular projection. Returns 0 for degenerate triangles.
fn menger_curvature(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2]) -> f64 {
    // Meters per degree at the triangle's latitude.
    let lat0 = p1[0].to_radians();
    let m_per_deg_lat = 111_132.0;
    let m_per_deg_lon = 111_320.0 * lat0.cos();

    let ax = (p1[1] - p0[1]) * m_per_deg_lon;
    let ay = (p1[0] - p0[0]) * m_per_deg_lat;
    let bx = (p2[1] - p0[1]) * m_per_deg_lon;
    let by = (p2[0] - p0[0]) * m_per_deg_lat;

    let a = (ax * ax + ay * ay).sqrt(); // p0→p1
    let cx = bx - ax;
    let cy = by - ay;
    let b = (cx * cx + cy * cy).sqrt(); // p1→p2
    let c = (bx * bx + by * by).sqrt(); // p0→p2
    if a < 1e-6 || b < 1e-6 || c < 1e-6 {
        return 0.0;
    }
    let cross = (ax * by - ay * bx).abs(); // 2·area
    2.0 * cross / (a * b * c)
}

/// Walk one clip's deduped point/speed/AP arrays and accumulate the
/// safety scalars. Mirrors `compute_route_aggregates`' conventions:
/// uniform dt over deduped points for DURATIONS, null-island pairs
/// skipped, channel presence detected by length match.
///
/// G-force source, in preference order:
///   1. MEASURED — the SEI stream's IMU fields (Route.accel_x/accel_y,
///      v19): real lateral/longitudinal acceleration, peak-preserved
///      through GPS dedup. No pedal gate, no consecutive-sample gate,
///      no speed floor beyond "moving" — the sensor is trusted.
///   2. DERIVED — clips extracted before v19 (or firmware without the
///      IMU fields): longitudinal from the speed derivative over a
///      `distance / avg speed` pair-time estimate (uniform dt stretches
///      when deduped stationary points collapse), lateral from GPS
///      curvature ×v², with the noise gates those estimates need.
pub fn compute_clip_safety(r: &Route) -> ClipSafety {
    let mut out = ClipSafety::default();
    let n = r.points.len();
    if n < 2 {
        return out;
    }
    let has_sei_speeds = r.speeds.len() == n && r.speeds.iter().any(|&sp| sp > 0.0);
    if !has_sei_speeds {
        return out;
    }
    let has_ap = r.autopilot_states.len() == n;

    let dt_ms = 60_000.0 / (n as f64 - 1.0);
    let brake_iv = brake_intervals(&r.flag_runs);
    // flag_runs present but with NO brake runs is real information (the
    // pedal was never pressed) — the gate stays active and rejects.
    let has_brake_channel = !r.flag_runs.is_empty();
    let brake_near = |t_ms: f64| -> bool {
        if !has_brake_channel {
            return true; // gate disabled on clips without flag runs
        }
        brake_iv
            .iter()
            .any(|iv| iv.1 > t_ms - BRAKE_GATE_BEFORE_MS && iv.0 < t_ms + BRAKE_GATE_AFTER_MS)
    };

    let manual_at = |i: usize| -> bool { !has_ap || r.autopilot_states[i] == AUTOPILOT_OFF };
    let valid = |i: usize| -> bool { !calc::is_null_island(r.points[i][0], r.points[i][1]) };
    // Measured IMU channel present (v19+ extraction, firmware emits it).
    let has_imu = r.accel_x.len() == n && r.accel_y.len() == n;

    let mut in_brake_run = false;
    let mut in_turn_run = false;
    let mut turn_streak: u32 = 0;

    for i in 1..n {
        if !valid(i) || !valid(i - 1) {
            in_brake_run = false;
            in_turn_run = false;
            turn_streak = 0;
            continue;
        }
        let vp = r.speeds[i - 1] as f64;
        let vc = r.speeds[i] as f64;
        // SEI speed is signed (negative in Reverse) and occasionally
        // glitches; ignore samples outside a sane forward range for
        // everything except the moving check, which uses magnitude.
        let sane = (-1.0..100.0).contains(&vp) && (-1.0..100.0).contains(&vc);

        if vc.abs() >= MOVING_MPS && vc.abs() < 100.0 {
            out.moving_ms += dt_ms as i64;
            if manual_at(i) {
                out.manual_moving_ms += dt_ms as i64;
            }
            if vc >= SPEEDING_MPS && vc < 100.0 {
                out.speeding_ms += dt_ms as i64;
            }
        }

        // ── Measured path: real IMU acceleration per sample ──
        if has_imu {
            if manual_at(i) && vc.abs() >= MOVING_MPS {
                // Sanity: |30 m/s²| ≈ 3g is beyond any road event.
                let decel = -(r.accel_y[i] as f64);
                let lateral = (r.accel_x[i] as f64).abs();
                let braking_here = if decel.abs() < 30.0 && decel >= ANY_BRAKE_MPS2 {
                    out.brake_any_ms += dt_ms as i64;
                    if decel >= HARD_BRAKE_MPS2 {
                        out.hard_brake_ms += dt_ms as i64;
                        if !in_brake_run {
                            out.hard_brake_events += 1;
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                in_brake_run = braking_here;

                let turning_here = if lateral < 30.0 && lateral >= ANY_TURN_MPS2 {
                    out.turn_any_ms += dt_ms as i64;
                    if lateral >= AGGR_TURN_MPS2 {
                        out.aggr_turn_ms += dt_ms as i64;
                        if !in_turn_run {
                            out.aggr_turn_events += 1;
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                in_turn_run = turning_here;
            } else {
                in_brake_run = false;
                in_turn_run = false;
            }
            continue;
        }

        // ── Derived path (pre-v19 rows / no IMU fields) ──
        // Hard braking: decel between consecutive samples over estimated
        // real pair time, manual-only, pedal-confirmed when possible.
        let mut braking_here = false;
        if sane && vp >= BRAKE_MIN_MPS && vc >= 0.0 && manual_at(i) {
            let d = calc::geodesic_m(
                r.points[i - 1][0],
                r.points[i - 1][1],
                r.points[i][0],
                r.points[i][1],
            );
            let v_avg = (vp + vc) / 2.0;
            if v_avg >= 1.0 {
                let pair_dt = (d / v_avg).clamp(PAIR_DT_MIN_S, PAIR_DT_MAX_S);
                let decel = (vp - vc) / pair_dt;
                // Denominator first: any deliberate braking (>0.1g). No
                // pedal gate here — regen alone reaches 0.1g and Tesla's
                // denominator is plain deceleration time.
                if decel >= ANY_BRAKE_MPS2 {
                    out.brake_any_ms += dt_ms as i64;
                }
                if decel >= HARD_BRAKE_MPS2 && brake_near(i as f64 * dt_ms) {
                    braking_here = true;
                    out.hard_brake_ms += dt_ms as i64;
                    if !in_brake_run {
                        out.hard_brake_events += 1;
                    }
                }
            }
        }
        in_brake_run = braking_here;

        // Aggressive turning: v²·k over the triple centered on i, with a
        // 2-consecutive-sample requirement to reject GPS bearing noise.
        let mut turning_here = false;
        if i + 1 < n && valid(i + 1) && sane && manual_at(i) {
            let v = vc.abs();
            if v >= TURN_MIN_MPS {
                let p0 = [r.points[i - 1][0], r.points[i - 1][1]];
                let p1 = [r.points[i][0], r.points[i][1]];
                let p2 = [r.points[i + 1][0], r.points[i + 1][1]];
                let hop_a = calc::geodesic_m(p0[0], p0[1], p1[0], p1[1]);
                let hop_b = calc::geodesic_m(p1[0], p1[1], p2[0], p2[1]);
                if hop_a >= TURN_MIN_HOP_M && hop_b >= TURN_MIN_HOP_M {
                    let lateral = v * v * menger_curvature(p0, p1, p2);
                    if lateral >= ANY_TURN_MPS2 {
                        out.turn_any_ms += dt_ms as i64;
                    }
                    if lateral >= AGGR_TURN_MPS2 {
                        turning_here = true;
                        turn_streak += 1;
                        if turn_streak == 2 {
                            out.aggr_turn_events += 1;
                            out.aggr_turn_ms += (2.0 * dt_ms) as i64;
                        } else if turn_streak > 2 {
                            out.aggr_turn_ms += dt_ms as i64;
                        }
                    }
                }
            }
        }
        if !turning_here {
            turn_streak = 0;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Score
// ---------------------------------------------------------------------------

/// Window totals the score is computed from — one drive, or the sum over
/// a rolling period of drives (summing totals and scoring once IS the
/// mileage/time-weighted aggregate; never average per-drive scores).
#[derive(Debug, Clone, Default)]
pub struct SafetyTotals {
    pub distance_mi: f64,
    pub assisted_mi: f64,
    pub night_mi: f64,
    /// Night miles scaled by [`night_weight`] — the penalty input.
    pub night_weighted_mi: f64,
    pub moving_ms: i64,
    pub manual_moving_ms: i64,
    pub hard_brake_ms: i64,
    pub hard_brake_events: i32,
    pub aggr_turn_ms: i64,
    pub aggr_turn_events: i32,
    pub speeding_ms: i64,
    pub brake_any_ms: i64,
    pub turn_any_ms: i64,
}

/// The score plus its full per-factor decomposition (all f64s rounded to
/// 1 decimal where they are UI-facing).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetyScore {
    /// 0–100, one decimal.
    pub score: f64,
    /// Factor rates as PERCENTAGES of their denominator (UI-facing).
    pub hard_brake_pct: f64,
    pub aggr_turn_pct: f64,
    pub speeding_pct: f64,
    pub night_pct: f64,
    /// Points charged per factor (after FSD relief where it applies).
    pub hard_brake_penalty: f64,
    pub aggr_turn_penalty: f64,
    pub speeding_penalty: f64,
    pub night_penalty: f64,
    /// Assisted share of miles (0–100) and the behavior-penalty relief
    /// fraction it granted (0–50, percent knocked off those penalties).
    pub fsd_share_pct: f64,
    pub fsd_relief_pct: f64,
}

fn curve(rate: f64, cap: f64) -> f64 {
    (rate / cap).clamp(0.0, 1.0).powf(CURVE_GAMMA)
}

/// `None` when the window is too small to score (see MIN_SCORED_*) or
/// carries no safety data at all.
pub fn compute_safety_score(t: &SafetyTotals) -> Option<SafetyScore> {
    if t.moving_ms < MIN_SCORED_MOVING_MS || t.distance_mi < MIN_SCORED_MILES {
        return None;
    }
    // Conditional ratios (Tesla-style): harsh time relative to time spent
    // braking/turning at all, with a floored denominator so a single stop
    // in an under-sampled window can't read as a saturated ratio.
    let hb_rate =
        t.hard_brake_ms as f64 / (t.brake_any_ms.max(MIN_RATE_DENOM_MS)) as f64;
    let at_rate = t.aggr_turn_ms as f64 / (t.turn_any_ms.max(MIN_RATE_DENOM_MS)) as f64;
    let sp_rate = t.speeding_ms as f64 / t.moving_ms as f64;
    let ln_rate = (t.night_mi / t.distance_mi).clamp(0.0, 1.0);
    let ln_weighted_rate = (t.night_weighted_mi / t.distance_mi).clamp(0.0, 1.0);
    let fsd_share = (t.assisted_mi / t.distance_mi).clamp(0.0, 1.0);
    let relief = 1.0 - FSD_RELIEF_MAX * fsd_share;

    let hb_pen = WEIGHT_HARD_BRAKE * curve(hb_rate, CAP_HARD_BRAKE) * relief;
    let at_pen = WEIGHT_AGGR_TURN * curve(at_rate, CAP_AGGR_TURN) * relief;
    let sp_pen = WEIGHT_SPEEDING * curve(sp_rate, CAP_SPEEDING);
    let ln_pen = WEIGHT_NIGHT * curve(ln_weighted_rate, CAP_NIGHT);

    let score = (100.0 - hb_pen - at_pen - sp_pen - ln_pen).clamp(0.0, 100.0);
    Some(SafetyScore {
        score: calc::round1(score),
        hard_brake_pct: calc::round1(hb_rate * 100.0),
        aggr_turn_pct: calc::round1(at_rate * 100.0),
        speeding_pct: calc::round1(sp_rate * 100.0),
        night_pct: calc::round1(ln_rate * 100.0),
        hard_brake_penalty: calc::round1(hb_pen),
        aggr_turn_penalty: calc::round1(at_pen),
        speeding_penalty: calc::round1(sp_pen),
        night_penalty: calc::round1(ln_pen),
        fsd_share_pct: calc::round1(fsd_share * 100.0),
        fsd_relief_pct: calc::round1(FSD_RELIEF_MAX * fsd_share * 100.0),
    })
}

/// True when the local wall-clock hour falls in the late-night window.
pub fn is_night_hour(hour: u32) -> bool {
    hour >= NIGHT_START_HOUR || hour < NIGHT_END_HOUR
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FlagRun, Route, AUTOPILOT_FSD, FLAG_BRAKE};

    /// Straight-line 61-point route (dt = 1000 ms) heading north at the
    /// given per-sample speeds (m/s). Point spacing follows the speeds so
    /// the pair-time estimate reads ~1 s per hop.
    fn route_with_speeds(speeds: Vec<f32>) -> Route {
        let n = speeds.len();
        let mut points = Vec::with_capacity(n);
        let mut lat = 37.7749_f64;
        points.push([lat, -122.4194]);
        for i in 1..n {
            // Advance by the pair's average speed × 1 s, in degrees lat.
            let v_avg = (speeds[i - 1] as f64 + speeds[i] as f64) / 2.0;
            lat += v_avg / 111_132.0;
            points.push([lat, -122.4194]);
        }
        Route {
            file: "test.mp4".to_string(),
            date: "2025-01-01".to_string(),
            points,
            speeds,
            ..Default::default()
        }
    }

    /// 25 m/s cruise with a 0.5 g stop in the middle (25 → 5 over 4 s).
    fn braking_speeds() -> Vec<f32> {
        let mut v = vec![25.0_f32; 61];
        for (k, s) in [20.0, 15.0, 10.0, 5.0].iter().enumerate() {
            v[30 + k] = *s;
        }
        for s in v.iter_mut().skip(34) {
            *s = 5.0;
        }
        v
    }

    #[test]
    fn hard_brake_detected_manual() {
        let r = route_with_speeds(braking_speeds());
        let cs = compute_clip_safety(&r);
        assert_eq!(cs.hard_brake_events, 1, "one contiguous event");
        assert!(cs.hard_brake_ms >= 3000, "≥3 qualifying seconds, got {}", cs.hard_brake_ms);
        assert!(cs.moving_ms > 0 && cs.manual_moving_ms == cs.moving_ms);
        assert!(
            cs.brake_any_ms >= cs.hard_brake_ms,
            "0.1g denominator must include all 0.3g time: {} vs {}",
            cs.brake_any_ms,
            cs.hard_brake_ms
        );
    }

    #[test]
    fn gentle_braking_counts_toward_denominator_only() {
        // 25 → 5 over 20 s ≈ 0.1 g: above ANY_BRAKE, below HARD_BRAKE.
        let mut v = vec![25.0_f32; 61];
        for k in 0..20 {
            v[25 + k] = 25.0 - (k as f32 + 1.0);
        }
        for s in v.iter_mut().skip(45) {
            *s = 5.0;
        }
        let cs = compute_clip_safety(&route_with_speeds(v));
        assert_eq!(cs.hard_brake_events, 0);
        assert!(
            cs.brake_any_ms >= 15_000,
            "~20 s of ~0.1 g braking should land in the denominator, got {}",
            cs.brake_any_ms
        );
    }

    #[test]
    fn hard_brake_under_fsd_excluded() {
        let mut r = route_with_speeds(braking_speeds());
        r.autopilot_states = vec![AUTOPILOT_FSD; 61];
        let cs = compute_clip_safety(&r);
        assert_eq!(cs.hard_brake_events, 0, "FSD braking is not the driver's");
        assert_eq!(cs.manual_moving_ms, 0);
        assert!(cs.moving_ms > 0);
    }

    #[test]
    fn hard_brake_rejected_without_pedal() {
        let mut r = route_with_speeds(braking_speeds());
        // Flag channel present, pedal never pressed → decel gated out
        // (e.g. a speed-signal glitch).
        r.flag_runs = vec![FlagRun { flags: 0, frames: 1800, max_mps: None }];
        let cs = compute_clip_safety(&r);
        assert_eq!(cs.hard_brake_events, 0);

        // Same clip with a brake run covering the stop → detected again.
        r.flag_runs = vec![
            FlagRun { flags: 0, frames: 880, max_mps: None },
            FlagRun { flags: FLAG_BRAKE, frames: 160, max_mps: None },
            FlagRun { flags: 0, frames: 760, max_mps: None },
        ];
        let cs = compute_clip_safety(&r);
        assert_eq!(cs.hard_brake_events, 1);
    }

    #[test]
    fn speeding_time_accumulates() {
        let mut v = vec![30.0_f32; 61];
        for s in v.iter_mut().skip(20).take(10) {
            *s = 39.0; // > 85 mph for ~10 s
        }
        let cs = compute_clip_safety(&route_with_speeds(v));
        assert!(
            (9_000..=11_000).contains(&cs.speeding_ms),
            "~10 s of speeding, got {} ms",
            cs.speeding_ms
        );
    }

    #[test]
    fn aggressive_turn_detected_and_low_speed_ignored() {
        // Circular arc: radius such that v²/r ≈ 0.5 g at 15 m/s → r ≈ 46 m.
        // ω = v/r ≈ 0.326 rad/s; 1 s per sample.
        let n = 61;
        let v_mps = 15.0_f64;
        let radius = 46.0_f64;
        let omega = v_mps / radius;
        let mut points = Vec::with_capacity(n);
        let (clat, clon) = (37.7749_f64, -122.4194_f64);
        let m_lat = 111_132.0;
        let m_lon = 111_320.0 * clat.to_radians().cos();
        for i in 0..n {
            let th = omega * i as f64;
            points.push([clat + radius * th.sin() / m_lat, clon + radius * (1.0 - th.cos()) / m_lon]);
        }
        let mut r = Route {
            file: "test.mp4".to_string(),
            date: "2025-01-01".to_string(),
            points,
            speeds: vec![v_mps as f32; n],
            ..Default::default()
        };
        let cs = compute_clip_safety(&r);
        assert!(cs.aggr_turn_events >= 1, "sustained 0.5 g arc must flag");
        assert!(cs.aggr_turn_ms >= 2000);
        assert!(cs.turn_any_ms >= cs.aggr_turn_ms, "0.2g denominator covers 0.4g time");

        // Same geometry at 5 m/s (below TURN_MIN_MPS): lateral ≈ 0.05 g
        // anyway, but the speed floor must keep it silent even for tight
        // curvature noise.
        r.speeds = vec![5.0; n];
        let cs = compute_clip_safety(&r);
        assert_eq!(cs.aggr_turn_events, 0);
    }

    #[test]
    fn measured_imu_overrides_derived_math() {
        // Constant speed (derived path would find NOTHING), but the IMU
        // channel carries a hard-brake spike and a cornering spike.
        let mut r = route_with_speeds(vec![25.0; 61]);
        let mut ax = vec![0.0_f32; 61];
        let mut ay = vec![0.0_f32; 61];
        for k in 30..33 {
            ay[k] = -4.0; // 0.41 g deceleration
        }
        ay[40] = -1.5; // 0.15 g — denominator only
        for k in 45..47 {
            ax[k] = 5.0; // 0.51 g lateral
        }
        ax[50] = 2.5; // 0.25 g — denominator only
        r.accel_x = ax;
        r.accel_y = ay;

        let cs = compute_clip_safety(&r);
        assert_eq!(cs.hard_brake_events, 1, "IMU brake spike must flag");
        assert!((2000..=4000).contains(&cs.hard_brake_ms), "got {}", cs.hard_brake_ms);
        assert_eq!(cs.aggr_turn_events, 1, "single measured sample suffices — no streak gate");
        assert!(cs.brake_any_ms > cs.hard_brake_ms);
        assert!(cs.turn_any_ms > cs.aggr_turn_ms);

        // Same clip under FSD: nothing counts.
        r.autopilot_states = vec![AUTOPILOT_FSD; 61];
        let cs = compute_clip_safety(&r);
        assert_eq!(cs.hard_brake_events, 0);
        assert_eq!(cs.aggr_turn_events, 0);
        assert_eq!(cs.brake_any_ms, 0);
    }

    #[test]
    fn straight_cruise_is_clean() {
        let cs = compute_clip_safety(&route_with_speeds(vec![25.0; 61]));
        assert_eq!(cs.hard_brake_events, 0);
        assert_eq!(cs.aggr_turn_events, 0);
        assert_eq!(cs.speeding_ms, 0);
        assert!((59_000..=61_000).contains(&cs.moving_ms), "got {}", cs.moving_ms);
    }

    #[test]
    fn no_sei_speeds_means_no_safety_data() {
        let mut r = route_with_speeds(vec![25.0; 61]);
        r.speeds = vec![];
        assert_eq!(compute_clip_safety(&r), ClipSafety::default());
    }

    // ── score formula ──

    fn base_totals() -> SafetyTotals {
        SafetyTotals {
            distance_mi: 100.0,
            moving_ms: 4 * 3_600_000,
            manual_moving_ms: 4 * 3_600_000,
            ..Default::default()
        }
    }

    #[test]
    fn clean_window_scores_100() {
        let s = compute_safety_score(&base_totals()).unwrap();
        assert_eq!(s.score, 100.0);
        assert_eq!(s.hard_brake_penalty, 0.0);
    }

    #[test]
    fn saturated_everything_scores_0() {
        let mut t = base_totals();
        t.brake_any_ms = t.manual_moving_ms;
        t.hard_brake_ms = t.brake_any_ms; // ratio 1.0 ≫ cap
        t.turn_any_ms = t.manual_moving_ms;
        t.aggr_turn_ms = t.turn_any_ms;
        t.speeding_ms = t.moving_ms;
        t.night_mi = t.distance_mi;
        t.night_weighted_mi = t.distance_mi;
        let s = compute_safety_score(&t).unwrap();
        assert_eq!(s.score, 0.0);
    }

    #[test]
    fn fsd_share_relieves_behavior_penalties_only() {
        let mut t = base_totals();
        t.brake_any_ms = 3_600_000; // 1 h of braking time
        t.hard_brake_ms = 72_000; // 2% of it → mid-curve
        t.speeding_ms = 72_000;
        let manual = compute_safety_score(&t).unwrap();

        t.assisted_mi = 80.0; // 80% FSD share → 40% relief
        let assisted = compute_safety_score(&t).unwrap();
        assert!(assisted.score > manual.score);
        assert!(assisted.hard_brake_penalty < manual.hard_brake_penalty);
        assert_eq!(assisted.speeding_penalty, manual.speeding_penalty);
        assert_eq!(assisted.fsd_relief_pct, 40.0);
    }

    #[test]
    fn denominator_floor_tempers_undersampled_ratios() {
        // One second of hard braking against only 2 s of total braking
        // would be a 50% ratio; the 60 s floor keeps it at ~1.7% instead
        // of instantly saturating the 5.2% cap.
        let mut t = base_totals();
        t.brake_any_ms = 2_000;
        t.hard_brake_ms = 1_000;
        let s = compute_safety_score(&t).unwrap();
        assert!(
            s.hard_brake_pct < 2.0,
            "floored ratio should be ~1.7%, got {}",
            s.hard_brake_pct
        );
        assert!(s.hard_brake_penalty < WEIGHT_HARD_BRAKE, "must not saturate");
    }

    #[test]
    fn night_weighting_scales_penalty_not_display() {
        let mut early = base_totals();
        early.night_mi = 30.0;
        early.night_weighted_mi = 30.0 * night_weight(22); // 10pm — half weight
        let mut late = base_totals();
        late.night_mi = 30.0;
        late.night_weighted_mi = 30.0 * night_weight(3); // 3am — 1.5×
        let se = compute_safety_score(&early).unwrap();
        let sl = compute_safety_score(&late).unwrap();
        assert_eq!(se.night_pct, sl.night_pct, "displayed share is unweighted");
        assert!(sl.night_penalty > se.night_penalty, "3am must cost more than 10pm");
    }

    #[test]
    fn tiny_windows_are_unscored() {
        let mut t = base_totals();
        t.distance_mi = 0.3;
        assert!(compute_safety_score(&t).is_none());
        let mut t = base_totals();
        t.moving_ms = 30_000;
        assert!(compute_safety_score(&t).is_none());
    }

    #[test]
    fn night_window_hours() {
        assert!(is_night_hour(22));
        assert!(is_night_hour(23));
        assert!(is_night_hour(0));
        assert!(is_night_hour(3));
        assert!(!is_night_hour(4));
        assert!(!is_night_hour(12));
        assert!(!is_night_hour(21));
        // Risk curve: monotonically rising through the night, zero outside.
        assert!(night_weight(22) < night_weight(23));
        assert!(night_weight(23) < night_weight(0));
        assert!(night_weight(0) < night_weight(1));
        assert!(night_weight(1) < night_weight(2));
        assert_eq!(night_weight(2), night_weight(3));
        assert_eq!(night_weight(4), 0.0);
        assert_eq!(night_weight(12), 0.0);
    }
}
