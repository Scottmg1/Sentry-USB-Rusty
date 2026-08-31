//! Drive grouping, gear splitting, stats computation, FSD analytics.
//!
//! Groups Tesla dashcam clips into logical drives by timestamp gaps and
//! gear-state transitions, then computes distance, speed, and
//! FSD/autopilot analytics per drive.

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};
use tracing::{info, warn};

use crate::calc;
use crate::extract::{
    AUTOPILOT_AUTOSTEER, AUTOPILOT_FSD, AUTOPILOT_OFF, AUTOPILOT_TACC, GEAR_PARK,
};
use crate::types::*;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Time gap (ms) that splits clips into separate drives (5 minutes).
pub(crate) const DRIVE_GAP_MS: i64 = 5 * 60 * 1000;

/// Minimum gap (ms) between consecutive RecentClips timestamps that counts
/// as a recording hole (≥1 missing minute-clip; normal spacing is ~60s).
pub(crate) const GAP_FILL_MIN_MS: i64 = 90 * 1000;

/// Max hole (ms) event clips may fill, and the max distance a chained
/// edge fill may extend past the nearest RecentClips clip. Sentry-covered
/// dropouts run long — the car parked, so the gap spans the arrival + a
/// chain of ~10-min pre-roll events (real data: 9-19 min). Wider than
/// DRIVE_GAP_MS on purpose; the coverage requirement (an event clip must
/// exist for each filled minute) is what actually bounds the fill. Beyond
/// this it's a genuine multi-drive boundary, not a recording dropout.
pub(crate) const GAP_FILL_MAX_MS: i64 = 30 * 60 * 1000;

/// Chain hop window (ms): an event clip is proximity-eligible when it sits
/// within this of a RecentClips clip or of another eligible event clip.
/// Event pre-roll segments run ~60-77 s apart, but real chains show
/// intra-event gaps up to ~152 s (July 4 data), so one clip length plus
/// generous slack. Anything further is not a continuation of surrounding
/// footage.
pub(crate) const GAP_FILL_ADJ_MS: i64 = 3 * 60 * 1000;

/// Duplicate window (ms): an event clip whose timestamp falls within this
/// AFTER a RecentClips clip (or after an already-kept event candidate) is
/// the same recorder segment wearing a different path — Tesla stamps the
/// twin 0-1 s apart in practice. One-sided on purpose: a clip shortly
/// BEFORE the next RecentClips clip is the event pre-roll's final segment,
/// truncated at the trigger, and carries unique hole-tail footage.
pub(crate) const GAP_FILL_DUP_MS: i64 = 30 * 1000;

/// Speed (m/s) above which a clip counts as containing driving even when
/// its gear telemetry is missing or reads Park throughout (~1.1 mph;
/// reverse reports negative speeds, callers compare on abs()).
pub(crate) const GAP_FILL_MIN_SPEED_MPS: f32 = 0.5;

/// Minimum Park duration (seconds) that ends the current drive within a clip.
const PARK_GAP_SECONDS: f64 = 2.0;

/// Nominal clip container length (ms) — the recorder splits all clips at
/// one-minute boundaries. Frame→time math against a clip's raw frame
/// count uses this span (the last clip of a session can carry far fewer
/// real frames than the container claims; those frames are real data).
const CLIP_DURATION_MS: i64 = 60_000;

// Summon detection thresholds — mirror Sentry-Drive's drive-calc.cjs
// exactly (locked there by drive-calc.test.js), verified against real
// Actually Smart Summon footage (2026-07-15 20:49/20:50): hazards = both
// blinker bits in the SAME run, held 1-4 s spanning BOTH gear
// transitions; accel/brake stay 0 the whole drive; autopilot_state stays
// 0 during summon, so it plays no part here. Speed cap: Tesla limits
// summon to ~6 mph on older firmware and 8 mph (3.58 m/s) on newer cars
// — 4.5 m/s (10.1 mph) gives the 8 mph ceiling the same headroom the
// previous 3.5 gate gave the 6 mph one (observed ASS peak on the 6 mph
// firmware: 2.7 m/s).

/// SEI max speed (m/s) above which a drive cannot be a summon.
const SUMMON_MAX_SPEED_MPS: f64 = 4.5;

/// Hazards must appear within this many seconds of the drive's start and
/// end (in frames-of-clip terms — see `detect_summon`).
const SUMMON_BOOKEND_SECONDS: f64 = 10.0;

/// Max summon drive duration (10 minutes).
const SUMMON_MAX_DURATION_MS: i64 = 10 * 60 * 1000;

/// How much of a drive's wall-clock may go unrepresented by clip
/// evidence. Half a clip: a single missing minute-clip is caught,
/// ordinary park-split timing skew is not.
const SUMMON_MAX_UNCOVERED_MS: i64 = CLIP_DURATION_MS / 2;

/// Minimum FSD share of the frames the drive's segments cover, for the
/// Self Driving signature. Newer hardware reports a Summon maneuver as
/// autopilot_state = FSD; the Off frames bracketing it (the car sitting
/// in Park at either end) are why this is a share and not "all".
const SUMMON_MIN_FSD_SHARE: f64 = 0.5;

// ---------------------------------------------------------------------------
// Public API
//
// The Route-taking grouping/stats/analytics entry points that re-walked
// every point BLOB per request (group_summaries, build_single_drive,
// compute_aggregate_stats, fsd_analytics) were removed once every
// endpoint moved to the summary-based path below — route_overviews is
// the only remaining full-Route consumer.
// ---------------------------------------------------------------------------

/// Overview routes for map display (downsampled, outlier-filtered).
/// Takes the routes by value: the full-Route set is the heaviest
/// allocation in the app (hundreds of MB on a long-used Pi), and the
/// grouper used to clone every route on top of it — peak memory was
/// 2× the store size on a 1 GB board.
pub fn route_overviews(routes: Vec<Route>, max_points_per_drive: usize) -> Vec<RouteOverview> {
    group_routes_overview(routes, max_points_per_drive)
}

// ---------------------------------------------------------------------------
// Public API — summary-based, no point BLOBs
//
// These drive the Drives list / stats / FSD-analytics endpoints using
// `RouteSummary` rows (metadata + pre-computed per-clip aggregate columns).
// The `Route`-taking versions above re-walk every point in the store on
// every request (~300 MB on a 5500-clip DB); these trade bit-for-bit
// numerical parity for a 50–100× drop in heap by trusting the aggregates
// that `compute_route_aggregates` populated on insert.
//
// The aggregates were computed with the same null-island + GPS-teleport
// filters the live path uses, so for any drive whose clips have clean GPS
// the numbers match. Dirty GPS is where the paths can drift by fractions
// of a percent on distance-derived fields — invisible after the UI's
// 0.1-mi / whole-percent rounding.
// ---------------------------------------------------------------------------

/// BLOB-free analogue of [`group_summaries`]. Builds the same
/// `DriveSummary` list for the Drives page by summing each clip's
/// pre-computed aggregate columns instead of re-walking their point
/// arrays.
pub fn group_summaries_fast(
    summaries: &[RouteSummary],
    tags: &HashMap<String, Vec<String>>,
) -> Vec<DriveSummary> {
    let groups = group_summary_clips(summaries);
    groups
        .iter()
        .enumerate()
        .map(|(idx, clips)| build_summary_from_aggregates(clips, idx, tags))
        .collect()
}

/// BLOB-free FSD analytics with explicit period
/// ("day" / "week" / "all"). Used by `GET /api/drives/fsd-analytics`
/// when the query string asks for something other than the cached
/// week view, so the Day / Week / All Time toggle on the FSD page
/// returns actually-different data.
pub fn fsd_analytics_from_summaries_for_period(
    summaries: &[RouteSummary],
    period: &str,
) -> FsdAnalytics {
    let empty_tags = HashMap::new();
    let drives = group_summaries_fast(summaries, &empty_tags);
    build_fsd_analytics(&drives, period)
}

/// Build FSD analytics from an already-grouped drive list. Used by the
/// cache rebuild path so `group_summaries_fast` is not called a second time.
pub fn fsd_analytics_from_drives(drives: &[DriveSummary]) -> FsdAnalytics {
    build_fsd_analytics(drives, "week")
}

/// BLOB-free Safety Score analytics with explicit period
/// ("day" / "week" / "month" / "all"). Used by
/// `GET /api/drives/safety-analytics`.
pub fn safety_analytics_from_summaries_for_period(
    summaries: &[RouteSummary],
    period: &str,
) -> SafetyAnalytics {
    let empty_tags = HashMap::new();
    let drives = group_summaries_fast(summaries, &empty_tags);
    build_safety_analytics(&drives, period)
}

/// Safety estimate analytics over an already-grouped drive list. Each
/// compatible local day is scored independently, then daily scores and
/// factor displays are weighted by native mileage.
fn build_safety_analytics(summaries: &[DriveSummary], period: &str) -> SafetyAnalytics {
    let now = chrono::Local::now().naive_local();
    let today = now.date();

    let period_start: Option<NaiveDate> = match period {
        "day" => Some(today),
        "week" => Some(today - chrono::Duration::days(7)),
        "month" => Some(today - chrono::Duration::days(30)),
        _ => None, // "all" — no filter
    };
    let period_start_str = period_start
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();

    // Same exclusions as FSD analytics: imported drives carry no SEI
    // safety data, summon sessions have no human driver to score.
    let period_drives: Vec<&DriveSummary> = summaries
        .iter()
        .filter(|d| {
            if is_imported(&d.source) || d.summon {
                return false;
            }
            if let Some(ps) = period_start {
                if let Ok(dt) =
                    NaiveDateTime::parse_from_str(&d.start_time, "%Y-%m-%dT%H:%M:%S")
                {
                    return dt.date() >= ps;
                }
                return false;
            }
            true
        })
        .collect();

    let mut totals = crate::safety::SafetyTotals::default();
    let mut total_dist_km: f64 = 0.0;
    let mut assisted_dist_km: f64 = 0.0;
    let mut night_km: f64 = 0.0;
    let mut scored_drives: i32 = 0;
    let mut fsd_disengagements: i32 = 0;

    // Daily accumulation: per-day SafetyTotals plus display counters.
    struct DayAcc {
        day_name: String,
        totals: crate::safety::SafetyTotals,
        drives: i32,
        distance_km: f64,
    }
    let mut daily_map: HashMap<String, DayAcc> = HashMap::new();

    for d in &period_drives {
        let assisted_mi =
            d.fsd_distance_mi + d.autosteer_distance_mi + d.tacc_distance_mi;
        totals.distance_mi += d.distance_mi;
        totals.assisted_mi += assisted_mi;
        totals.night_mi += d.safety_night_mi;
        totals.night_weighted_mi += d.safety_night_weighted_mi;
        totals.moving_ms += d.safety_moving_ms;
        totals.imu_moving_ms += d.safety_imu_moving_ms;
        totals.night_ms += d.safety_night_ms;
        totals.night_weighted_ms += d.safety_night_weighted_ms;
        totals.manual_moving_ms += d.safety_manual_moving_ms;
        totals.hard_brake_ms += d.safety_hard_brake_ms;
        totals.hard_brake_events += d.safety_hard_brake_events;
        totals.aggr_turn_ms += d.safety_aggr_turn_ms;
        totals.aggr_turn_events += d.safety_aggr_turn_events;
        totals.speeding_ms += d.safety_speeding_ms;
        totals.brake_any_ms += d.safety_brake_any_ms;
        totals.turn_any_ms += d.safety_turn_any_ms;
        total_dist_km += d.distance_km;
        assisted_dist_km +=
            d.fsd_distance_km + d.autosteer_distance_km + d.tacc_distance_km;
        night_km += d.safety_night_mi * calc::M_PER_MILE / 1000.0;
        if d.safety_score.is_some() {
            scored_drives += 1;
        }
        fsd_disengagements += d.fsd_disengagements;

        if let Ok(dt) = NaiveDateTime::parse_from_str(&d.start_time, "%Y-%m-%dT%H:%M:%S") {
            let date_key = dt.format("%Y-%m-%d").to_string();
            let day_name = match dt.weekday() {
                chrono::Weekday::Mon => "Mon",
                chrono::Weekday::Tue => "Tue",
                chrono::Weekday::Wed => "Wed",
                chrono::Weekday::Thu => "Thu",
                chrono::Weekday::Fri => "Fri",
                chrono::Weekday::Sat => "Sat",
                chrono::Weekday::Sun => "Sun",
            };
            let acc = daily_map.entry(date_key).or_insert_with(|| DayAcc {
                day_name: day_name.to_string(),
                totals: crate::safety::SafetyTotals::default(),
                drives: 0,
                distance_km: 0.0,
            });
            acc.totals.distance_mi += d.distance_mi;
            acc.totals.assisted_mi += assisted_mi;
            acc.totals.night_mi += d.safety_night_mi;
            acc.totals.night_weighted_mi += d.safety_night_weighted_mi;
            acc.totals.moving_ms += d.safety_moving_ms;
            acc.totals.imu_moving_ms += d.safety_imu_moving_ms;
            acc.totals.night_ms += d.safety_night_ms;
            acc.totals.night_weighted_ms += d.safety_night_weighted_ms;
            acc.totals.manual_moving_ms += d.safety_manual_moving_ms;
            acc.totals.hard_brake_ms += d.safety_hard_brake_ms;
            acc.totals.hard_brake_events += d.safety_hard_brake_events;
            acc.totals.aggr_turn_ms += d.safety_aggr_turn_ms;
            acc.totals.aggr_turn_events += d.safety_aggr_turn_events;
            acc.totals.speeding_ms += d.safety_speeding_ms;
            acc.totals.brake_any_ms += d.safety_brake_any_ms;
            acc.totals.turn_any_ms += d.safety_turn_any_ms;
            acc.drives += 1;
            acc.distance_km += d.distance_km;
        }
    }

    let mut eligible_daily_scores: Vec<(crate::safety::SafetyScore, f64)> = Vec::new();
    let mut daily: Vec<SafetyDayStats> = daily_map
        .into_iter()
        .map(|(date, acc)| {
            let score_breakdown = crate::safety::compute_safety_score(&acc.totals);
            if let Some(score) = score_breakdown.as_ref() {
                eligible_daily_scores.push((score.clone(), acc.totals.distance_mi));
            }
            let score = score_breakdown.map(|s| s.score);
            let coverage_pct = if acc.totals.moving_ms > 0 {
                round1(100.0 * acc.totals.imu_moving_ms as f64 / acc.totals.moving_ms as f64)
            } else {
                0.0
            };
            SafetyDayStats {
                date,
                day_name: acc.day_name,
                score,
                drives: acc.drives,
                distance_mi: round2(acc.totals.distance_mi),
                distance_km: round2(acc.distance_km),
                hard_brake_events: acc.totals.hard_brake_events,
                aggr_turn_events: acc.totals.aggr_turn_events,
                speeding_ms: acc.totals.speeding_ms,
                night_mi: round2(acc.totals.night_mi),
                night_ms: acc.totals.night_ms,
                night_weighted_ms: acc.totals.night_weighted_ms,
                moving_ms: acc.totals.moving_ms,
                manual_moving_ms: acc.totals.manual_moving_ms,
                hard_brake_ms: acc.totals.hard_brake_ms,
                aggr_turn_ms: acc.totals.aggr_turn_ms,
                brake_any_ms: acc.totals.brake_any_ms,
                turn_any_ms: acc.totals.turn_any_ms,
                imu_moving_ms: acc.totals.imu_moving_ms,
                coverage_pct,
                eligible: score.is_some(),
            }
        })
        .collect();
    daily.sort_by(|a, b| a.date.cmp(&b.date));

    let mut best_day = String::new();
    let mut best_day_score: Option<f64> = None;
    for ds in &daily {
        if let Some(s) = ds.score {
            if best_day_score.is_none_or(|b| s > b) {
                best_day = ds.date.clone();
                best_day_score = Some(s);
            }
        }
    }

    let assisted_percent = if total_dist_km > 0.0 {
        round1(assisted_dist_km / total_dist_km * 100.0)
    } else {
        0.0
    };
    let compatible_distance_mi: f64 = eligible_daily_scores.iter().map(|(_, mi)| *mi).sum();
    let compatible_days = eligible_daily_scores.len() as i32;
    let coverage_pct = if totals.moving_ms > 0 {
        round1(100.0 * totals.imu_moving_ms as f64 / totals.moving_ms as f64)
    } else {
        0.0
    };
    let period_score = mileage_weighted_safety_breakdown(&eligible_daily_scores);

    SafetyAnalytics {
        model_id: crate::safety::MODEL_ID.to_string(),
        model_label: crate::safety::MODEL_LABEL.to_string(),
        period: period.to_string(),
        period_start: period_start_str,
        total_drives: period_drives.len() as i32,
        scored_drives,
        compatible_days,
        compatible_distance_mi: round2(compatible_distance_mi),
        total_native_distance_mi: round2(totals.distance_mi),
        coverage_pct,
        unavailable_factors: vec![
            "Unsafe following".to_string(),
            "Forced Autopilot disengagements".to_string(),
            "Unbuckled driving".to_string(),
            "Lead-relative speeding".to_string(),
            "Yellow-light braking exemption".to_string(),
        ],
        score: period_score,
        total_distance_mi: round2(totals.distance_mi),
        total_distance_km: round2(total_dist_km),
        moving_ms: totals.moving_ms,
        imu_moving_ms: totals.imu_moving_ms,
        night_ms: totals.night_ms,
        night_weighted_ms: totals.night_weighted_ms,
        manual_moving_ms: totals.manual_moving_ms,
        hard_brake_events: totals.hard_brake_events,
        hard_brake_ms: totals.hard_brake_ms,
        aggr_turn_events: totals.aggr_turn_events,
        aggr_turn_ms: totals.aggr_turn_ms,
        speeding_ms: totals.speeding_ms,
        brake_any_ms: totals.brake_any_ms,
        turn_any_ms: totals.turn_any_ms,
        night_mi: round2(totals.night_mi),
        night_km: round2(night_km),
        assisted_percent,
        fsd_disengagements,
        daily,
        best_day,
        best_day_score,
    }
}

fn mileage_weighted_safety_breakdown(
    days: &[(crate::safety::SafetyScore, f64)],
) -> Option<crate::safety::SafetyScore> {
    let total_miles: f64 = days.iter().map(|(_, miles)| miles.max(0.0)).sum();
    if total_miles <= 0.0 {
        return None;
    }
    let weighted = |pick: fn(&crate::safety::SafetyScore) -> f64| {
        days.iter()
            .map(|(score, miles)| pick(score) * miles.max(0.0))
            .sum::<f64>()
            / total_miles
    };
    Some(crate::safety::SafetyScore {
        score: round1(weighted(|s| s.score)),
        hard_brake_pct: round1(weighted(|s| s.hard_brake_pct)),
        aggr_turn_pct: round1(weighted(|s| s.aggr_turn_pct)),
        speeding_pct: round1(weighted(|s| s.speeding_pct)),
        night_pct: round1(weighted(|s| s.night_pct)),
        hard_brake_penalty: round1(weighted(|s| s.hard_brake_penalty)),
        aggr_turn_penalty: round1(weighted(|s| s.aggr_turn_penalty)),
        speeding_penalty: round1(weighted(|s| s.speeding_penalty)),
        night_penalty: round1(weighted(|s| s.night_penalty)),
        fsd_share_pct: round1(weighted(|s| s.fsd_share_pct)),
        fsd_relief_pct: 0.0,
    })
}

/// Resolve a drive id (numeric index or start-time string) to the
/// summary-path index **and** the file list that makes up that drive.
/// Used by `single_drive` to scope the full-BLOB decode to just the
/// clips in the requested drive rather than the whole store.
///
/// Returning both is load-bearing: the handler needs the numeric index
/// to stamp onto the resulting `Drive.id` so the UI's subsequent
/// `/api/drives/:id/*` calls keep lining up, and it needs the file
/// list for the targeted BLOB fetch. Returns `None` if the id doesn't
/// match any drive.
pub fn find_drive_files(
    summaries: &[RouteSummary],
    id: &str,
) -> Option<(usize, Vec<String>)> {
    let groups = group_summary_clips(summaries);

    let pick = |idx: usize| -> Vec<String> {
        // Dedupe parent files: when a clip's mid-clip park gap splits it
        // across two drives, each drive's sub-clip list references the
        // parent once; within a single drive a parent appears at most
        // once, but the dedupe is cheap insurance against future logic
        // changes that allow multiple sub-clips of the same parent in
        // one drive.
        let mut seen = std::collections::HashSet::new();
        groups[idx]
            .iter()
            .filter_map(|c| {
                if seen.insert(c.summary.file.as_str()) {
                    Some(c.summary.file.clone())
                } else {
                    None
                }
            })
            .collect()
    };

    if let Ok(idx) = id.parse::<usize>() {
        if idx < groups.len() {
            return Some((idx, pick(idx)));
        }
    }
    for (idx, group) in groups.iter().enumerate() {
        if group.is_empty() {
            continue;
        }
        let st = group[0]
            .timestamp
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        if st == id {
            return Some((idx, pick(idx)));
        }
    }
    None
}

/// Full drive_key → member-file mapping for cloud tag sync.
/// drive_key is the canonical start_time string
/// (`drive_tags.drive_key` join key, same formatting as
/// `find_drive_start_time`); files are the deduped parent clip paths.
/// One grouper pass for the whole store — the sync engine maps each
/// dirty drive to its member routeIds (push) and each changed cloud
/// route back to its drive (pull) from this.
pub fn drive_key_file_map(summaries: &[RouteSummary]) -> Vec<(String, Vec<String>)> {
    let groups = group_summary_clips(summaries);
    let mut out = Vec::with_capacity(groups.len());
    for group in groups.iter() {
        let Some(first) = group.first() else { continue };
        let key = first.timestamp.format("%Y-%m-%dT%H:%M:%S").to_string();
        let mut seen = std::collections::HashSet::new();
        let files: Vec<String> = group
            .iter()
            .filter_map(|c| {
                if seen.insert(c.summary.file.as_str()) {
                    Some(c.summary.file.clone())
                } else {
                    None
                }
            })
            .collect();
        out.push((key, files));
    }
    out
}

/// Build the same `DriveSummary` the Drives list would emit for the
/// drive at `idx`, using the BLOB-free aggregate path. The single-drive
/// API handler uses this to overlay the canonical headline percentages
/// (FSD %, autopilot %, TACC %, distances) onto its full `Drive`
/// response, so the number shown on the detail page matches the list
/// down to the rounding digit. Without this overlay the two paths
/// drift ~0.1–0.5 % because `build_drive_stats` walks the merged points
/// with outlier filtering while `build_summary_from_aggregates` sums
/// per-clip pre-computed columns — same data, different algorithm.
///
/// Returns `None` when `idx` is out of range.
pub fn build_summary_for_idx(
    summaries: &[RouteSummary],
    idx: usize,
    tags: &HashMap<String, Vec<String>>,
) -> Option<DriveSummary> {
    let groups = group_summary_clips(summaries);
    let clips = groups.get(idx)?;
    if clips.is_empty() {
        return None;
    }
    Some(build_summary_from_aggregates(clips, idx, tags))
}

/// Resolve a drive id to the canonical start_time string used as the
/// `drive_tags.drive_key` join key.
///
/// Accepts the same id forms `find_drive_files` does — numeric index
/// or `%Y-%m-%dT%H:%M:%S` start_time string — and always returns the
/// start_time string the grouper uses when joining tags onto drives
/// (`build_summary_from_aggregates` / `build_drive_stats`).
///
/// Without this resolver, `PUT /api/drives/{id}/tags` stored rows keyed
/// by the raw URL id (typically the numeric index), which never matched
/// the start_time key the list endpoint later read by — so tags were
/// written but never displayed.
pub fn find_drive_start_time(summaries: &[RouteSummary], id: &str) -> Option<String> {
    let groups = group_summary_clips(summaries);

    let key_of = |idx: usize| -> Option<String> {
        groups
            .get(idx)
            .and_then(|g| g.first())
            .map(|c| c.timestamp.format("%Y-%m-%dT%H:%M:%S").to_string())
    };

    if let Ok(idx) = id.parse::<usize>() {
        if let Some(st) = key_of(idx) {
            return Some(st);
        }
    }
    for group in groups.iter() {
        if let Some(first) = group.first() {
            let st = first.timestamp.format("%Y-%m-%dT%H:%M:%S").to_string();
            if st == id {
                return Some(st);
            }
        }
    }
    None
}

/// Output of [`summon_check_candidates`].
pub struct SummonCheckCandidates {
    /// Drives whose shape says "could hide a summon".
    pub candidate_drives: usize,
    /// Unique candidate clip files (drive order) lacking current flag
    /// evidence — the set worth re-reading from the USB.
    pub files: Vec<String>,
}

/// Candidate selection for the targeted summon evidence re-read — port
/// of Sentry-Drive's `check-summon` repair (electron-main.cjs). Returns
/// the clip files that could hide a summon drive but lack current flag
/// evidence, so the processor can re-read just those MP4s instead of
/// reprocessing the whole library.
///
/// Candidates come from two places (mirroring the JS selection):
///  1. Whole drives inside the summon speed/duration envelope (an
///     isolated summon that already grouped as its own tiny drive).
///  2. The LOW-SPEED EDGE CLIPS of every dashcam drive, plus ONE
///     boundary clip past each slow run. A summon fused onto a
///     following drive hides at its head (verified live on 2026-07-27:
///     a summon-end clip's row missed the trailing Park run, so the
///     park splitter never separated the summon from the hour of
///     driving after it — the merged drive fails the envelope and would
///     never be re-read). Summon only ever sits at a drive's edges (it
///     is always bracketed by Park), so edge clips at parking-lot speed
///     are the complete hiding set. The boundary clip matters because a
///     summon ending seconds before the human drives off shares its
///     final clip with fast driving — that mixed clip holds the end
///     bookend AND the park run that lets the splitter isolate the
///     summon.
///
/// Evidence is current only when every flag run carries per-run speed
/// (`max_mps`) — earlier extractions lacked it and their drives can
/// fail the speed gate on point-slice pollution, so they get one
/// upgrade re-read.
pub fn summon_check_candidates(summaries: &[RouteSummary]) -> SummonCheckCandidates {
    /// Summon duration cap is 10 min = at most 10 minute-clips per edge.
    const MAX_EDGE_CLIPS: usize = 10;
    // + 0.01 rounding guard: max_speed_mph is stored rounded to 2 dp.
    let max_mph = SUMMON_MAX_SPEED_MPS * calc::MPS_TO_MPH + 0.01;

    let by_file: HashMap<&str, &RouteSummary> =
        summaries.iter().map(|s| (s.file.as_str(), s)).collect();

    let has_current_evidence = |s: &RouteSummary| -> bool {
        !s.flag_runs.is_empty()
            && s.flag_runs
                .iter()
                .all(|r| r.max_mps.is_some_and(f64::is_finite))
            // v21: without ap_runs the Self Driving signature cannot
            // run, so maxMps-bearing flagRuns alone are NOT current.
            // Omitting this leaves every pre-v21 row looking up to date
            // and it is never re-read.
            && !s.ap_runs.is_empty()
    };

    // Sentry-Drive scans the route's per-sample |speeds|; summary rows
    // don't carry the speeds BLOB, so the v16 `sei_speed_abs_max`
    // column stands in — it IS the max |SEI speed| (reverse counts).
    // Pre-v16 rows fall back to the locked `max_speed_mps`
    // (positive-only, possibly GPS-derived): candidate selection only,
    // where a wrong call costs (or skips) a single clip re-read. No
    // speed evidence at all means "can't verify slow", matching the
    // JS empty-speeds bail.
    let route_is_slow = |file: &str| -> bool {
        let Some(r) = by_file.get(file) else {
            return false;
        };
        match r.aggregates.sei_speed_abs_max {
            Some(m) => m <= SUMMON_MAX_SPEED_MPS,
            None => {
                r.aggregates.speed_sample_count > 0
                    && r.aggregates.max_speed_mps <= SUMMON_MAX_SPEED_MPS
            }
        }
    };

    let mut candidate_drives = 0usize;
    let mut files_out: Vec<String> = Vec::new();
    let mut queued: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut add_clip = |file: &str| {
        // Synthetic gap-fill bridge rows have no MP4 on disk.
        if file.contains("-front-bridge.mp4") {
            return;
        }
        let Some(r) = by_file.get(file) else {
            return;
        };
        if has_current_evidence(r) {
            return;
        }
        if let Some((key, _)) = by_file.get_key_value(file) {
            if queued.insert(key) {
                files_out.push(file.to_string());
            }
        }
    };

    let groups = group_summary_clips(summaries);
    let empty_tags = HashMap::new();
    for (idx, clips) in groups.iter().enumerate() {
        if clips.is_empty() {
            continue;
        }
        // Deduped parent files in drive order (see `find_drive_files`).
        let mut seen = std::collections::HashSet::new();
        let files: Vec<&str> = clips
            .iter()
            .filter_map(|c| {
                if seen.insert(c.summary.file.as_str()) {
                    Some(c.summary.file.as_str())
                } else {
                    None
                }
            })
            .collect();
        if files.is_empty() {
            continue;
        }

        let d = build_summary_from_aggregates(clips, idx, &empty_tags);
        // Imported drives can't gain SEI evidence; flagged drives are done.
        if d.source.as_deref().is_some_and(|s| s != "sei") || d.summon {
            continue;
        }

        // No lower speed bound: reverse-only summons report NEGATIVE
        // SEI speeds, which the display stat ignores — such drives
        // show 0 mph.
        let whole_drive =
            d.max_speed_mph <= max_mph && d.duration_ms <= SUMMON_MAX_DURATION_MS;
        if whole_drive {
            candidate_drives += 1;
            for f in &files {
                add_clip(f);
            }
            continue;
        }

        // Low-speed head and tail of a faster drive (fused-summon
        // case): the slow run PLUS ONE boundary clip each way.
        let mut took = false;
        for &f in files.iter().take(MAX_EDGE_CLIPS) {
            if !route_is_slow(f) {
                if took {
                    add_clip(f); // boundary clip after the slow run
                }
                break;
            }
            add_clip(f);
            took = true;
        }
        let mut took_tail = false;
        for &f in files.iter().rev().take(MAX_EDGE_CLIPS) {
            if !route_is_slow(f) {
                if took_tail {
                    add_clip(f); // boundary clip before the slow tail
                }
                break;
            }
            add_clip(f);
            took_tail = true;
        }
        if took || took_tail {
            candidate_drives += 1;
        }
    }

    SummonCheckCandidates {
        candidate_drives,
        files: files_out,
    }
}

/// Build a full `Drive` (with all merged point data, gear/FSD arrays,
/// and FSD events) from a slice of routes whose parent clips are
/// **already known to belong to a single drive**. Skips the gap-split
/// half of `group_clips` so the caller can scope the expensive BLOB
/// decode via the summary path without re-running the full grouper
/// against the whole store.
///
/// The fetched parents are WHOLE clips, but a drive that shares a clip
/// with its neighbor (park-split mid-clip — every fused-summon shape)
/// must not drag the neighbor's points onto the detail map. The same
/// gear-state park splitter `group_clips` uses runs here, and
/// `target_start` — the summary drive's canonical `%Y-%m-%dT%H:%M:%S`
/// start time — picks the matching sub-drive. The two sides compute
/// segment starts differently (gear-frame offsets vs point-fraction
/// offsets), so the match is nearest-start rather than exact; adjacent
/// sub-drives are separated by a ≥2 s park, typically far more.
/// Without a target the first sub-drive wins.
///
/// `idx` is the drive's numeric index in the summary-path global list
/// — stamped onto `Drive.id` so the frontend's subsequent per-drive
/// calls line up.
pub fn build_single_drive_from_clips(
    routes: &[Route],
    idx: i32,
    tags: &HashMap<String, Vec<String>>,
    target_start: Option<&str>,
) -> Option<Drive> {
    if routes.is_empty() {
        return None;
    }

    let mut timed: Vec<TimedRoute> = routes
        .iter()
        .filter_map(|r| {
            // parse_clip_timestamp (basename), NOT parse_file_timestamp:
            // gap-fill routes are keyed at event-folder paths
            // (SentryClips/<event-ts>/<clip-ts>-front.mp4) whose folder
            // timestamp a left-to-right scan would win, placing the clip at
            // the event time instead of its own — a phantom gap in the
            // drive's point/speed timeline.
            parse_clip_timestamp(&r.file).map(|ts| TimedRoute {
                route: r.clone(),
                timestamp: ts,
                clip_span_ms: CLIP_DURATION_MS,
            })
        })
        .collect();
    if timed.is_empty() {
        return None;
    }
    timed.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
    // Real clip spans within this drive, so point timestamps and the end
    // time match the list view. Only this drive's clips were fetched, so
    // the final clip keeps the nominal minute — the handler overlays the
    // summary's canonical duration on top for exactly that reason.
    {
        let entries: Vec<(NaiveDateTime, bool)> = timed
            .iter()
            .map(|t| (t.timestamp, bounds_clip_span(&t.route.source, &t.route.file)))
            .collect();
        for (t, span) in timed.iter_mut().zip(clip_spans(&entries)) {
            t.clip_span_ms = span;
        }
    }

    let mut sub_drives = split_by_gear_state(timed);
    if sub_drives.is_empty() {
        return None;
    }
    let pick = if sub_drives.len() == 1 {
        0
    } else if let Some(t) = target_start
        .and_then(|s| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok())
    {
        sub_drives
            .iter()
            .enumerate()
            .min_by_key(|(_, g)| (g[0].timestamp - t).num_seconds().abs())
            .map(|(i, _)| i)
            .unwrap_or(0)
    } else {
        0
    };
    let clips = sub_drives.swap_remove(pick);
    Some(build_drive_stats(&clips, idx, tags))
}

// ---------------------------------------------------------------------------
// Internal: clip grouping
// ---------------------------------------------------------------------------

/// True when a route's `file` path lives under a Tesla event folder
/// (`SavedClips/` or `SentryClips/`). The `replace('\\', "/")` handles
/// drive-data.json imports that came from a Windows export (Sentry-Drive
/// writes backslashes in its file paths).
pub(crate) fn is_event_folder_path(file: &str) -> bool {
    let norm = file.replace('\\', "/");
    norm.starts_with("SavedClips/") || norm.starts_with("SentryClips/")
}

// ---------------------------------------------------------------------------
// Internal: event-clip gap-fill
//
// The processor only ingests RecentClips, so a hole in the continuous
// recording becomes a hole in the drive route — even when the same minute
// clips exist inside a SavedClips/SentryClips event folder (Tesla events
// carry a ~10-minute pre-roll). Gap-fill admits event clips in three
// shapes:
//   interior — timestamp strictly inside a RecentClips hole (a clip on
//              both sides, gap ≤ GAP_FILL_MAX_MS);
//   trailing — the drive ran PAST its last RecentClips clip into an event
//              pre-roll before parking (no clip after, so no "hole");
//   leading  — departure footage that only made an event pre-roll before
//              the first RecentClips clip.
// Edge shapes are admitted by chaining: the clip must sit within
// GAP_FILL_ADJ_MS of a RecentClips clip or of another eligible event clip,
// with the whole chain anchored to RecentClips and capped at
// GAP_FILL_MAX_MS from the nearest anchor. On top of proximity, admission
// is DRIVING-GATED wherever SEI is available: a clip whose telemetry shows
// no movement (gear Park throughout, speed ~0) is never admitted, so a
// parked car's sentry recordings — even ones minutes after a drive ended —
// stay out of the drive list and out of the playback manifest. Everything
// else in the event folders (parked recordings, duplicates of RecentClips
// clips, isolated clusters with no adjacent driving) stays filtered.
// ---------------------------------------------------------------------------

/// Clip timestamp parsed from the FILENAME component only. Event paths
/// embed the event-folder timestamp first (`SentryClips/<event_ts>/<clip_ts>-
/// front.mp4`), which a left-to-right `parse_file_timestamp` would win.
pub(crate) fn parse_clip_timestamp(file_path: &str) -> Option<NaiveDateTime> {
    let norm = file_path.replace('\\', "/");
    parse_file_timestamp(norm.rsplit('/').next().unwrap_or(&norm))
}

/// Holes in a SORTED continuous-clip timestamp sequence that qualify for
/// event-clip gap-fill: wider than one missing minute-clip but within the
/// fill cap — a longer gap is a park/drive boundary, not a recording
/// dropout.
pub(crate) fn fillable_holes(sorted_ts: &[NaiveDateTime]) -> Vec<(NaiveDateTime, NaiveDateTime)> {
    sorted_ts
        .windows(2)
        .filter(|w| {
            let gap = (w[1] - w[0]).num_milliseconds();
            gap > GAP_FILL_MIN_MS && gap <= GAP_FILL_MAX_MS
        })
        .map(|w| (w[0], w[1]))
        .collect()
}

/// True when `ts` lies strictly inside one of `holes` (sorted,
/// non-overlapping). Endpoints are occupied RecentClips slots and stay
/// RecentClips-owned.
pub(crate) fn ts_in_holes(holes: &[(NaiveDateTime, NaiveDateTime)], ts: NaiveDateTime) -> bool {
    let i = holes.partition_point(|h| h.0 < ts);
    i > 0 && ts < holes[i - 1].1
}

/// True when SEI telemetry shows the car moving: any non-Park gear frame
/// or any speed sample above a crawl. All-Park + speed≈0 (or no telemetry
/// at all — no positive evidence of driving) returns false.
pub(crate) fn telemetry_has_driving(
    gear_runs: &[GearRun],
    gear_states: &[u8],
    speeds: &[f32],
    raw_park_count: u32,
    raw_frame_count: u32,
) -> bool {
    telemetry_has_driving_core(
        gear_runs.iter().any(|r| r.gear != GEAR_PARK),
        gear_runs.is_empty(),
        gear_states.iter().any(|&g| g != GEAR_PARK),
        gear_states.is_empty(),
        raw_park_count,
        raw_frame_count,
        speeds.iter().any(|&s| s.abs() > GAP_FILL_MIN_SPEED_MPS),
    )
}

/// [`telemetry_has_driving`] over pre-reduced scalars, so the overview
/// planner can evaluate it from metadata without loading the arrays.
/// One source of truth: the array version above delegates here.
#[allow(clippy::fn_params_excessive_bools)]
pub(crate) fn telemetry_has_driving_core(
    gear_runs_non_park: bool,
    gear_runs_empty: bool,
    gear_states_non_park: bool,
    gear_states_empty: bool,
    raw_park_count: u32,
    raw_frame_count: u32,
    speeds_driving: bool,
) -> bool {
    gear_runs_non_park
        || gear_states_non_park
        || (gear_runs_empty
            && gear_states_empty
            && raw_frame_count > 0
            && raw_park_count < raw_frame_count)
        || speeds_driving
}

/// POSITIVE gear evidence only: at least one non-Park gear frame.
///
/// Deliberately NOT [`telemetry_has_driving`], which also accepts a
/// speed sample above a crawl and — when gear telemetry is missing
/// entirely — infers motion from raw frame counters. Those are fine for
/// admitting a clip next to an anchor, but they are not proof for
/// minting a drive out of an isolated event cluster: a single bogus
/// speed sample, a legacy/imported row with no gear RLE, or a partial
/// extract would all qualify. Gear comes straight from Tesla's SEI for
/// the recording car, so a non-Park frame cannot be produced by another
/// vehicle moving in view.
pub(crate) fn telemetry_gear_driving(gear_runs: &[GearRun], gear_states: &[u8]) -> bool {
    gear_runs.iter().any(|r| r.gear != GEAR_PARK)
        || gear_states.iter().any(|&g| g != GEAR_PARK)
}

/// [`telemetry_has_driving`] over a full Route row.
fn route_has_driving(r: &Route) -> bool {
    telemetry_has_driving(
        &r.gear_runs,
        &r.gear_states,
        &r.speeds,
        r.raw_park_count,
        r.raw_frame_count,
    )
}

/// [`telemetry_has_driving`] over a stored route row: gear runs + raw
/// frame counts + the pre-computed aggregate max speed (no per-frame
/// arrays available post-storage). The single predicate the grouper's
/// gap-fill admission AND the playback manifest (`Store::gap_fill_files`)
/// both apply to stored rows, so the two never disagree on whether an
/// event clip is driving.
pub(crate) fn row_has_driving(
    gear_runs: &[GearRun],
    raw_park_count: u32,
    raw_frame_count: u32,
    max_speed_mps: f64,
) -> bool {
    telemetry_has_driving(gear_runs, &[], &[], raw_park_count, raw_frame_count)
        || max_speed_mps > GAP_FILL_MIN_SPEED_MPS as f64
}

/// [`row_has_driving`] over a summary row.
fn summary_has_driving(s: &RouteSummary) -> bool {
    row_has_driving(
        &s.gear_runs,
        s.raw_park_count,
        s.raw_frame_count,
        s.aggregates.max_speed_mps,
    )
}

/// An event clip proposed for gap-fill. `driving` is the SEI verdict:
/// `Some(true)` = movement frames present, `Some(false)` = parked
/// throughout (never admitted), `None` = not yet extracted (the
/// processor's pre-extraction scan, which selects a timestamp-bounded
/// superset and lets the post-extraction gate discard the parked ones).
pub(crate) struct GapFillCandidate<'a> {
    pub(crate) ts: NaiveDateTime,
    pub(crate) file: &'a str,
    pub(crate) driving: Option<bool>,
    /// POSITIVE gear evidence — see [`telemetry_gear_driving`]. Stricter
    /// than `driving`: no speed clause and no raw-frame-count inference,
    /// so neither a lone bogus speed sample nor a row that simply lacks
    /// gear telemetry can qualify. Only unanchored-cluster admission
    /// consults this; anchored admission keeps the ordinary `driving`
    /// gate. Always false for pre-extraction candidates.
    pub(crate) gear_driving: bool,
}

/// Steps 2-3 of gap-fill admission (see [`select_gap_fill`]): drop
/// candidates that duplicate an occupied RecentClips slot (one-sided,
/// within GAP_FILL_DUP_MS after a recent clip — Tesla stamps the
/// Saved/Sentry twin of a segment 0-1 s later), then dedup overlapping
/// candidates in (timestamp, path) order so lowest path wins and
/// SavedClips/SentryClips twins of the same minute never both land.
/// `order` holds the still-eligible indices into `cands`; returns the
/// kept subset.
fn dedup_candidates(
    recent_sorted_ts: &[NaiveDateTime],
    cands: &[(NaiveDateTime, &str)],
    mut order: Vec<usize>,
) -> Vec<usize> {
    let dup_of_recent = |ts: NaiveDateTime| -> bool {
        let i = recent_sorted_ts.partition_point(|&r| r <= ts);
        i > 0 && (ts - recent_sorted_ts[i - 1]).num_milliseconds() <= GAP_FILL_DUP_MS
    };
    order.retain(|&i| !dup_of_recent(cands[i].0));
    order.sort_by(|&a, &b| cands[a].cmp(&cands[b]));
    let mut kept: Vec<usize> = Vec::new();
    for i in order {
        if let Some(&last) = kept.last()
            && (cands[i].0 - cands[last].0).num_milliseconds() <= GAP_FILL_DUP_MS
        {
            continue;
        }
        kept.push(i);
    }
    kept
}

/// Playback-manifest selection: event clips whose timestamp lies STRICTLY
/// INSIDE a fillable RecentClips hole, WITHOUT the driving gate. An
/// interior clip's minute is missing from RecentClips by definition
/// (Tesla moved it into the event folder), so cross-linking it back
/// cannot double-list footage — the reason drive-path admission is
/// driving-gated does not apply. Trailing/leading chains stay excluded
/// here: with no bounding recent clip, an ungated chain would pull whole
/// parked sentry sessions into RecentClips. Timestamp-only — no SEI
/// needed, so clips already rejected (and marked processed) by the
/// driving gate still qualify. Returns indices into `candidates`.
pub(crate) fn select_interior_fill(
    recent_sorted_ts: &[NaiveDateTime],
    candidates: &[(NaiveDateTime, &str)],
) -> Vec<usize> {
    if recent_sorted_ts.is_empty() || candidates.is_empty() {
        return Vec::new();
    }
    let holes = fillable_holes(recent_sorted_ts);
    if holes.is_empty() {
        return Vec::new();
    }
    dedup_candidates(recent_sorted_ts, candidates, (0..candidates.len()).collect())
        .into_iter()
        .filter(|&i| ts_in_holes(&holes, candidates[i].0))
        .collect()
}

/// Timestamp-only wrapper around [`select_gap_fill`] for callers without
/// SEI in hand (the processor's disk scan).
pub(crate) fn select_gap_fill_events(
    recent_sorted_ts: &[NaiveDateTime],
    candidates: &[(NaiveDateTime, &str)],
) -> Vec<usize> {
    let cands: Vec<GapFillCandidate> = candidates
        .iter()
        .map(|&(ts, file)| GapFillCandidate { ts, file, driving: None, gear_driving: false })
        .collect();
    select_gap_fill(recent_sorted_ts, &cands)
}

/// Select which event clips fill RecentClips gaps. `recent_sorted_ts` is
/// the sorted continuous-clip timeline. Returns indices into `candidates`.
///
/// Admission pipeline:
/// 1. drop clips whose SEI says parked-only (`driving == Some(false)`);
/// 2. drop duplicates of occupied RecentClips slots (timestamp within
///    GAP_FILL_DUP_MS after a recent clip — Tesla stamps the Saved/Sentry
///    twin of a segment 0-1 s later);
/// 3. dedup overlapping candidates (lowest path wins within the dup
///    window, so twins of the same minute never both land);
/// 4. admit what remains if it lies strictly inside a RecentClips hole
///    (interior) OR chains to the recent timeline: hops ≤ GAP_FILL_ADJ_MS
///    through recent clips / other kept candidates, anchored to at least
///    one recent clip, capped at GAP_FILL_MAX_MS from the nearest one
///    (bounds trailing/leading fills) — OR forms an unanchored cluster
///    whose every member carries positive gear-based SEI evidence
///    (`gear_driving`). A user save can swallow an ENTIRE short drive
///    (Tesla moves every minute of it out of RecentClips, leaving nothing
///    to anchor to — 2026-08-08 honk-save incident); ego gear frames
///    prove the recording car itself was moving, which parked sentry
///    footage can never show, so proximity has nothing left to guard.
///    Pre-extraction candidates (`driving == None`, `gear_driving`
///    false) never qualify unanchored, keeping the ingest scan bounded.
pub(crate) fn select_gap_fill(
    recent_sorted_ts: &[NaiveDateTime],
    candidates: &[GapFillCandidate],
) -> Vec<usize> {
    if candidates.is_empty() {
        return Vec::new();
    }
    let holes = fillable_holes(recent_sorted_ts);

    let nearest_recent_ms = |ts: NaiveDateTime| -> i64 {
        let i = recent_sorted_ts.partition_point(|&r| r < ts);
        let mut best = i64::MAX;
        if i > 0 {
            best = best.min((ts - recent_sorted_ts[i - 1]).num_milliseconds());
        }
        if i < recent_sorted_ts.len() {
            best = best.min((recent_sorted_ts[i] - ts).num_milliseconds());
        }
        best
    };

    // Steps 1-3: driving eligibility, then the shared dup-of-recent +
    // twin dedup in (timestamp, path) order.
    let pairs: Vec<(NaiveDateTime, &str)> =
        candidates.iter().map(|c| (c.ts, c.file)).collect();
    let order: Vec<usize> = (0..candidates.len())
        .filter(|&i| candidates[i].driving != Some(false))
        .collect();
    let kept = dedup_candidates(recent_sorted_ts, &pairs, order);

    // Step 4b: chain connectivity over the merged recent+kept timeline.
    // Clusters are maximal runs whose consecutive gaps stay within
    // GAP_FILL_ADJ_MS; a kept candidate is chained when its cluster holds
    // at least one recent clip and it sits within GAP_FILL_MAX_MS of the
    // nearest recent clip.
    let mut merged: Vec<(NaiveDateTime, Option<usize>)> = recent_sorted_ts
        .iter()
        .map(|&t| (t, None))
        .chain(kept.iter().enumerate().map(|(k, &i)| (candidates[i].ts, Some(k))))
        .collect();
    merged.sort_by_key(|&(t, _)| t);

    let mut chained = vec![false; kept.len()];
    let mut cluster_start = 0usize;
    for idx in 0..=merged.len() {
        let cluster_ends = idx == merged.len()
            || (idx > 0
                && (merged[idx].0 - merged[idx - 1].0).num_milliseconds() > GAP_FILL_ADJ_MS);
        if !cluster_ends {
            continue;
        }
        let cluster = &merged[cluster_start..idx];
        if cluster.iter().any(|&(_, k)| k.is_none()) {
            for &(ts, k) in cluster {
                if let Some(k) = k
                    && nearest_recent_ms(ts) <= GAP_FILL_MAX_MS
                {
                    chained[k] = true;
                }
            }
        } else if cluster
            .iter()
            .all(|&(_, k)| k.is_some_and(|k| candidates[kept[k]].gear_driving))
        {
            // Unanchored driving cluster: no recent clip to anchor to,
            // but every member has gear-verified ego movement — a real
            // drive Tesla relocated wholesale into an event folder.
            for &(_, k) in cluster {
                if let Some(k) = k {
                    chained[k] = true;
                }
            }
        }
        cluster_start = idx;
    }

    kept.iter()
        .enumerate()
        .filter(|&(k, &i)| ts_in_holes(&holes, candidates[i].ts) || chained[k])
        .map(|(_, &i)| i)
        .collect()
}

/// Real wall-clock span of each clip in a SORTED series, parallel to the
/// input: the gap to the next NATIVE clip when that gap is a positive
/// part of a minute, else the nominal minute.
///
/// Clips are nominally a minute, but a recording that stops early — the
/// last clip of a session, or one interrupted by an event — keeps its
/// frames and loses its duration. The next clip's start is the ground
/// truth for when this one ended.
///
/// Only a native dashcam clip can bound another one. Imported providers
/// synthesise clips on an exact minute grid and gap-fill invents bridge
/// routes, so an import interleaved with real footage would otherwise
/// declare a full minute of dashcam video to have lasted seconds.
/// Mirrors Sentry-Drive's `annotateClipSpans` / `clipSpanOf`.
fn clip_spans(entries: &[(NaiveDateTime, bool)]) -> Vec<i64> {
    let mut spans = vec![CLIP_DURATION_MS; entries.len()];
    let native: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, (_, is_native))| *is_native)
        .map(|(i, _)| i)
        .collect();
    for w in native.windows(2) {
        let gap = (entries[w[1]].0 - entries[w[0]].0).num_milliseconds();
        if gap > 0 && gap <= CLIP_DURATION_MS {
            spans[w[0]] = gap;
        }
    }
    spans
}

/// True for a clip that can bound another one's span: native SEI
/// footage, not a synthetic gap-fill bridge.
fn bounds_clip_span(source: &Option<String>, file: &str) -> bool {
    source.as_deref().is_none_or(|s| s == "sei") && !file.contains("-front-bridge.mp4")
}

/// Dedup by normalized file path, parse timestamps, sort, split on 5-min gaps,
/// then split by gear state transitions. Consumes the routes — no clones
/// of the point-heavy Route values.
fn group_clips(routes: Vec<Route>) -> Vec<Vec<TimedRoute>> {
    if routes.is_empty() {
        return Vec::new();
    }

    // Partition off routes that live under SavedClips/SentryClips event
    // folders BEFORE dedup. These contain (a) clips that duplicate
    // RecentClips data with a different path the dedup-by-path can't catch,
    // and (b) parked Sentry-mode recordings the gear-state splitter would
    // otherwise emit as a spurious "drive" bordering an actual trip.
    // Mirrors the discovery filter in processor.rs::scan_dir and
    // Sentry-Drive's process.js:91-94. The only event routes admitted are
    // gap-fills (see below); the rest are dropped exactly as before.
    let input_count = routes.len();
    let mut seen = HashMap::with_capacity(routes.len());
    let mut unique: Vec<Route> = Vec::with_capacity(routes.len());
    let mut event_candidates: Vec<Route> = Vec::new();
    let mut filtered_event_folder = 0usize;
    for r in routes {
        let norm = r.file.replace('\\', "/");
        if seen.insert(norm, ()).is_some() {
            continue;
        }
        if is_event_folder_path(&r.file) {
            event_candidates.push(r);
        } else {
            unique.push(r);
        }
    }
    let unique_count = unique.len();
    if unique_count + event_candidates.len() < input_count {
        warn!(
            "group_clips: dedup dropped {} duplicate-path route(s) (input={} unique={} event_candidates={})",
            input_count - unique_count - event_candidates.len(),
            input_count,
            unique_count,
            event_candidates.len(),
        );
    }

    // Parse timestamps and build TimedRoute references — record up to 10
    // dropped filenames so the most common cause of "missing drives on
    // import" (filenames lacking the YYYY-MM-DD_HH-MM-SS pattern) shows up
    // in operator logs.
    let mut dropped_examples: Vec<String> = Vec::new();
    let mut dropped_total: usize = 0;
    let mut timed: Vec<TimedRoute> = unique
        .into_iter()
        .filter_map(|r| match parse_clip_timestamp(&r.file) {
            Some(ts) => Some(TimedRoute {
                route: r,
                timestamp: ts,
                clip_span_ms: CLIP_DURATION_MS,
            }),
            None => {
                dropped_total += 1;
                if dropped_examples.len() < 10 {
                    dropped_examples.push(r.file.clone());
                }
                None
            }
        })
        .collect();
    if dropped_total > 0 {
        warn!(
            "group_clips: {} route(s) dropped — filename does not contain YYYY-MM-DD_HH-MM-SS pattern. Examples: {:?}",
            dropped_total, dropped_examples
        );
    }

    // Don't bail yet when every route lives under an event folder —
    // the gap-fill pass below can still admit unanchored driving
    // clusters (a store whose RecentClips twins all rotated off before
    // ingest would otherwise show zero drives forever).
    if timed.is_empty() && event_candidates.is_empty() {
        info!(
            "group_clips: input={} unique={} timed=0 groups=0 (no parseable timestamps)",
            input_count, unique_count
        );
        return Vec::new();
    }

    timed.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    // Gap-fill: admit event routes whose CLIP timestamp (filename, not
    // event-folder name) fills a RecentClips gap — interior hole or a
    // driving-gated trailing/leading chain (see the section comment
    // above). One route per timestamp; everything else stays filtered.
    if !event_candidates.is_empty() {
        let recent_ts: Vec<NaiveDateTime> = timed.iter().map(|t| t.timestamp).collect();
        let mut cands: Vec<(NaiveDateTime, Option<Route>)> = Vec::new();
        for r in event_candidates {
            match parse_clip_timestamp(&r.file) {
                Some(ts) => cands.push((ts, Some(r))),
                None => filtered_event_folder += 1,
            }
        }
        let keys: Vec<GapFillCandidate> = cands
            .iter()
            .map(|(ts, r)| {
                let r = r.as_ref().unwrap();
                GapFillCandidate {
                    ts: *ts,
                    file: r.file.as_str(),
                    driving: Some(route_has_driving(r)),
                    // gear_runs ONLY — deliberately ignoring the
                    // per-frame gear_states this path happens to have.
                    // The drives list is built from RouteSummary, which
                    // carries no gear_states, so consulting them here
                    // would admit unanchored clusters the list rejects
                    // (map shows a drive the list lacks). Anchored
                    // admission still uses the broader `driving` gate
                    // below, which keeps the richer evidence.
                    gear_driving: telemetry_gear_driving(&r.gear_runs, &[]),
                }
            })
            .collect();
        let mut gap_filled = 0usize;
        for i in select_gap_fill(&recent_ts, &keys) {
            let (ts, r) = &mut cands[i];
            timed.push(TimedRoute {
                route: r.take().unwrap(),
                timestamp: *ts,
                clip_span_ms: CLIP_DURATION_MS,
            });
            gap_filled += 1;
        }
        filtered_event_folder += cands.iter().filter(|(_, r)| r.is_some()).count();
        if gap_filled > 0 {
            timed.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
            info!(
                "group_clips: gap-filled {} event route(s) into RecentClips holes",
                gap_filled
            );
        }
        if filtered_event_folder > 0 {
            info!(
                "group_clips: filtered {} SavedClips/SentryClips route(s)",
                filtered_event_folder
            );
        }
    }
    // Real clip spans, computed once over the whole sorted series so
    // every consumer — the park splitter's sub-segments, drive end time,
    // and per-point timestamps — agrees on how long a clip lasted.
    {
        let entries: Vec<(NaiveDateTime, bool)> = timed
            .iter()
            .map(|t| {
                (
                    t.timestamp,
                    bounds_clip_span(&t.route.source, &t.route.file),
                )
            })
            .collect();
        for (t, span) in timed.iter_mut().zip(clip_spans(&entries)) {
            t.clip_span_ms = span;
        }
    }

    let timed_count = timed.len();
    if timed.is_empty() {
        return Vec::new();
    }

    // First pass: group by time gap
    let mut time_groups: Vec<Vec<TimedRoute>> = Vec::new();
    let mut current = vec![timed.remove(0)];

    for tr in timed {
        let gap_ms = (tr.timestamp - current.last().unwrap().timestamp)
            .num_milliseconds();
        if gap_ms > DRIVE_GAP_MS {
            time_groups.push(std::mem::take(&mut current));
        }
        current.push(tr);
    }
    if !current.is_empty() {
        time_groups.push(current);
    }

    // Second pass: split each time group further by gear state (Park transitions),
    // then by external signature (prevents Tessie drives from merging).
    let mut groups = Vec::new();
    for tg in time_groups {
        for gear_group in split_by_gear_state(tg) {
            for sig_group in split_by_external_signature(gear_group) {
                groups.push(sig_group);
            }
        }
    }
    info!(
        "group_clips: input={} unique={} timed={} groups={}",
        input_count,
        unique_count,
        timed_count,
        groups.len()
    );
    groups
}

// ---------------------------------------------------------------------------
// Overview planning (streaming map builder)
//
// Mirrors group_clips' topology over reduced metadata so drive fragments
// are known before any point BLOB is decoded. Every rule delegates to
// the same functions the in-memory path uses; the differential tests
// assert byte-equal output.
// ---------------------------------------------------------------------------

/// Median-cluster then neighbour-jump filtering over a drive's collected
/// points — shared by the in-memory overview path and the streaming
/// assembler so trails can never diverge between them.
pub(crate) fn overview_filter_points(pts: &mut Vec<GpsPoint>) {
    use crate::calc::MAX_FROM_MEDIAN_M;
    use crate::calc::MAX_JUMP_M;

    // Median-cluster filter: drop points >1000km from median
    if pts.len() > 2 {
        let q1 = pts.len() / 4;
        let q3 = pts.len() * 3 / 4;
        let count = q3 - q1 + 1;
        let mut sum_lat: f64 = 0.0;
        let mut sum_lng: f64 = 0.0;
        for i in q1..=q3 {
            sum_lat += pts[i][0];
            sum_lng += pts[i][1];
        }
        let med_lat = sum_lat / count as f64;
        let med_lng = sum_lng / count as f64;

        pts.retain(|p| calc::geodesic_m(p[0], p[1], med_lat, med_lng) <= MAX_FROM_MEDIAN_M);
    }

    // Neighbor-jump filter
    if pts.len() > 2 {
        let n = pts.len();
        let mut remove = vec![false; n];
        for i in 0..n {
            let has_prev = i > 0;
            let has_next = i < n - 1;
            let far_from_prev =
                has_prev && calc::geodesic_m(pts[i - 1][0], pts[i - 1][1], pts[i][0], pts[i][1]) > MAX_JUMP_M;
            let far_from_next =
                has_next && calc::geodesic_m(pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1]) > MAX_JUMP_M;
            if (has_prev && has_next && far_from_prev && far_from_next)
                || (!has_prev && far_from_next)
                || (!has_next && far_from_prev)
            {
                remove[i] = true;
            }
        }
        let mut write = 0;
        for read in 0..n {
            if !remove[read] {
                pts[write] = pts[read];
                write += 1;
            }
        }
        pts.truncate(write);
    }
}

/// Any |speed| above the gap-fill driving threshold — the speeds clause
/// of [`telemetry_has_driving`], shared with the DB-side meta loader.
pub(crate) fn speeds_driving(speeds: &[f32]) -> bool {
    speeds.iter().any(|&sp| sp.abs() > GAP_FILL_MIN_SPEED_MPS)
}

/// Reduced per-route facts — everything the grouping topology consults,
/// none of the point data.
pub(crate) struct OverviewClipMeta {
    pub(crate) file: String,
    pub(crate) n_points: usize,
    pub(crate) gear_runs: Vec<GearRun>,
    pub(crate) gear_states_len: usize,
    pub(crate) gear_states_non_park: bool,
    pub(crate) gear_states_park_count: usize,
    pub(crate) raw_park_count: u32,
    pub(crate) raw_frame_count: u32,
    /// Any |speed| above the gap-fill threshold. Consulted only for
    /// event rows.
    pub(crate) speeds_driving: bool,
    pub(crate) source: Option<String>,
    pub(crate) external_signature: Option<String>,
}

impl OverviewClipMeta {
    #[cfg(test)]
    pub(crate) fn from_route(r: &Route) -> Self {
        OverviewClipMeta {
            file: r.file.clone(),
            n_points: r.points.len(),
            gear_runs: r.gear_runs.clone(),
            gear_states_len: r.gear_states.len(),
            gear_states_non_park: r.gear_states.iter().any(|&g| g != GEAR_PARK),
            gear_states_park_count: r.gear_states.iter().filter(|&&g| g == GEAR_PARK).count(),
            raw_park_count: r.raw_park_count,
            raw_frame_count: r.raw_frame_count,
            speeds_driving: speeds_driving(&r.speeds),
            source: r.source.clone(),
            external_signature: r.external_signature.clone(),
        }
    }

    fn driving(&self) -> bool {
        telemetry_has_driving_core(
            self.gear_runs.iter().any(|g| g.gear != GEAR_PARK),
            self.gear_runs.is_empty(),
            self.gear_states_non_park,
            self.gear_states_len == 0,
            self.raw_park_count,
            self.raw_frame_count,
            self.speeds_driving,
        )
    }
}

/// One planned slice of one clip inside a drive.
pub(crate) struct PlannedFragment {
    pub(crate) meta_idx: usize,
    pub(crate) range: std::ops::Range<usize>,
    pub(crate) timestamp: NaiveDateTime,
}

pub(crate) struct PlannedDrive {
    pub(crate) fragments: Vec<PlannedFragment>,
}

struct TimedMeta {
    idx: usize,
    ts: NaiveDateTime,
}

/// group_clips over metadata. `metas` must be in the same order the
/// route query returns (ORDER BY file) — dedup keeps first occurrence.
pub(crate) fn plan_overviews(metas: &[OverviewClipMeta]) -> Vec<PlannedDrive> {
    let mut seen = std::collections::HashMap::new();
    let mut timed: Vec<TimedMeta> = Vec::new();
    let mut events: Vec<usize> = Vec::new();
    for (i, m) in metas.iter().enumerate() {
        let norm = m.file.replace('\\', "/");
        if seen.insert(norm, ()).is_some() {
            continue;
        }
        if is_event_folder_path(&m.file) {
            events.push(i);
        } else if let Some(ts) = parse_clip_timestamp(&m.file) {
            timed.push(TimedMeta { idx: i, ts });
        }
    }
    timed.sort_by(|a, b| a.ts.cmp(&b.ts));

    if !events.is_empty() {
        let recent_ts: Vec<NaiveDateTime> = timed.iter().map(|t| t.ts).collect();
        let mut cands: Vec<(NaiveDateTime, Option<usize>)> = Vec::new();
        for &i in &events {
            if let Some(ts) = parse_clip_timestamp(&metas[i].file) {
                cands.push((ts, Some(i)));
            }
        }
        let keys: Vec<GapFillCandidate> = cands
            .iter()
            .map(|(ts, i)| {
                let m = &metas[i.unwrap()];
                GapFillCandidate {
                    ts: *ts,
                    file: m.file.as_str(),
                    driving: Some(m.driving()),
                    // gear_runs ONLY — mirrors group_clips (see its
                    // comment on map/list unanchored-cluster parity).
                    gear_driving: m.gear_runs.iter().any(|g| g.gear != GEAR_PARK),
                }
            })
            .collect();
        let mut admitted = 0usize;
        for k in select_gap_fill(&recent_ts, &keys) {
            let (ts, i) = &mut cands[k];
            timed.push(TimedMeta { idx: i.take().unwrap(), ts: *ts });
            admitted += 1;
        }
        if admitted > 0 {
            timed.sort_by(|a, b| a.ts.cmp(&b.ts));
        }
    }
    if timed.is_empty() {
        return Vec::new();
    }

    let mut time_groups: Vec<Vec<TimedMeta>> = Vec::new();
    let mut current = vec![timed.remove(0)];
    for tm in timed {
        if (tm.ts - current.last().unwrap().ts).num_milliseconds() > DRIVE_GAP_MS {
            time_groups.push(std::mem::take(&mut current));
        }
        current.push(tm);
    }
    if !current.is_empty() {
        time_groups.push(current);
    }

    let mut drives = Vec::new();
    for tg in time_groups {
        for gg in plan_split_by_gear(metas, tg) {
            for sg in plan_split_by_signature(metas, gg) {
                drives.push(PlannedDrive { fragments: sg });
            }
        }
    }
    drives
}

fn whole_fragment(metas: &[OverviewClipMeta], tm: &TimedMeta) -> PlannedFragment {
    PlannedFragment {
        meta_idx: tm.idx,
        range: 0..metas[tm.idx].n_points,
        timestamp: tm.ts,
    }
}

/// split_by_gear_state over metadata.
fn plan_split_by_gear(
    metas: &[OverviewClipMeta],
    group: Vec<TimedMeta>,
) -> Vec<Vec<PlannedFragment>> {
    if group.is_empty() {
        return Vec::new();
    }
    let has_gear_runs = group.iter().any(|t| !metas[t.idx].gear_runs.is_empty());
    if !has_gear_runs {
        return plan_split_legacy(metas, group);
    }

    let mut result: Vec<Vec<PlannedFragment>> = Vec::new();
    let mut current: Vec<PlannedFragment> = Vec::new();
    for tm in group.iter() {
        let m = &metas[tm.idx];
        if m.gear_runs.is_empty() {
            current.push(whole_fragment(metas, tm));
            continue;
        }
        match plan_clip_at_park_gaps(&m.gear_runs, m.n_points) {
            None => {
                // Mirrors the slicer: an unsplit gear-run clip is kept
                // only when it has points.
                if m.n_points > 0 {
                    current.push(whole_fragment(metas, tm));
                }
            }
            Some(plan) => {
                for seg in plan {
                    if seg.parked {
                        if !current.is_empty() {
                            result.push(std::mem::take(&mut current));
                        }
                    } else {
                        current.push(PlannedFragment {
                            meta_idx: tm.idx,
                            range: seg.range,
                            timestamp: tm.ts + chrono::Duration::seconds(seg.offset_secs),
                        });
                    }
                }
            }
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        // Everything parked: the whole group passes through unsplit.
        return vec![group.iter().map(|tm| whole_fragment(metas, tm)).collect()];
    }
    result
}

/// split_by_gear_state_legacy over metadata — including its quirk of
/// returning EMPTY (not the original group) when every clip is parked.
fn plan_split_legacy(
    metas: &[OverviewClipMeta],
    group: Vec<TimedMeta>,
) -> Vec<Vec<PlannedFragment>> {
    let as_whole =
        |g: &[TimedMeta]| g.iter().map(|tm| whole_fragment(metas, tm)).collect::<Vec<_>>();
    if group.len() <= 1 {
        return vec![as_whole(&group)];
    }
    let has_gear = group.iter().any(|t| metas[t.idx].gear_states_len > 0);
    if !has_gear {
        return vec![as_whole(&group)];
    }

    let mut result: Vec<Vec<PlannedFragment>> = Vec::new();
    let mut current: Vec<PlannedFragment> = Vec::new();
    for tm in group.iter() {
        let m = &metas[tm.idx];
        if mostly_parked_legacy_core(
            m.raw_park_count,
            m.raw_frame_count,
            m.gear_states_park_count,
            m.gear_states_len,
        ) {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(whole_fragment(metas, tm));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

/// split_by_external_signature over fragments — same HashMap bucket
/// construct, so its (nondeterministic) multi-signature ordering
/// matches the production path's behavior class.
fn plan_split_by_signature(
    metas: &[OverviewClipMeta],
    group: Vec<PlannedFragment>,
) -> Vec<Vec<PlannedFragment>> {
    if group.len() <= 1 {
        return vec![group];
    }
    let has_any = group
        .iter()
        .any(|f| metas[f.meta_idx].external_signature.is_some());
    if !has_any {
        return vec![group];
    }

    let mut buckets: std::collections::HashMap<String, Vec<PlannedFragment>> =
        std::collections::HashMap::new();
    let mut no_sig: Vec<PlannedFragment> = Vec::new();
    for frag in group {
        match &metas[frag.meta_idx].external_signature {
            Some(sig) => buckets.entry(sig.clone()).or_default().push(frag),
            None => no_sig.push(frag),
        }
    }

    let mut result = Vec::new();
    if !no_sig.is_empty() {
        result.push(no_sig);
    }
    for bucket in buckets.into_values() {
        result.push(bucket);
    }
    result
}

/// Render one planned drive. `points_for` yields a clip's full decoded
/// point array by meta index; the caller controls batching and reuse.
pub(crate) fn overview_from_fragments(
    id: i32,
    drive: &PlannedDrive,
    metas: &[OverviewClipMeta],
    points_for: &mut dyn FnMut(usize) -> anyhow::Result<std::rc::Rc<Vec<GpsPoint>>>,
    max_points_per_drive: usize,
) -> anyhow::Result<RouteOverview> {
    let mut pts: Vec<GpsPoint> = Vec::new();
    for f in &drive.fragments {
        let all = points_for(f.meta_idx)?;
        // Clamped: a row rewritten between the plan and this read can
        // shrink the array; a stale range must degrade, not panic. The
        // next generation bump rebuilds from consistent data.
        let end = f.range.end.min(all.len());
        let start = f.range.start.min(end);
        for p in &all[start..end] {
            if !(p[0].abs() < 1.0 && p[1].abs() < 1.0) {
                pts.push([p[0], p[1]]);
            }
        }
    }
    overview_filter_points(&mut pts);

    let first = drive.fragments.first();
    Ok(RouteOverview {
        id,
        points: downsample(&pts, max_points_per_drive),
        source: first.and_then(|f| metas[f.meta_idx].source.clone()),
        start_time: first
            .map(|f| f.timestamp.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Internal: external-signature splitting (Tessie drives)
// ---------------------------------------------------------------------------

/// Split a group by `external_signature`. Clips without a signature (native
/// SEI) stay as one group. Clips with different signatures become separate
/// groups. This prevents Tessie-imported drives from merging with each other
/// — the grouper's time-gap / park-gap heuristics can't reliably tell two
/// back-to-back Tessie drives apart, but the signature is unambiguous.
///
/// Port of Sentry-Drive's `splitByExternalSignature`.
fn split_by_external_signature(group: Vec<TimedRoute>) -> Vec<Vec<TimedRoute>> {
    if group.len() <= 1 {
        return vec![group];
    }
    let has_any = group.iter().any(|c| c.route.external_signature.is_some());
    if !has_any {
        return vec![group];
    }

    let mut buckets: std::collections::HashMap<String, Vec<TimedRoute>> =
        std::collections::HashMap::new();
    let mut no_sig: Vec<TimedRoute> = Vec::new();

    for clip in group {
        match &clip.route.external_signature {
            Some(sig) => buckets.entry(sig.clone()).or_default().push(clip),
            None => no_sig.push(clip),
        }
    }

    let mut result = Vec::new();
    if !no_sig.is_empty() {
        result.push(no_sig);
    }
    for bucket in buckets.into_values() {
        result.push(bucket);
    }
    result
}

// ---------------------------------------------------------------------------
// Internal: gear-state splitting
// ---------------------------------------------------------------------------

/// Split a group of clips into sub-groups when gear state shows a Park period
/// >= PARK_GAP_SECONDS. Uses GearRuns for sub-clip precision when available,
/// falls back to clip-level heuristic for legacy data.
fn split_by_gear_state(group: Vec<TimedRoute>) -> Vec<Vec<TimedRoute>> {
    if group.is_empty() {
        return Vec::new();
    }

    let has_gear_runs = group.iter().any(|c| !c.route.gear_runs.is_empty());
    if !has_gear_runs {
        return split_by_gear_state_legacy(group);
    }

    let mut result: Vec<Vec<TimedRoute>> = Vec::new();
    let mut current: Vec<TimedRoute> = Vec::new();

    for clip in group.iter() {
        if clip.route.gear_runs.is_empty() {
            current.push(clip.clone());
            continue;
        }

        let segments = split_clip_at_park_gaps(clip);
        for seg in segments {
            if seg.parked {
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
            } else if !seg.route.route.points.is_empty() {
                current.push(seg.route);
            }
        }
    }
    if !current.is_empty() {
        result.push(current);
    }

    // If everything was parked, return original group to avoid losing data
    if result.is_empty() {
        return vec![group];
    }
    result
}

/// A portion of a clip — either a driving segment or a park boundary marker.
struct ClipSegment {
    route: TimedRoute,
    parked: bool,
}

/// One planned fragment of a clip: point range + segment timestamp
/// offset, or a park boundary marker. Pure index math over gear runs
/// and the point COUNT — the single source of truth for the
/// frame-to-point mapping, shared by the in-memory splitter below and
/// the streaming overview builder, which plans fragments without
/// decoding point BLOBs.
pub(crate) struct PlannedSeg {
    pub(crate) range: std::ops::Range<usize>,
    pub(crate) offset_secs: i64,
    /// This segment's share of the clip's raw frames — how much of the
    /// clip's real wall-clock span it occupies.
    pub(crate) span_frac: f64,
    pub(crate) parked: bool,
}

/// `None` — the clip stays whole (no gear runs, or no park gap long
/// enough to split at).
pub(crate) fn plan_clip_at_park_gaps(
    gear_runs: &[GearRun],
    n_points: usize,
) -> Option<Vec<PlannedSeg>> {
    let total_raw_frames: u32 = gear_runs.iter().map(|r| r.frames).sum();
    if total_raw_frames == 0 {
        return None;
    }

    let seconds_per_frame = 60.0 / total_raw_frames as f64;

    // Identify raw segments that are park gaps
    struct RawSeg {
        start_frame: u32,
        end_frame: u32,
        parked: bool,
    }

    let mut raw_segs = Vec::new();
    let mut frame: u32 = 0;
    for run in gear_runs {
        let duration = run.frames as f64 * seconds_per_frame;
        let is_park_gap = run.gear == GEAR_PARK && duration >= PARK_GAP_SECONDS;
        raw_segs.push(RawSeg {
            start_frame: frame,
            end_frame: frame + run.frames,
            parked: is_park_gap,
        });
        frame += run.frames;
    }

    // Merge consecutive non-parked segments
    let mut merged: Vec<RawSeg> = Vec::new();
    for seg in raw_segs {
        if let Some(last) = merged.last_mut() {
            if !last.parked && !seg.parked {
                last.end_frame = seg.end_frame;
                continue;
            }
        }
        merged.push(seg);
    }

    if !merged.iter().any(|s| s.parked) {
        return None;
    }

    // Map raw frame ranges to deduped point indices
    let mut out = Vec::new();
    for seg in &merged {
        if seg.parked {
            out.push(PlannedSeg {
                range: 0..0,
                offset_secs: 0,
                span_frac: 0.0,
                parked: true,
            });
            continue;
        }

        let start_frac = seg.start_frame as f64 / total_raw_frames as f64;
        let end_frac = seg.end_frame as f64 / total_raw_frames as f64;

        let mut start_idx = (start_frac * n_points as f64).round() as usize;
        let mut end_idx = (end_frac * n_points as f64).round() as usize;

        if start_idx >= n_points {
            start_idx = n_points.saturating_sub(1);
        }
        if end_idx > n_points {
            end_idx = n_points;
        }
        // A non-empty frame span always keeps at least one point. Both
        // fractions round to the same index when a segment is short in
        // frames and GPS dedup left few points, which used to delete the
        // segment outright — along with the flag/gear evidence Summon
        // detection reads from it. Only the trailing segment escaped,
        // rescued by the start_idx clamp above; leading and middle
        // segments vanished, so whether a maneuver was detected depended
        // on how much the car had moved. The frame bounds carry the real
        // duration, so a one-point segment still measures its true
        // length. Ported from Sentry-Drive's splitClipAtParkGaps.
        if end_idx <= start_idx {
            end_idx = start_idx + 1;
        }
        if end_idx > n_points {
            continue; // no points at all — nothing to slice
        }

        out.push(PlannedSeg {
            range: start_idx..end_idx,
            // Deliberately the NOMINAL minute, not the clip's real span.
            // This offset lands in the segment's timestamp, which becomes
            // the drive's start_time — and start_time is the key user
            // drive tags are stored under (`drive_tags.drive_key`), with
            // no fallback and no migration. Using the real span here
            // would move drives and silently orphan every tag on them at
            // the next cache rebuild. An imprecise label is worth less
            // than the user's own data; the span below carries the real
            // duration.
            offset_secs: (start_frac * 60.0) as i64,
            span_frac: end_frac - start_frac,
            parked: false,
        });
    }
    Some(out)
}

/// Analyse a clip's GearRuns and split its points at any Park gap >=
/// PARK_GAP_SECONDS. Returns one or more segments.
fn split_clip_at_park_gaps(clip: &TimedRoute) -> Vec<ClipSegment> {
    let Some(plan) = plan_clip_at_park_gaps(&clip.route.gear_runs, clip.route.points.len())
    else {
        return vec![ClipSegment {
            route: clip.clone(),
            parked: false,
        }];
    };

    let mut result = Vec::new();
    for seg in plan {
        if seg.parked {
            result.push(ClipSegment {
                route: TimedRoute {
                    route: Route::empty(),
                    timestamp: clip.timestamp,
                    clip_span_ms: 0,
                },
                parked: true,
            });
            continue;
        }

        let (start_idx, end_idx) = (seg.range.start, seg.range.end);
        let seg_points = clip.route.points[start_idx..end_idx].to_vec();

        let seg_gears = if clip.route.gear_states.len() >= end_idx {
            clip.route.gear_states[start_idx..end_idx].to_vec()
        } else {
            Vec::new()
        };

        let seg_ap = if clip.route.autopilot_states.len() >= end_idx {
            clip.route.autopilot_states[start_idx..end_idx].to_vec()
        } else {
            Vec::new()
        };

        let seg_speeds = if clip.route.speeds.len() >= end_idx {
            clip.route.speeds[start_idx..end_idx].to_vec()
        } else {
            Vec::new()
        };

        let seg_accel = if clip.route.accel_positions.len() >= end_idx {
            clip.route.accel_positions[start_idx..end_idx].to_vec()
        } else {
            Vec::new()
        };
        let seg_ax = if clip.route.accel_x.len() >= end_idx {
            clip.route.accel_x[start_idx..end_idx].to_vec()
        } else {
            Vec::new()
        };
        let seg_ay = if clip.route.accel_y.len() >= end_idx {
            clip.route.accel_y[start_idx..end_idx].to_vec()
        } else {
            Vec::new()
        };

        let offset = chrono::Duration::seconds(seg.offset_secs);
        // The sub-segment lasts its own fraction of the parent's real
        // span, which is what the drive's end time and per-point
        // timestamps are built from.
        let seg_span_ms = (clip.clip_span_ms as f64 * seg.span_frac).round() as i64;

        result.push(ClipSegment {
            route: TimedRoute {
                route: Route {
                    file: clip.route.file.clone(),
                    date: clip.route.date.clone(),
                    points: seg_points,
                    gear_states: seg_gears,
                    autopilot_states: seg_ap,
                    speeds: seg_speeds,
                    accel_positions: seg_accel,
                    raw_park_count: 0,
                    raw_frame_count: 0,
                    gear_runs: Vec::new(),
                    // Parent's full-clip flag RLE rides along (mirrors
                    // Sentry-Drive's `{...clip}` sub-segment spread) —
                    // flag runs live in RAW frame space, so slicing
                    // them to the segment would corrupt the frame
                    // indexing.
                    flag_runs: clip.route.flag_runs.clone(),
                    // Same for the autopilot RLE: raw frame space, read
                    // through the segment's own [start_frame, end_frame)
                    // bounds by the detector, never sliced here.
                    ap_runs: clip.route.ap_runs.clone(),
                    accel_x: seg_ax,
                    accel_y: seg_ay,
                    source: clip.route.source.clone(),
                    external_signature: clip.route.external_signature.clone(),
                    tessie_autopilot_percent: clip.route.tessie_autopilot_percent,
                    // BLE rollup belongs to the whole clip's 60s window;
                    // copy through to each derived sub-segment so the
                    // per-drive aggregator can still see start/end
                    // battery, temps, etc.
                    battery_pct_start: clip.route.battery_pct_start,
                    battery_pct_end: clip.route.battery_pct_end,
                    interior_temp_min: clip.route.interior_temp_min,
                    interior_temp_max: clip.route.interior_temp_max,
                    exterior_temp_avg: clip.route.exterior_temp_avg,
                    hvac_runtime_s: clip.route.hvac_runtime_s,
                    tire_fl_psi: clip.route.tire_fl_psi,
                    tire_fr_psi: clip.route.tire_fr_psi,
                    tire_rl_psi: clip.route.tire_rl_psi,
                    tire_rr_psi: clip.route.tire_rr_psi,
                    odometer_mi_start: clip.route.odometer_mi_start,
                    odometer_mi_end: clip.route.odometer_mi_end,
                    location_name_start: clip.route.location_name_start.clone(),
                    location_name_end: clip.route.location_name_end.clone(),
                    temp_samples: clip.route.temp_samples.clone(),
                },
                timestamp: clip.timestamp + offset,
                clip_span_ms: seg_span_ms,
            },
            parked: false,
        });
    }

    result
}

/// Legacy fallback for routes without GearRuns. Clips that are majority Park
/// are treated as drive boundaries.
fn split_by_gear_state_legacy(group: Vec<TimedRoute>) -> Vec<Vec<TimedRoute>> {
    if group.len() <= 1 {
        return vec![group];
    }

    let has_gear = group.iter().any(|c| !c.route.gear_states.is_empty());
    if !has_gear {
        return vec![group];
    }

    let mut result: Vec<Vec<TimedRoute>> = Vec::new();
    let mut current: Vec<TimedRoute> = Vec::new();

    for clip in group {
        if clip_is_mostly_parked_legacy(&clip) {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(clip);
        }
    }
    if !current.is_empty() {
        result.push(current);
    }

    if result.is_empty() {
        // Cannot reconstruct `group` since we consumed it — return empty.
        // This mirrors the Go code returning the original group to avoid data loss,
        // but in practice if result is empty and we consumed the clips, we've already
        // determined they're all parked. The Go code returns the original group as a
        // single-element slice so the drive still shows up. We rebuild it.
        // Since we moved the clips out, we can't recover them. Instead we rely on the
        // caller (split_by_gear_state) to handle the empty case — but that path only
        // reaches here for legacy data without gear runs, which is rare.
        return Vec::new();
    }
    result
}

/// Returns true if the clip is majority Park (legacy heuristic).
fn clip_is_mostly_parked_legacy(clip: &TimedRoute) -> bool {
    let park_count = clip
        .route
        .gear_states
        .iter()
        .filter(|&&g| g == GEAR_PARK)
        .count();
    mostly_parked_legacy_core(
        clip.route.raw_park_count,
        clip.route.raw_frame_count,
        park_count,
        clip.route.gear_states.len(),
    )
}

/// [`clip_is_mostly_parked_legacy`] over pre-reduced scalars, shared
/// with the overview planner. The array version delegates here.
pub(crate) fn mostly_parked_legacy_core(
    raw_park_count: u32,
    raw_frame_count: u32,
    gear_park_count: usize,
    gear_states_len: usize,
) -> bool {
    if raw_frame_count > 0 {
        return (raw_park_count as f64 / raw_frame_count as f64) > calc::PARK_MAJORITY_FRACTION;
    }
    if gear_states_len == 0 {
        return false;
    }
    gear_park_count > gear_states_len / 2
}

// ---------------------------------------------------------------------------
// Summon detection
//
// Port of Sentry-Drive's flagRunsOverlap / detectSummon (drive-calc.cjs).
// Evidence lives in RAW SEI frame space (flag_runs + park-split segment
// bounds), so it is immune to GPS dedup and to the fraction-based
// frame→point index mapping the splitter uses.
// ---------------------------------------------------------------------------

/// Per-clip summon evidence, one entry per clip of the drive in drive
/// order. `[start_frame, end_frame)` bounds the drive's segment of that
/// clip in raw SEI frame space (the full clip when the park splitter
/// left it whole).
///
/// `ap_runs` / `gear_runs` feed the Self Driving signature only; empty
/// means "no evidence", which that signature treats as unverifiable.
/// The hazard signature never reads them — HW3 reports autopilot Off for
/// a whole maneuver and is detected on hazards alone.
pub(crate) struct SummonClipEvidence<'a> {
    pub flag_runs: &'a [FlagRun],
    pub ap_runs: &'a [ApRun],
    pub gear_runs: &'a [GearRun],
    pub start_frame: u32,
    pub end_frame: u32,
    pub total_frames: u32,
}

/// True when an RLE spans exactly `total_frames` raw frames — the proof
/// that it is in the same index space as the segment bounds. Mirrors
/// Sentry-Drive's `runsSpanFrames`.
///
/// Cross-tool, not merely defensive: every RLE here is built before GPS
/// dedup, and one built after is shorter, indexes point space, and reads
/// the wrong frames without failing. Absent and empty deliberately
/// collapse to the same `false` — both mean "cannot be judged on this
/// path", and the drive falls back to the hazard signature.
fn runs_span_frames(frames: impl Iterator<Item = u32>, total_frames: u32) -> bool {
    if total_frames == 0 {
        return false;
    }
    let mut sum: u64 = 0;
    let mut any = false;
    for f in frames {
        if f == 0 {
            return false;
        }
        any = true;
        sum += f as u64;
    }
    any && sum == total_frames as u64
}

/// Gear at a raw frame index — `None` when `gear_runs` is empty or ends
/// before it. Mirrors Sentry-Drive's `gearAtFrame`.
fn gear_at_frame(gear_runs: &[GearRun], frame: u32) -> Option<u8> {
    let mut end: u64 = 0;
    for run in gear_runs {
        end += run.frames as u64;
        if (frame as u64) < end {
            return Some(run.gear);
        }
    }
    None
}

/// True when a segment's own gear evidence shows the car at rest in Park
/// on the far side of the given end — the frame before `start_frame`, or
/// the frame after `end_frame`, falling back to the boundary frame
/// itself where the segment runs to the edge of its clip. Mirrors
/// Sentry-Drive's `segmentBoundedByPark`.
///
/// A Summon maneuver is always park-to-park. A drive is not: the grouper
/// also cuts on clip-time gaps, so an unfilled hole in RecentClips
/// leaves a fragment of an ordinary trip that begins and ends
/// mid-motion. Without this gate such a fragment is tagged Summon and —
/// because a detected Summon zeroes its own FSD statistics — a genuine
/// FSD drive is erased from the lifetime numbers.
///
/// Park is read at the ends and nowhere else, deliberately: a maneuver
/// never shifts into Park part-way through, so no interior gear state
/// needs handling. Direction is unconstrained — forward-only,
/// reverse-only, and reverse-then-forward maneuvers all occur, and
/// neither signature inspects gear direction.
fn segment_bounded_by_park(c: &SummonClipEvidence, at_end: bool) -> bool {
    if at_end {
        if let Some(after) = gear_at_frame(c.gear_runs, c.end_frame) {
            return after == GEAR_PARK;
        }
        // end_frame > start_frame >= 0, so end_frame >= 1 — no underflow.
        return gear_at_frame(c.gear_runs, c.end_frame - 1) == Some(GEAR_PARK);
    }
    if c.start_frame > 0 {
        return gear_at_frame(c.gear_runs, c.start_frame - 1) == Some(GEAR_PARK);
    }
    gear_at_frame(c.gear_runs, 0) == Some(GEAR_PARK)
}

/// Frame counts by autopilot mode over the `ap_runs` overlapping a
/// segment's `[start_frame, end_frame)`. Mirrors Sentry-Drive's
/// `segmentApFrames`.
pub(crate) struct SegmentApFrames {
    pub fsd: u64,
    /// Autosteer/TACC (and any unknown non-Off state) — all need a
    /// driver, so any of them rules Summon out.
    pub other: u64,
    pub total: u64,
}

/// `None` when the clip carries no autopilot run evidence, or none of it
/// overlaps the segment.
fn segment_ap_frames(c: &SummonClipEvidence) -> Option<SegmentApFrames> {
    if c.ap_runs.is_empty() {
        return None;
    }
    let mut frame: u64 = 0;
    let mut fsd: u64 = 0;
    let mut other: u64 = 0;
    let mut total: u64 = 0;
    for run in c.ap_runs {
        let start = frame;
        let end = frame + run.frames as u64;
        frame = end;
        if end <= c.start_frame as u64 {
            continue;
        }
        if start >= c.end_frame as u64 {
            break;
        }
        let overlap = end.min(c.end_frame as u64) - start.max(c.start_frame as u64);
        total += overlap;
        if run.ap == AUTOPILOT_FSD {
            fsd += overlap;
        } else if run.ap != AUTOPILOT_OFF {
            other += overlap;
        }
    }
    if total == 0 {
        return None;
    }
    Some(SegmentApFrames { fsd, other, total })
}

/// True when any flag run overlapping `[from_frame, to_frame)` carries
/// `mask` bits: all of them when `require_all` (hazards = left AND right
/// in the SAME run), any of them otherwise (pedal input = brake OR
/// accel).
pub(crate) fn flag_runs_overlap(
    runs: &[FlagRun],
    from_frame: u32,
    to_frame: u32,
    mask: u8,
    require_all: bool,
) -> bool {
    let mut frame: u32 = 0;
    for run in runs {
        let start = frame;
        let end = frame + run.frames;
        frame = end;
        if end <= from_frame {
            continue;
        }
        if start >= to_frame {
            break;
        }
        let bits = run.flags & mask;
        if require_all {
            if bits == mask {
                return true;
            }
        } else if bits != 0 {
            return true;
        }
    }
    false
}

/// Frame-space max |SEI speed| over the flag runs overlapping a
/// segment's `[start_frame, end_frame)` — `None` when any overlapping
/// run predates per-run speed evidence (pre-`max_mps` extraction), so
/// callers can fall back. Mirrors Sentry-Drive's `segmentMaxSpeed`.
fn segment_max_speed(c: &SummonClipEvidence) -> Option<f64> {
    let mut frame: u32 = 0;
    let mut max = 0.0f64;
    for run in c.flag_runs {
        let start = frame;
        let end = frame + run.frames;
        frame = end;
        if end <= c.start_frame {
            continue;
        }
        if start >= c.end_frame {
            break;
        }
        let m = run.max_mps.filter(|m| m.is_finite())?;
        if m > max {
            max = m;
        }
    }
    Some(max)
}

/// Detect a Summon / Smart Summon drive from per-clip SEI flag evidence.
///
/// The verified signature: hazards within the opening seconds of the
/// first segment AND the closing seconds of the last, no pedal input
/// anywhere in between, and the whole drive at parking-lot speed. A
/// driverless car is the only thing that satisfies all three at once — a
/// human repositioning with hazards on still touches a pedal or exceeds
/// the summon speed cap.
///
/// Speed gate, frame-accurate when possible: per-run `max_mps` evidence
/// is immune to the dedup point-slice overshoot that can leak the
/// following drive's speed into a summon segment's stats. Legacy
/// evidence (no `max_mps`) falls back to `max_speed_mps` — the max
/// **absolute** SEI speed (Reverse reports negative) — and there
/// GPS-derived speeds are still untrustworthy at summon magnitudes, so
/// `has_sei_speeds` is required.
pub(crate) fn detect_summon(
    clips: &[SummonClipEvidence],
    max_speed_mps: f64,
    duration_ms: i64,
    has_sei_speeds: bool,
) -> bool {
    if clips.is_empty() {
        return false;
    }
    if !(duration_ms > 0) || duration_ms > SUMMON_MAX_DURATION_MS {
        return false;
    }

    // Every clip needs flag evidence — a single pre-flags clip (older
    // extraction, or routes written by a tool that hasn't ported
    // flagRuns yet) makes the drive unverifiable, and unverifiable must
    // mean "not summon".
    // Bounds are validated rather than trusted: the hazard bookend
    // windows are sized from total_frames but walked over flag_runs
    // offsets, so runs indexing a different space would put the window
    // in the wrong place — and that path returns true, making it the
    // failure-open direction.
    for c in clips {
        if c.flag_runs.is_empty()
            || c.total_frames == 0
            || c.end_frame <= c.start_frame
            || c.end_frame > c.total_frames
            || !runs_span_frames(c.flag_runs.iter().map(|r| r.frames), c.total_frames)
        {
            return false;
        }
    }

    // Every veto below is scoped to the frames of the clips actually
    // present, so wall-clock the evidence does not cover is
    // unconstrained. A hole in RecentClips shorter than the
    // drive-splitting gap leaves ONE drive whose visible ends are a
    // slow, pedal-free, park-bracketed departure and arrival while the
    // fast middle is simply missing — an ordinary trip wearing a
    // maneuver's ends. Require the segments to account for the drive's
    // own duration. The tolerance is under one clip so a single missing
    // minute is caught, and comfortably over the sub-clip timing skew of
    // a park split.
    let mut covered_ms = 0.0f64;
    for c in clips {
        covered_ms += ((c.end_frame - c.start_frame) as f64 * CLIP_DURATION_MS as f64)
            / c.total_frames as f64;
    }
    if duration_ms as f64 - covered_ms > SUMMON_MAX_UNCOVERED_MS as f64 {
        return false;
    }

    let mut speed_mps = 0.0f64;
    let mut frame_accurate = true;
    for c in clips {
        match segment_max_speed(c) {
            None => {
                frame_accurate = false;
                break;
            }
            Some(m) => {
                if m > speed_mps {
                    speed_mps = m;
                }
            }
        }
    }
    if !frame_accurate {
        if !has_sei_speeds {
            return false;
        }
        speed_mps = max_speed_mps;
    }
    // `!(x > 0.0)` rather than `x <= 0.0`: NaN fails both comparisons, so
    // the negated form rejects a NaN speed while `<=` would admit it —
    // same semantics as Sentry-Drive's `!(speedMps > 0)`.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    if !(speed_mps > 0.0) || speed_mps > SUMMON_MAX_SPEED_MPS {
        return false;
    }

    const HAZARD: u8 = FLAG_BLINKER_LEFT | FLAG_BLINKER_RIGHT;
    const PEDAL: u8 = FLAG_BRAKE | FLAG_ACCEL;

    for c in clips {
        if flag_runs_overlap(c.flag_runs, c.start_frame, c.end_frame, PEDAL, false) {
            return false;
        }
    }

    // Seconds→frames via the clip's own frame density, so variable SEI
    // rates (and short final clips) keep the window at real seconds.
    // CEILING division, in integers (RUST-SUMMON-PORT.md "Determinism"):
    // plain `/` truncates and would size every window one frame short.
    let bookend_frames = |c: &SummonClipEvidence| -> u32 {
        let num = c.total_frames as u64 * SUMMON_BOOKEND_SECONDS as u64 * 1000;
        (num.div_ceil(CLIP_DURATION_MS as u64) as u32).max(1)
    };
    let first = &clips[0];
    let last = &clips[clips.len() - 1];
    // The windows reach one bookend-length PAST the segment into the
    // same clip, because the flash brackets the maneuver, not the
    // driving. The car flashes while still in Park before it pulls away
    // and again after it has parked itself, and the splitter cuts the
    // drive exactly at those two shifts — so the evidence routinely
    // lies just outside the segment. Measured on 17 days of real clips:
    // one further maneuver detected, no other drive affected.
    //
    // The pedal veto above deliberately does NOT widen with it. Those
    // same parked frames hold the owner's own brake press from the
    // drive before or after, so widening it drops real detections from
    // 9 to 2 on that library.
    let hazard_at_start = flag_runs_overlap(
        first.flag_runs,
        first.start_frame.saturating_sub(bookend_frames(first)),
        first.end_frame.min(first.start_frame + bookend_frames(first)),
        HAZARD,
        true,
    );
    let hazard_at_end = flag_runs_overlap(
        last.flag_runs,
        last.start_frame.max(last.end_frame.saturating_sub(bookend_frames(last))),
        last.total_frames.min(last.end_frame + bookend_frames(last)),
        HAZARD,
        true,
    );
    if hazard_at_start && hazard_at_end {
        return true;
    }

    // ── Self Driving signature ──
    // Both extra RLEs must span the clip's raw frames: runs from a
    // producer that indexes a different space would be walked with the
    // wrong bounds, and a wrong answer is worse than no answer.
    for c in clips {
        if !runs_span_frames(c.ap_runs.iter().map(|r| r.frames), c.total_frames) {
            return false;
        }
        if !runs_span_frames(c.gear_runs.iter().map(|r| r.frames), c.total_frames) {
            return false;
        }
    }

    // Park-to-park only, so a slow pedal-free fragment of a longer FSD
    // trip cannot pass as a maneuver. Missing gear evidence reads as not
    // bracketed — the safe direction: hazards already covered those
    // drives before autopilot state was recorded at all.
    if !segment_bounded_by_park(first, false) || !segment_bounded_by_park(last, true) {
        return false;
    }

    // Needs autopilot evidence on every clip, or the drive's FSD share
    // is measured over an unknown fraction of it.
    let mut fsd_frames: u64 = 0;
    let mut ap_frames: u64 = 0;
    for c in clips {
        match segment_ap_frames(c) {
            None => return false,
            Some(ap) => {
                if ap.other > 0 {
                    return false;
                }
                fsd_frames += ap.fsd;
                ap_frames += ap.total;
            }
        }
    }
    // Sentry-Drive writes `fsdFrames >= apFrames * SUMMON_MIN_FSD_SHARE`,
    // multiplying rather than dividing so both sides stay exact in
    // IEEE-754. The share is 1/2, so the integer form below is the same
    // comparison with no float involved at all — it cannot disagree.
    debug_assert_eq!(SUMMON_MIN_FSD_SHARE, 0.5);
    ap_frames > 0 && fsd_frames * 2 >= ap_frames
}

// ---------------------------------------------------------------------------
// BuildSingleDrive — full point data for one drive
// ---------------------------------------------------------------------------

/// Build a full Drive with merged point arrays, gear/FSD state arrays, and FSD
/// events for a single drive identified by index.
fn build_drive_stats(
    clips: &[TimedRoute],
    idx: i32,
    tags: &HashMap<String, Vec<String>>,
) -> Drive {
    let first_clip = &clips[0];
    let last_clip = &clips[clips.len() - 1];
    let start_time = first_clip.timestamp;
    // A park-split drive ends at its segment boundary; an unsplit clip
    // runs to the end of its own span, which is a minute only when the
    // recording ran that long.
    let end_time =
        last_clip.timestamp + chrono::Duration::milliseconds(last_clip.clip_span_ms);

    // Merge all points with interpolated timestamps and metadata
    struct AnnotatedPoint {
        lat: f64,
        lng: f64,
        time_ms: f64,
        ap_state: u8,
        gear: u8,
        sei_speed: f32,
        accel_pos: f32,
    }

    let mut all_points: Vec<AnnotatedPoint> = Vec::new();

    for clip in clips {
        let clip_start = clip.timestamp.and_utc().timestamp_millis() as f64;
        let n = clip.route.points.len();
        // Spread this clip's points across the span they actually cover.
        // A park-split segment covers its own fraction of the clip, not
        // the whole clip — stamping its points across a full minute
        // pushes them past the drive's own end and makes the next clip's
        // first point step backwards, which inflates every
        // duration-weighted statistic built from them.
        let clip_duration_ms: f64 = clip.clip_span_ms as f64;
        let has_ap = clip.route.autopilot_states.len() == n;
        let has_gears = clip.route.gear_states.len() == n;
        let has_speeds = clip.route.speeds.len() == n;
        let has_accel = clip.route.accel_positions.len() == n;

        for i in 0..n {
            let t = if n > 1 {
                clip_start + (clip_duration_ms * i as f64 / (n - 1) as f64)
            } else {
                clip_start
            };
            all_points.push(AnnotatedPoint {
                lat: clip.route.points[i][0],
                lng: clip.route.points[i][1],
                time_ms: t,
                ap_state: if has_ap {
                    clip.route.autopilot_states[i]
                } else {
                    0
                },
                gear: if has_gears {
                    clip.route.gear_states[i]
                } else {
                    0
                },
                sei_speed: if has_speeds {
                    clip.route.speeds[i]
                } else {
                    0.0
                },
                accel_pos: if has_accel {
                    clip.route.accel_positions[i]
                } else {
                    0.0
                },
            });
        }
    }

    // Remove null island
    all_points.retain(|p| !(p.lat.abs() < 1.0 && p.lng.abs() < 1.0));

    // Filter GPS outliers
    if all_points.len() > 2 {
        // Step 1: median location from middle 50%
        let q1 = all_points.len() / 4;
        let q3 = all_points.len() * 3 / 4;
        let count = q3 - q1 + 1;
        let mut med_lat: f64 = 0.0;
        let mut med_lng: f64 = 0.0;
        for i in q1..=q3 {
            med_lat += all_points[i].lat;
            med_lng += all_points[i].lng;
        }
        med_lat /= count as f64;
        med_lng /= count as f64;

        // Step 2: remove points >1000 km from median
        use crate::calc::MAX_FROM_MEDIAN_M;
        all_points.retain(|p| calc::geodesic_m(p.lat, p.lng, med_lat, med_lng) <= MAX_FROM_MEDIAN_M);

        // Step 3: remove isolated outliers far from both neighbors
        use crate::calc::MAX_JUMP_M;
        let n = all_points.len();
        if n > 2 {
            let mut remove = vec![false; n];
            for i in 0..n {
                let has_prev = i > 0;
                let has_next = i < n - 1;
                let far_from_prev = has_prev
                    && calc::geodesic_m(
                        all_points[i - 1].lat,
                        all_points[i - 1].lng,
                        all_points[i].lat,
                        all_points[i].lng,
                    ) > MAX_JUMP_M;
                let far_from_next = has_next
                    && calc::geodesic_m(
                        all_points[i].lat,
                        all_points[i].lng,
                        all_points[i + 1].lat,
                        all_points[i + 1].lng,
                    ) > MAX_JUMP_M;
                if (has_prev && has_next && far_from_prev && far_from_next)
                    || (!has_prev && far_from_next)
                    || (!has_next && far_from_prev)
                {
                    remove[i] = true;
                }
            }
            let mut write = 0;
            for read in 0..n {
                if !remove[read] {
                    if write != read {
                        // Safe to move since we only write to already-processed indices
                        all_points.swap(write, read);
                    }
                    write += 1;
                }
            }
            all_points.truncate(write);
        }
    }

    // Compute distance and speeds
    let has_sei_speeds = all_points.iter().any(|p| p.sei_speed > 0.0);

    let mut total_distance_m: f64 = 0.0;
    let mut max_speed_mps: f64 = 0.0;
    let mut speeds_vec: Vec<f64> = Vec::new();

    for i in 1..all_points.len() {
        let d = calc::geodesic_m(
            all_points[i - 1].lat,
            all_points[i - 1].lng,
            all_points[i].lat,
            all_points[i].lng,
        );
        total_distance_m += d;

        if has_sei_speeds {
            let speed = all_points[i].sei_speed as f64;
            if speed >= 0.0 && speed < 100.0 {
                speeds_vec.push(speed);
                if speed > max_speed_mps {
                    max_speed_mps = speed;
                }
            }
        } else {
            let dt = (all_points[i].time_ms - all_points[i - 1].time_ms) / 1000.0;
            if dt > 0.0 {
                let speed = d / dt;
                if speed < 70.0 {
                    speeds_vec.push(speed);
                    if speed > max_speed_mps {
                        max_speed_mps = speed;
                    }
                }
            }
        }
    }

    let avg_speed_mps = if !speeds_vec.is_empty() {
        speeds_vec.iter().sum::<f64>() / speeds_vec.len() as f64
    } else {
        0.0
    };

    // Build point data array: [lat, lng, timeMs, speedMps].
    //
    // `timeMs` is **relative** to the drive's start_time (ms since the
    // drive began). The frontend reconstructs absolute wall-clock time
    // via `new Date(start_time) + timeMs`, which only works when the
    // offset is relative — emitting Unix-ms here caused the scrubber
    // and map info card to display garbage (off by the local timezone
    // offset, because the doubled epoch wrapped around modulo 24h).
    let drive_start_ms = start_time.and_utc().timestamp_millis() as f64;
    let mut point_data: Vec<[f64; 4]> = Vec::with_capacity(all_points.len());
    let mut gear_states: Vec<i32> = Vec::with_capacity(all_points.len());
    let mut fsd_states: Vec<i32> = Vec::with_capacity(all_points.len());
    let mut has_fsd_data = false;
    let mut has_gear_data = false;

    for (i, p) in all_points.iter().enumerate() {
        let speed = if has_sei_speeds {
            p.sei_speed as f64
        } else if i > 0 {
            let d = calc::geodesic_m(
                all_points[i - 1].lat,
                all_points[i - 1].lng,
                p.lat,
                p.lng,
            );
            let dt = (p.time_ms - all_points[i - 1].time_ms) / 1000.0;
            if dt > 0.0 {
                (d / dt).min(70.0)
            } else {
                0.0
            }
        } else {
            0.0
        };
        point_data.push([p.lat, p.lng, p.time_ms - drive_start_ms, round2(speed)]);
        gear_states.push(p.gear as i32);
        if p.gear != GEAR_PARK {
            has_gear_data = true;
        }
        fsd_states.push(p.ap_state as i32);
        if p.ap_state != AUTOPILOT_OFF {
            has_fsd_data = true;
        }
    }

    // Compute autopilot analytics
    let mut fsd_engaged_ms: i64 = 0;
    let mut fsd_disengagements: i32 = 0;
    let mut fsd_accel_pushes: i32 = 0;
    let mut fsd_distance_m: f64 = 0.0;
    let mut autosteer_engaged_ms: i64 = 0;
    let mut autosteer_distance_m: f64 = 0.0;
    let mut tacc_engaged_ms: i64 = 0;
    let mut tacc_distance_m: f64 = 0.0;
    let mut assisted_distance_m: f64 = 0.0;
    let mut fsd_events: Vec<FsdEvent> = Vec::new();

    if has_fsd_data && all_points.len() > 1 {
        let mut in_accel_press = false;
        let mut accel_press_lat: f64 = 0.0;
        let mut accel_press_lng: f64 = 0.0;
        let mut fsd_engage_time_ms: f64 = 0.0;

        let mut pending_disengage = false;
        let mut pending_disengage_time_ms: f64 = 0.0;
        let mut pending_disengage_lat: f64 = 0.0;
        let mut pending_disengage_lng: f64 = 0.0;

        for i in 1..all_points.len() {
            let prev = &all_points[i - 1];
            let cur = &all_points[i];
            let dt = cur.time_ms - prev.time_ms;
            let d = calc::geodesic_m(prev.lat, prev.lng, cur.lat, cur.lng);

            let prev_fsd = prev.ap_state == AUTOPILOT_FSD;
            let cur_fsd = cur.ap_state == AUTOPILOT_FSD;
            let cur_engaged = cur.ap_state != AUTOPILOT_OFF;

            // Resolve any pending FSD disengagement
            if pending_disengage {
                let time_since = cur.time_ms - pending_disengage_time_ms;
                if cur.gear == GEAR_PARK && time_since <= 2000.0 {
                    pending_disengage = false;
                } else if time_since > 2000.0 || cur_fsd {
                    fsd_disengagements += 1;
                    fsd_events.push(FsdEvent {
                        lat: pending_disengage_lat,
                        lng: pending_disengage_lng,
                        event_type: "disengagement".to_string(),
                    });
                    pending_disengage = false;
                }
            }

            // Track FSD engagement start
            if !prev_fsd && cur_fsd {
                in_accel_press = false;
                fsd_engage_time_ms = cur.time_ms;
            }

            // Count engaged time and distance by mode
            if cur_engaged {
                assisted_distance_m += d;
                match cur.ap_state {
                    x if x == AUTOPILOT_FSD => {
                        fsd_engaged_ms += dt as i64;
                        fsd_distance_m += d;
                    }
                    x if x == AUTOPILOT_AUTOSTEER => {
                        autosteer_engaged_ms += dt as i64;
                        autosteer_distance_m += d;
                    }
                    x if x == AUTOPILOT_TACC => {
                        tacc_engaged_ms += dt as i64;
                        tacc_distance_m += d;
                    }
                    _ => {}
                }
            }

            // Detect FSD disengagement — defer for Park grace period
            if prev_fsd && !cur_fsd {
                pending_disengage = true;
                pending_disengage_time_ms = cur.time_ms;
                pending_disengage_lat = cur.lat;
                pending_disengage_lng = cur.lng;
                in_accel_press = false;
            }

            // Normalize pedal position
            let mut accel_pct = cur.accel_pos as f64;
            if accel_pct <= 1.0 {
                accel_pct *= 100.0;
            }

            // Detect start of human accelerator press while FSD active
            if cur_fsd
                && !in_accel_press
                && accel_pct > 1.0
                && (cur.time_ms - fsd_engage_time_ms) >= 3000.0
            {
                in_accel_press = true;
                accel_press_lat = cur.lat;
                accel_press_lng = cur.lng;
            }

            // Press complete when pedal returns to 0%
            if in_accel_press && accel_pct <= 0.0 {
                fsd_accel_pushes += 1;
                fsd_events.push(FsdEvent {
                    lat: accel_press_lat,
                    lng: accel_press_lng,
                    event_type: "accel_push".to_string(),
                });
                in_accel_press = false;
            }
        }

        // Flush pending disengagement at end of drive
        if pending_disengage && !all_points.is_empty() {
            if all_points.last().unwrap().gear != GEAR_PARK {
                fsd_disengagements += 1;
                fsd_events.push(FsdEvent {
                    lat: pending_disengage_lat,
                    lng: pending_disengage_lng,
                    event_type: "disengagement".to_string(),
                });
            }
        }
    }

    let duration_ms = (end_time - start_time).num_milliseconds();
    let (fsd_percent, autosteer_percent, tacc_percent, assisted_percent) =
        compute_autopilot_percents(
            total_distance_m,
            fsd_distance_m,
            autosteer_distance_m,
            tacc_distance_m,
            assisted_distance_m,
        );

    let gear_state_result = if has_gear_data {
        gear_states
    } else {
        Vec::new()
    };
    let fsd_state_result = if has_fsd_data {
        fsd_states
    } else {
        Vec::new()
    };

    let start_time_str = start_time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let drive_tags = tags.get(&start_time_str).cloned().unwrap_or_default();

    Drive {
        id: idx,
        date: first_clip.route.date.clone(),
        start_time: start_time_str,
        end_time: end_time.format("%Y-%m-%dT%H:%M:%S").to_string(),
        duration_ms,
        distance_mi: round2(total_distance_m / calc::M_PER_MILE),
        distance_km: round2(total_distance_m / 1000.0),
        avg_speed_mph: round2(avg_speed_mps * calc::MPS_TO_MPH),
        max_speed_mph: round2(max_speed_mps * calc::MPS_TO_MPH),
        avg_speed_kmh: round2(avg_speed_mps * 3.6),
        max_speed_kmh: round2(max_speed_mps * 3.6),
        clip_count: clips.len(),
        point_count: all_points.len(),
        points: point_data,
        gear_states: gear_state_result,
        fsd_states: fsd_state_result,
        fsd_events,
        tags: drive_tags,
        fsd_engaged_ms,
        fsd_disengagements,
        fsd_accel_pushes,
        fsd_percent,
        fsd_distance_km: round2(fsd_distance_m / 1000.0),
        fsd_distance_mi: round2(fsd_distance_m / calc::M_PER_MILE),
        autosteer_engaged_ms,
        autosteer_percent,
        autosteer_distance_km: round2(autosteer_distance_m / 1000.0),
        autosteer_distance_mi: round2(autosteer_distance_m / calc::M_PER_MILE),
        tacc_engaged_ms,
        tacc_percent,
        tacc_distance_km: round2(tacc_distance_m / 1000.0),
        tacc_distance_mi: round2(tacc_distance_m / calc::M_PER_MILE),
        assisted_percent,
        // Summon is decided on the summary path, which carries the
        // park-split segment frame bounds this full-Route path lacks
        // (build_single_drive_from_clips receives whole clips). The
        // single-drive API handler overlays the canonical summary value,
        // exactly like the headline percentages above it.
        summon: false,
        source: first_clip.route.source.clone(),
        external_signature: first_clip.route.external_signature.clone(),
        tessie_autopilot_percent: first_clip.route.tessie_autopilot_percent,
    }
}

// ---------------------------------------------------------------------------
// GroupRoutesOverview — downsampled routes for map display
// ---------------------------------------------------------------------------

/// Returns downsampled route polylines for every drive, with outlier filtering.
fn group_routes_overview(routes: Vec<Route>, max_points_per_drive: usize) -> Vec<RouteOverview> {
    let groups = group_clips(routes);
    let mut result = Vec::with_capacity(groups.len());

    for (idx, clips) in groups.iter().enumerate() {
        // Collect valid (non-null-island) lat/lng from each clip
        let mut pts: Vec<GpsPoint> = Vec::new();
        for clip in clips {
            for p in &clip.route.points {
                if !(p[0].abs() < 1.0 && p[1].abs() < 1.0) {
                    pts.push([p[0], p[1]]);
                }
            }
        }

        overview_filter_points(&mut pts);

        let source = clips.first().and_then(|c| c.route.source.clone());
        // Format matches build_summary_from_aggregates (grouper.rs ~2588) so
        // the frontend can join /api/drives/routes entries against the cached
        // /api/drives DriveSummary list by start_time. The integer `id` is
        // kept for backwards compatibility but the two enumeration paths
        // (group_clips here vs. group_summary_clips for the list cache) can
        // produce different indices when sub-clip splitting occurs.
        let start_time = clips
            .first()
            .map(|c| c.timestamp.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();
        result.push(RouteOverview {
            id: idx as i32,
            points: downsample(&pts, max_points_per_drive),
            source,
            start_time,
        });
    }

    result
}

// ---------------------------------------------------------------------------
// FSD analytics (period-based breakdown)
// ---------------------------------------------------------------------------

/// Build FSD analytics from pre-computed drive summaries.
fn build_fsd_analytics(summaries: &[DriveSummary], period: &str) -> FsdAnalytics {
    let now = chrono::Local::now().naive_local();
    let today = now.date();

    let period_start: Option<NaiveDate> = match period {
        "day" => Some(today),
        "week" => Some(today - chrono::Duration::days(7)),
        _ => None, // "all" or "trip" — no filter
    };

    let period_start_str = period_start
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default();

    // Filter drives in period. Imported drives (tessie, teslascope, …)
    // are excluded from FSD analytics entirely — their autopilot data is
    // inferred or absent, not dashcam SEI telemetry, so mixing them
    // would dilute the score. Summon drives are excluded too (mirrors
    // Sentry-Drive's aggregate builder): driverless with autopilot_state
    // unset, they'd otherwise read as fake "0% FSD" drives — dragging
    // down the score and padding the per-drive disengagement averages
    // with trips no human (or FSD) ever drove.
    let period_drives: Vec<&DriveSummary> = summaries
        .iter()
        .filter(|d| {
            if is_imported(&d.source) || d.summon {
                return false;
            }
            if let Some(ps) = period_start {
                if let Ok(dt) =
                    NaiveDateTime::parse_from_str(&d.start_time, "%Y-%m-%dT%H:%M:%S")
                {
                    return dt.date() >= ps;
                }
                return false;
            }
            true
        })
        .collect();

    let mut fsd_engaged_ms: i64 = 0;
    let mut total_dist_km: f64 = 0.0;
    let mut total_dist_mi: f64 = 0.0;
    let mut fsd_dist_km: f64 = 0.0;
    let mut fsd_dist_mi: f64 = 0.0;
    let mut disengagements: i32 = 0;
    let mut accel_pushes: i32 = 0;
    let mut fsd_sessions: i32 = 0;
    let mut autosteer_engaged_ms: i64 = 0;
    let mut tacc_engaged_ms: i64 = 0;
    let mut autosteer_dist_km: f64 = 0.0;
    let mut autosteer_dist_mi: f64 = 0.0;
    let mut tacc_dist_km: f64 = 0.0;
    let mut tacc_dist_mi: f64 = 0.0;

    // Daily breakdown
    let mut daily_map: HashMap<String, FsdDayStats> = HashMap::new();
    // Track total distance per day for percent calculation
    let mut daily_total_dist_km: HashMap<String, f64> = HashMap::new();

    for d in &period_drives {
        fsd_engaged_ms += d.fsd_engaged_ms;
        total_dist_km += d.distance_km;
        total_dist_mi += d.distance_mi;
        fsd_dist_km += d.fsd_distance_km;
        fsd_dist_mi += d.fsd_distance_mi;
        disengagements += d.fsd_disengagements;
        accel_pushes += d.fsd_accel_pushes;
        autosteer_engaged_ms += d.autosteer_engaged_ms;
        autosteer_dist_km += d.autosteer_distance_km;
        autosteer_dist_mi += d.autosteer_distance_mi;
        tacc_engaged_ms += d.tacc_engaged_ms;
        tacc_dist_km += d.tacc_distance_km;
        tacc_dist_mi += d.tacc_distance_mi;

        if d.fsd_engaged_ms > 0 {
            fsd_sessions += 1;
        }

        if let Ok(dt) = NaiveDateTime::parse_from_str(&d.start_time, "%Y-%m-%dT%H:%M:%S") {
            let date_key = dt.format("%Y-%m-%d").to_string();
            let day_name = match dt.weekday() {
                chrono::Weekday::Mon => "Mon",
                chrono::Weekday::Tue => "Tue",
                chrono::Weekday::Wed => "Wed",
                chrono::Weekday::Thu => "Thu",
                chrono::Weekday::Fri => "Fri",
                chrono::Weekday::Sat => "Sat",
                chrono::Weekday::Sun => "Sun",
            };
            let ds = daily_map.entry(date_key.clone()).or_insert_with(|| FsdDayStats {
                date: date_key.clone(),
                day_name: day_name.to_string(),
                disengagements: 0,
                accel_pushes: 0,
                fsd_percent: 0.0,
                drives: 0,
                fsd_distance_km: 0.0,
                fsd_distance_mi: 0.0,
                total_duration_ms: 0,
                fsd_engaged_ms: 0,
            });
            ds.disengagements += d.fsd_disengagements;
            ds.accel_pushes += d.fsd_accel_pushes;
            ds.drives += 1;
            ds.fsd_distance_km += d.fsd_distance_km;
            ds.fsd_distance_mi += d.fsd_distance_mi;
            ds.total_duration_ms += d.duration_ms;
            ds.fsd_engaged_ms += d.fsd_engaged_ms;
            *daily_total_dist_km.entry(date_key).or_insert(0.0) += d.distance_km;
        }
    }

    // Compute daily FSD percent and find best day
    let mut best_day = String::new();
    let mut best_day_percent: f64 = 0.0;
    for (date_key, ds) in daily_map.iter_mut() {
        let total_km = daily_total_dist_km.get(date_key).copied().unwrap_or(0.0);
        if total_km > 0.0 {
            ds.fsd_percent = round1(ds.fsd_distance_km / total_km * 100.0);
        }
        ds.fsd_distance_km = round2(ds.fsd_distance_km);
        ds.fsd_distance_mi = round2(ds.fsd_distance_mi);
        if ds.fsd_percent > best_day_percent {
            best_day_percent = ds.fsd_percent;
            best_day = date_key.clone();
        }
    }

    // Sort daily stats by date
    let mut daily_stats: Vec<FsdDayStats> = daily_map.into_values().collect();
    daily_stats.sort_by(|a, b| a.date.cmp(&b.date));

    // Today's stats
    let today_key = today.format("%Y-%m-%d").to_string();
    let today_percent = daily_stats
        .iter()
        .find(|ds| ds.date == today_key)
        .map(|ds| ds.fsd_percent)
        .unwrap_or(0.0);

    let fsd_percent = if total_dist_km > 0.0 {
        round1(fsd_dist_km / total_dist_km * 100.0)
    } else {
        0.0
    };

    // FSD grade — same bands and labels as Sentry-Drive's fsdScoreLabel
    // so the two apps never disagree on the same score.
    let fsd_grade = if fsd_percent >= 90.0 {
        "Great"
    } else if fsd_percent >= 70.0 {
        "Good"
    } else if fsd_percent >= 40.0 {
        "Okay"
    } else {
        "Bad"
    };

    // Streak: consecutive days with FSD usage counting backwards from today
    let mut streak_days: i32 = 0;
    let mut check_date = today;
    loop {
        let key = check_date.format("%Y-%m-%d").to_string();
        if let Some(ds) = daily_stats.iter().find(|d| d.date == key) {
            if ds.fsd_engaged_ms > 0 {
                streak_days += 1;
                check_date -= chrono::Duration::days(1);
                continue;
            }
        }
        break;
    }

    // Format FSD engaged time
    let total_sec = fsd_engaged_ms / 1000;
    let hours = total_sec / 3600;
    let mins = (total_sec % 3600) / 60;
    let fsd_time_formatted = if hours > 0 {
        format!("{}h {}m", hours, mins)
    } else {
        format!("{}m", mins)
    };

    // Avg per drive — denominator is EVERY SEI drive in the period, not
    // just FSD sessions, matching Sentry-Drive ("avg disengagements per
    // drive", where a manual drive contributes a zero).
    let drive_count = period_drives.len();
    let avg_disengagements = if drive_count > 0 {
        round2(disengagements as f64 / drive_count as f64)
    } else {
        0.0
    };
    let avg_accel_pushes = if drive_count > 0 {
        round2(accel_pushes as f64 / drive_count as f64)
    } else {
        0.0
    };

    // Assisted totals
    let total_assisted_dist_km = fsd_dist_km + autosteer_dist_km + tacc_dist_km;
    let assisted_percent = if total_dist_km > 0.0 {
        round1(total_assisted_dist_km / total_dist_km * 100.0)
    } else {
        0.0
    };

    FsdAnalytics {
        period: period.to_string(),
        period_start: period_start_str,
        total_drives: period_drives.len() as i32,
        fsd_sessions,
        fsd_percent,
        today_percent,
        best_day,
        best_day_percent,
        fsd_engaged_ms,
        fsd_distance_km: round2(fsd_dist_km),
        fsd_distance_mi: round2(fsd_dist_mi),
        total_distance_km: round2(total_dist_km),
        total_distance_mi: round2(total_dist_mi),
        disengagements,
        accel_pushes,
        daily: daily_stats,
        fsd_grade: fsd_grade.to_string(),
        streak_days,
        fsd_time_formatted,
        avg_disengagements_per_drive: avg_disengagements,
        avg_accel_pushes_per_drive: avg_accel_pushes,
        autosteer_engaged_ms,
        autosteer_distance_km: round2(autosteer_dist_km),
        autosteer_distance_mi: round2(autosteer_dist_mi),
        tacc_engaged_ms,
        tacc_distance_km: round2(tacc_dist_km),
        tacc_distance_mi: round2(tacc_dist_mi),
        assisted_percent,
    }
}

// ---------------------------------------------------------------------------
// Tessie/SEI overlap filter
// ---------------------------------------------------------------------------

/// Filter out Tessie-imported drives whose `[start_time, end_time]` window
/// overlaps any native SEI drive. Tessie drives that fall in SEI gaps are
/// kept.
///
/// Without this filter, the same physical trip can appear twice in the
/// drive list — once as a high-fidelity SEI drive (date stored as the
/// raw TeslaCam directory name) and once as the Tessie fallback (date
/// stored as just `YYYY-MM-DD`). Hide policy is applied on read; the
/// underlying clip rows stay in the DB so the Tessie drive resurfaces
/// if the SEI drive is later removed.
///
/// Drive `id` values are NOT renumbered — callers that look up drives
/// by ID (e.g. `find_drive_files`) must continue to operate on the
/// un-hidden grouping so the IDs handed to the frontend stay valid.
pub fn hide_tessie_overlapping_sei(summaries: Vec<DriveSummary>) -> Vec<DriveSummary> {
    let before = summaries.len();
    // Build sorted list of [start, end] ranges from the SEI drives.
    let mut sei_ranges: Vec<(i64, i64)> = Vec::new();
    for d in &summaries {
        if is_tessie(&d.source) {
            continue;
        }
        let (Some(s), Some(e)) = (parse_iso_seconds(&d.start_time), parse_iso_seconds(&d.end_time))
        else {
            continue;
        };
        sei_ranges.push((s, e));
    }
    if sei_ranges.is_empty() {
        return summaries;
    }
    sei_ranges.sort_by_key(|r| r.0);

    let mut out = Vec::with_capacity(summaries.len());
    for d in summaries {
        if !is_tessie(&d.source) {
            out.push(d);
            continue;
        }
        let (Some(ts), Some(te)) = (parse_iso_seconds(&d.start_time), parse_iso_seconds(&d.end_time))
        else {
            // Unparseable timestamps — keep the drive rather than silently
            // hiding it (defensive).
            out.push(d);
            continue;
        };
        let mut hide = false;
        for &(rs, re) in &sei_ranges {
            if re <= ts {
                continue;
            }
            if rs >= te {
                break;
            }
            hide = true;
            break;
        }
        if !hide {
            out.push(d);
        }
    }
    let hidden = before.saturating_sub(out.len());
    if hidden > 0 {
        info!(
            "hide_tessie_overlapping_sei: hid {} Tessie drive(s) overlapping SEI windows (before={} after={})",
            hidden, before, out.len()
        );
    }
    out
}

fn parse_iso_seconds(s: &str) -> Option<i64> {
    NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S")
        .ok()
        .map(|dt| dt.and_utc().timestamp())
}

// ---------------------------------------------------------------------------
// Utility functions
// ---------------------------------------------------------------------------

/// Returns true when a source tag indicates a Tessie-imported drive.
/// Used by the Tessie-overlap hiding pass, which is specifically about
/// Tessie duplicating SEI windows.
fn is_tessie(source: &Option<String>) -> bool {
    source.as_deref() == Some("tessie")
}

/// Returns true for ANY imported source (tessie, teslascope, future
/// importers): a non-NULL source other than "sei". FSD analytics are
/// dashcam-only — imported autopilot data is inferred or absent, so
/// mixing it dilutes the score. Matches Sentry-Drive's isImportedSource.
fn is_imported(source: &Option<String>) -> bool {
    matches!(source.as_deref(), Some(s) if s != "sei")
}


/// Even-spaced downsampling. Returns at most `max_points` entries, always
/// including the last point.
fn downsample(points: &[GpsPoint], max_points: usize) -> Vec<GpsPoint> {
    if points.len() <= max_points {
        return points.to_vec();
    }
    let step = points.len() as f64 / max_points as f64;
    let mut result = Vec::with_capacity(max_points + 1);
    for i in 0..max_points {
        result.push(points[(i as f64 * step) as usize]);
    }
    result.push(*points.last().unwrap());
    result
}

/// Parse a timestamp from a Tesla dashcam filename.
/// Expected pattern: `YYYY-MM-DD_HH-MM-SS` anywhere in the path.
pub(crate) fn parse_file_timestamp(file_path: &str) -> Option<NaiveDateTime> {
    // Find the pattern YYYY-MM-DD_HH-MM-SS in the filename
    // We search for it with a simple scan rather than pulling in regex
    let bytes = file_path.as_bytes();
    if bytes.len() < 19 {
        return None;
    }

    for start in 0..=bytes.len() - 19 {
        // Check pattern: D D D D - D D - D D _ D D - D D - D D
        if bytes[start + 4] == b'-'
            && bytes[start + 7] == b'-'
            && bytes[start + 10] == b'_'
            && bytes[start + 13] == b'-'
            && bytes[start + 16] == b'-'
            && bytes[start..start + 4].iter().all(|b| b.is_ascii_digit())
            && bytes[start + 5..start + 7].iter().all(|b| b.is_ascii_digit())
            && bytes[start + 8..start + 10].iter().all(|b| b.is_ascii_digit())
            && bytes[start + 11..start + 13].iter().all(|b| b.is_ascii_digit())
            && bytes[start + 14..start + 16].iter().all(|b| b.is_ascii_digit())
            && bytes[start + 17..start + 19].iter().all(|b| b.is_ascii_digit())
        {
            let s = &file_path[start..start + 19];
            let iso = format!(
                "{}T{}:{}:{}",
                &s[..10],
                &s[11..13],
                &s[14..16],
                &s[17..19]
            );
            if let Ok(dt) = NaiveDateTime::parse_from_str(&iso, "%Y-%m-%dT%H:%M:%S") {
                return Some(dt);
            }
        }
    }
    None
}


/// Compute autopilot percent-of-distance values, rounded to 1 decimal.
fn compute_autopilot_percents(
    total_dist_m: f64,
    fsd_dist_m: f64,
    autosteer_dist_m: f64,
    tacc_dist_m: f64,
    assisted_dist_m: f64,
) -> (f64, f64, f64, f64) {
    if total_dist_m <= 0.0 {
        return (0.0, 0.0, 0.0, 0.0);
    }
    (
        round1(fsd_dist_m / total_dist_m * 100.0),
        round1(autosteer_dist_m / total_dist_m * 100.0),
        round1(tacc_dist_m / total_dist_m * 100.0),
        round1(assisted_dist_m / total_dist_m * 100.0),
    )
}

// round2 / round1 are re-exported from calc so every consumer in this
// module shares the single source of truth without churning ~30 call
// sites to fully-qualified paths.
use crate::calc::{round1, round2};

// ---------------------------------------------------------------------------
// Route::empty helper
// ---------------------------------------------------------------------------

impl Route {
    /// Create an empty Route (used for park boundary markers in clip splitting).
    fn empty() -> Self {
        Route::default()
    }
}

// ---------------------------------------------------------------------------
// Summary-based internals (no point data)
// ---------------------------------------------------------------------------

/// A `RouteSummary` tagged with its parsed filename timestamp, used as the
/// working item for the summary-side grouper. Borrows to avoid cloning
/// the gear_runs vec.
struct TimedSummary<'a> {
    summary: &'a RouteSummary,
    timestamp: NaiveDateTime,
    /// The parent clip's real wall-clock span (see `clip_spans`).
    clip_span_ms: i64,
}

/// Sub-segment of a clip, produced when a clip contains internal park
/// gaps that should split it across two or more drives.
///
/// A "whole-clip" sub-clip has `start_frame=0, end_frame=total_frames,
/// fraction=1.0`. Aggregator multiplies per-clip aggregates by `fraction`
/// so a clip split mid-way contributes proportionally to each drive
/// instead of dumping the full aggregate into whichever drive it
/// started in.
#[derive(Clone)]
struct SubClipSummary<'a> {
    summary: &'a RouteSummary,
    /// Timestamp of the START of this sub-segment. For whole-clip wraps
    /// this is the parent clip's parsed file timestamp; for mid-clip
    /// sub-segments it is offset by `start_frame * (60_000 ms / total_frames)`
    /// so two sub-drives derived from the same clip get distinct, ordered
    /// start times.
    timestamp: NaiveDateTime,
    /// Inclusive start frame index within the parent clip. 0 for whole clips.
    start_frame: u32,
    /// The parent clip's real wall-clock span in ms — a minute only when
    /// the recording ran that long (see `clip_spans`). The segment's own
    /// length is this scaled by its frame share.
    clip_span_ms: i64,
    /// Exclusive end frame index within the parent clip. Equal to
    /// `total_frames` for whole clips.
    end_frame: u32,
    /// Total frame count of the parent clip. 1 for clips without gear data
    /// (so fraction stays 1.0 in the degenerate case).
    total_frames: u32,
    /// `(end_frame - start_frame) / total_frames`. Aggregator multiplies
    /// time-attributable per-clip fields by this.
    fraction: f64,
}

impl<'a> SubClipSummary<'a> {
    /// Wrap a whole TimedSummary as a single sub-clip covering its full
    /// length. Used when the input has no gear_runs and we fall back to
    /// per-clip semantics.
    fn whole(ts: TimedSummary<'a>) -> Self {
        let total_frames = if ts.summary.gear_runs.is_empty() {
            1
        } else {
            ts.summary.gear_runs.iter().map(|r| r.frames).sum::<u32>().max(1)
        };
        SubClipSummary {
            summary: ts.summary,
            timestamp: ts.timestamp,
            clip_span_ms: ts.clip_span_ms,
            start_frame: 0,
            end_frame: total_frames,
            total_frames,
            fraction: 1.0,
        }
    }
}

/// Dedup by normalised path, parse timestamps, sort, split on 5-minute
/// gaps, and split within clips at long Park periods. Mirrors
/// `group_clips` but operates on summary rows that don't carry point
/// arrays.
///
/// Returns `Vec<Vec<SubClipSummary>>`: `split_summary_by_gear_state`
/// slices clips with internal park gaps into sub-segments so multi-
/// park-gap clips produce the correct drive count and per-drive
/// aggregates fraction-scale across the resulting drives.
fn group_summary_clips<'a>(summaries: &'a [RouteSummary]) -> Vec<Vec<SubClipSummary<'a>>> {
    if summaries.is_empty() {
        return Vec::new();
    }

    // Event-folder rows are partitioned off like `group_clips` does — the
    // processor only ingests them as gap-fills, but a drive-data.json
    // import from an unfixed build can carry arbitrary event rows, so the
    // same hole-gated admission applies here.
    let mut seen = HashMap::with_capacity(summaries.len());
    let mut unique: Vec<&RouteSummary> = Vec::with_capacity(summaries.len());
    let mut event_candidates: Vec<&RouteSummary> = Vec::new();
    for s in summaries {
        let norm = s.file.replace('\\', "/");
        if seen.insert(norm, ()).is_some() {
            continue;
        }
        if is_event_folder_path(&s.file) {
            event_candidates.push(s);
        } else {
            unique.push(s);
        }
    }

    let mut timed: Vec<TimedSummary> = unique
        .into_iter()
        .filter_map(|s| {
            let ts = parse_clip_timestamp(&s.file)?;
            Some(TimedSummary {
                summary: s,
                timestamp: ts,
                clip_span_ms: CLIP_DURATION_MS,
            })
        })
        .collect();
    // Same event-only guard as group_clips: unanchored driving clusters
    // can still be admitted below even when no non-event route exists.
    if timed.is_empty() && event_candidates.is_empty() {
        return Vec::new();
    }
    timed.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

    // Gap-fill admission — see group_clips for the rationale. Clip
    // timestamps come from the filename component (the event-folder name
    // also matches the timestamp pattern and would win a full-path scan).
    if !event_candidates.is_empty() {
        let recent_ts: Vec<NaiveDateTime> = timed.iter().map(|t| t.timestamp).collect();
        let cands: Vec<(NaiveDateTime, &RouteSummary)> = event_candidates
            .into_iter()
            .filter_map(|s| parse_clip_timestamp(&s.file).map(|ts| (ts, s)))
            .collect();
        let keys: Vec<GapFillCandidate> = cands
            .iter()
            .map(|(ts, s)| GapFillCandidate {
                ts: *ts,
                file: s.file.as_str(),
                driving: Some(summary_has_driving(s)),
                gear_driving: telemetry_gear_driving(&s.gear_runs, &[]),
            })
            .collect();
        let admitted = select_gap_fill(&recent_ts, &keys);
        if !admitted.is_empty() {
            for i in admitted {
                let (ts, s) = cands[i];
                timed.push(TimedSummary {
                    summary: s,
                    timestamp: ts,
                    clip_span_ms: CLIP_DURATION_MS,
                });
            }
            timed.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));
        }
    }

    if timed.is_empty() {
        return Vec::new();
    }

    // Real clip spans over the whole sorted series, so the drive's end
    // time reflects a recording that stopped early. Mirrors Drive's
    // annotateClipSpans; the START offsets below stay nominal so drive
    // tag keys never move.
    {
        let entries: Vec<(NaiveDateTime, bool)> = timed
            .iter()
            .map(|t| {
                (
                    t.timestamp,
                    bounds_clip_span(&t.summary.source, &t.summary.file),
                )
            })
            .collect();
        for (t, span) in timed.iter_mut().zip(clip_spans(&entries)) {
            t.clip_span_ms = span;
        }
    }

    // Time-gap split.
    let mut time_groups: Vec<Vec<TimedSummary>> = Vec::new();
    let mut current = vec![timed.remove(0)];
    for tr in timed {
        let gap_ms = (tr.timestamp - current.last().unwrap().timestamp).num_milliseconds();
        if gap_ms > DRIVE_GAP_MS {
            time_groups.push(std::mem::take(&mut current));
        }
        current.push(tr);
    }
    if !current.is_empty() {
        time_groups.push(current);
    }

    // Gear-state split (produces sub-clips), then external-signature
    // split (operates on sub-clips).
    let mut groups = Vec::new();
    for tg in time_groups {
        for gear_group in split_summary_by_gear_state(tg) {
            for sig_group in split_summary_by_external_signature(gear_group) {
                groups.push(sig_group);
            }
        }
    }
    groups
}

/// Summary-side equivalent of `split_by_external_signature`. Operates
/// on sub-clips (post gear-state split) so signature buckets preserve
/// any mid-clip park-gap slicing.
fn split_summary_by_external_signature<'a>(
    group: Vec<SubClipSummary<'a>>,
) -> Vec<Vec<SubClipSummary<'a>>> {
    if group.len() <= 1 {
        return vec![group];
    }
    let has_any = group.iter().any(|c| c.summary.external_signature.is_some());
    if !has_any {
        return vec![group];
    }

    let mut buckets: std::collections::HashMap<&str, Vec<SubClipSummary<'a>>> =
        std::collections::HashMap::new();
    let mut no_sig: Vec<SubClipSummary<'a>> = Vec::new();

    for clip in group {
        match &clip.summary.external_signature {
            Some(sig) => buckets.entry(sig.as_str()).or_default().push(clip),
            None => no_sig.push(clip),
        }
    }

    let mut result = Vec::new();
    if !no_sig.is_empty() {
        result.push(no_sig);
    }
    for bucket in buckets.into_values() {
        result.push(bucket);
    }
    result
}

/// Summary-side equivalent of `split_by_gear_state`. Slices clips with
/// internal Park gaps into sub-segments — including the multi-park-gap
/// case where one clip contributes to 3+ drives.
///
/// Each produced sub-clip carries `(start_frame, end_frame, fraction)`
/// so `build_summary_from_aggregates` can fraction-scale per-clip
/// aggregates instead of dumping the whole clip's totals into one drive.
fn split_summary_by_gear_state<'a>(
    group: Vec<TimedSummary<'a>>,
) -> Vec<Vec<SubClipSummary<'a>>> {
    if group.is_empty() {
        return Vec::new();
    }

    let has_gear_runs = group.iter().any(|c| !c.summary.gear_runs.is_empty());
    if !has_gear_runs {
        return split_summary_by_gear_state_legacy(group);
    }

    let mut result: Vec<Vec<SubClipSummary<'a>>> = Vec::new();
    let mut current: Vec<SubClipSummary<'a>> = Vec::new();

    for clip in group {
        let total_frames: u32 = clip.summary.gear_runs.iter().map(|r| r.frames).sum();

        // No gear data: treat whole clip as one non-park sub-segment.
        if total_frames == 0 {
            current.push(SubClipSummary::whole(clip));
            continue;
        }

        let spf = 60.0 / total_frames as f64;

        // Raw per-gear-run segments, marked parked iff GEAR_PARK and the
        // run lasts at least PARK_GAP_SECONDS.
        #[derive(Clone)]
        struct Seg {
            start: u32,
            end: u32,
            parked: bool,
        }
        let mut raw_segs: Vec<Seg> = Vec::with_capacity(clip.summary.gear_runs.len());
        let mut offset: u32 = 0;
        for run in &clip.summary.gear_runs {
            let parked = run.gear == GEAR_PARK
                && (run.frames as f64 * spf) >= PARK_GAP_SECONDS;
            raw_segs.push(Seg {
                start: offset,
                end: offset + run.frames,
                parked,
            });
            offset += run.frames;
        }

        // Merge consecutive non-parked segments (a non-park run that
        // changes gear shouldn't split the drive).
        let mut merged: Vec<Seg> = Vec::new();
        for seg in raw_segs {
            match merged.last_mut() {
                Some(last) if !last.parked && !seg.parked => last.end = seg.end,
                _ => merged.push(seg),
            }
        }

        // Whole clip parked → boundary, no sub-clip emitted on either side.
        if merged.iter().all(|s| s.parked) {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
            continue;
        }

        // No internal park gap → whole clip stays in current sub-drive.
        if !merged.iter().any(|s| s.parked) {
            current.push(SubClipSummary {
                summary: clip.summary,
                timestamp: clip.timestamp,
                clip_span_ms: clip.clip_span_ms,
                start_frame: 0,
                end_frame: total_frames,
                total_frames,
                fraction: 1.0,
            });
            continue;
        }

        // Mixed: emit sub-clip per non-park segment, close current
        // drive at each park boundary. The sub-clip's timestamp is
        // offset to the segment's start frame so two sub-drives derived
        // from one clip get distinct, ordered start times.
        for seg in merged {
            if seg.parked {
                if !current.is_empty() {
                    result.push(std::mem::take(&mut current));
                }
            } else {
                let seg_offset_ms = (seg.start as f64 * spf * 1000.0).round() as i64;
                current.push(SubClipSummary {
                    summary: clip.summary,
                    timestamp: clip.timestamp
                        + chrono::Duration::milliseconds(seg_offset_ms),
                    clip_span_ms: clip.clip_span_ms,
                    start_frame: seg.start,
                    end_frame: seg.end,
                    total_frames,
                    fraction: (seg.end - seg.start) as f64 / total_frames as f64,
                });
            }
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        // All clips were parked — return nothing so drives_count stays 0.
        return Vec::new();
    }
    result
}

/// Legacy fallback for clip groups without `gear_runs` data (v1
/// summaries, or full routes pre-Phase-1). Each surviving clip becomes
/// a whole-clip sub-clip with fraction=1.0.
fn split_summary_by_gear_state_legacy<'a>(
    group: Vec<TimedSummary<'a>>,
) -> Vec<Vec<SubClipSummary<'a>>> {
    if group.len() <= 1 {
        return vec![group.into_iter().map(SubClipSummary::whole).collect()];
    }
    let mut result: Vec<Vec<SubClipSummary<'a>>> = Vec::new();
    let mut current: Vec<SubClipSummary<'a>> = Vec::new();
    for clip in group {
        let mostly_park = if clip.summary.raw_frame_count > 0 {
            (clip.summary.raw_park_count as f64 / clip.summary.raw_frame_count as f64)
                > calc::PARK_MAJORITY_FRACTION
        } else {
            false
        };
        if mostly_park {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else {
            current.push(SubClipSummary::whole(clip));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

/// Compute drive distance from summary clips using:
/// 1) per-clip aggregate distance scaled by sub-clip fraction, plus
/// 2) boundary gaps between consecutive clips (prev end -> next start).
///
/// Fraction-scaling lets a clip split mid-way (internal park gap)
/// contribute proportionally to two drives instead of dumping the full
/// distance into one. The gap term matches Sentry-Drive's merged-point
/// walk behavior and is especially important for sparse Tessie clips.
/// Output of [`distance_from_summary_clips`]: the drive's total distance
/// plus how much of the inter-clip bridge distance belongs to each
/// autopilot mode (attributed by the incoming clip's `ap_at_start`,
/// matching Sentry-Drive's merged walk where the crossing segment lands
/// on the next point's state).
#[derive(Default)]
struct SummaryDistances {
    total_m: f64,
    fsd_bridge_m: f64,
    autosteer_bridge_m: f64,
    tacc_bridge_m: f64,
}

fn distance_from_summary_clips(clips: &[SubClipSummary]) -> SummaryDistances {
    fn is_null_island_pair(lat: f64, lng: f64) -> bool {
        lat.abs() < 1.0 && lng.abs() < 1.0
    }

    let mut out = SummaryDistances::default();
    let mut prev_end: Option<(f64, f64)> = None;

    for clip in clips {
        let a = &clip.summary.aggregates;
        out.total_m += a.distance_m * clip.fraction;

        if let (Some((prev_lat, prev_lng)), Some(cur_lat), Some(cur_lng)) =
            (prev_end, a.start_lat, a.start_lng)
        {
            if !is_null_island_pair(prev_lat, prev_lng) && !is_null_island_pair(cur_lat, cur_lng)
            {
                let gap_m = calc::geodesic_m(prev_lat, prev_lng, cur_lat, cur_lng);
                out.total_m += gap_m;
                match a.ap_at_start {
                    Some(m) if m == AUTOPILOT_FSD as i32 => out.fsd_bridge_m += gap_m,
                    Some(m) if m == AUTOPILOT_AUTOSTEER as i32 => {
                        out.autosteer_bridge_m += gap_m
                    }
                    Some(m) if m == AUTOPILOT_TACC as i32 => out.tacc_bridge_m += gap_m,
                    _ => {}
                }
            }
        }

        prev_end = if let (Some(lat), Some(lng)) = (a.end_lat, a.end_lng) {
            Some((lat, lng))
        } else if let (Some(lat), Some(lng)) = (a.start_lat, a.start_lng) {
            Some((lat, lng))
        } else {
            prev_end
        };
    }

    out
}

/// Build a single `DriveSummary` from sub-clips. Per-clip time-
/// attributable aggregates (distance, durations, engaged-ms, sample
/// counts) are multiplied by each sub-clip's `fraction` so a parent
/// clip split mid-way by an internal park gap contributes
/// proportionally to two drives. `max_speed_mps` is not scaled — peak
/// is peak. `fsd_disengagements` and `fsd_accel_pushes` are counted
/// once per parent file (attributed to the first sub-clip of that
/// file in this drive) to avoid double-counting in the rare case a
/// parent contributes multiple sub-clips here.
fn safety_prefix_for_grace(
    prefix: &crate::safety::SafetyGracePrefix,
    grace_ms: i64,
) -> crate::safety::SafetyPrefixBucket {
    let mut out = crate::safety::SafetyPrefixBucket::default();
    let mut remaining = grace_ms.clamp(0, 5_000);
    for bucket in prefix {
        if remaining <= 0 {
            break;
        }
        let fraction = (remaining.min(1_000) as f64 / 1_000.0).clamp(0.0, 1.0);
        out.hard_brake_ms += (bucket.hard_brake_ms as f64 * fraction).round() as i64;
        out.brake_any_ms += (bucket.brake_any_ms as f64 * fraction).round() as i64;
        out.aggr_turn_ms += (bucket.aggr_turn_ms as f64 * fraction).round() as i64;
        out.turn_any_ms += (bucket.turn_any_ms as f64 * fraction).round() as i64;
        out.speeding_ms += (bucket.speeding_ms as f64 * fraction).round() as i64;
        remaining -= 1_000;
    }
    out
}

fn build_summary_from_aggregates(
    clips: &[SubClipSummary],
    idx: usize,
    tags: &HashMap<String, Vec<String>>,
) -> DriveSummary {
    let first_clip = &clips[0];
    let last_clip = &clips[clips.len() - 1];
    // Sub-clip-aware start/end times: a mid-clip sub-segment carries an
    // offset timestamp; the drive's end_time also respects the last
    // sub-segment's end_frame rather than always adding a full minute.
    let start_time = first_clip.timestamp;
    // The last segment's length comes from the parent clip's REAL span,
    // so a drive ending on a recording that stopped early does not claim
    // the rest of a nominal minute. The start offsets above stay nominal
    // on purpose — start_time is the drive-tag key.
    let last_spf_ms = if last_clip.total_frames > 0 {
        last_clip.clip_span_ms as f64 / last_clip.total_frames as f64
    } else {
        0.0
    };
    let last_segment_len_ms = ((last_clip.end_frame - last_clip.start_frame) as f64
        * last_spf_ms)
        .round() as i64;
    let end_time = last_clip.timestamp + chrono::Duration::milliseconds(last_segment_len_ms);
    let duration_ms = (end_time - start_time).num_milliseconds();

    let dists = distance_from_summary_clips(clips);
    let total_dist_m: f64 = dists.total_m;
    let mut max_speed_mps: f64 = 0.0;
    let mut speed_sum: f64 = 0.0;
    let mut speed_count: f64 = 0.0;
    let mut point_count: f64 = 0.0;
    let mut fsd_engaged_ms: f64 = 0.0;
    let mut autosteer_engaged_ms: f64 = 0.0;
    let mut tacc_engaged_ms: f64 = 0.0;
    let mut fsd_dist_m: f64 = 0.0;
    let mut autosteer_dist_m: f64 = 0.0;
    let mut tacc_dist_m: f64 = 0.0;
    let mut assisted_dist_m: f64 = 0.0;
    let mut fsd_disengagements: i32 = 0;
    let mut fsd_accel_pushes: i32 = 0;

    // v18 Safety Score accumulators (see safety.rs). Time quantities
    // scale by sub-clip fraction like the FSD ms totals; event counts
    // are per-parent-file like disengagements. Night exposure is
    // classified by each sub-clip's local start hour — 60 s granularity
    // is plenty for a 6-hour window.
    let mut safety_hard_brake_ms: f64 = 0.0;
    let mut safety_hard_brake_events: i32 = 0;
    let mut safety_aggr_turn_ms: f64 = 0.0;
    let mut safety_aggr_turn_events: i32 = 0;
    let mut safety_speeding_ms: f64 = 0.0;
    let mut safety_moving_ms: f64 = 0.0;
    let mut safety_imu_moving_ms: f64 = 0.0;
    let mut safety_manual_moving_ms: f64 = 0.0;
    let mut safety_brake_any_ms: f64 = 0.0;
    let mut safety_turn_any_ms: f64 = 0.0;
    let mut safety_night_ms: f64 = 0.0;
    let mut safety_night_weighted_ms: f64 = 0.0;
    let mut safety_night_m: f64 = 0.0;
    let mut safety_night_weighted_m: f64 = 0.0;

    let mut start_point: Option<GpsPoint> = None;
    let mut end_point: Option<GpsPoint> = None;

    // Dedupe parent files so non-time-attributable counts (disengagements,
    // accel pushes, max-speed) are taken from each parent at most once.
    let mut seen_files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut unique_clip_count: usize = 0;

    // v15 clip-seam state, mirroring the cloud summary v4 aggregator
    // (SentryCloud grouper.js): a pending disengagement open at one
    // clip's end is decided by the NEXT clip's first 2s — Park inside
    // the remaining grace window means FSD parked the car (no count),
    // anything else is a driver disengagement. Carried only across
    // whole-clip boundaries: a mid-clip park split means the pending
    // already resolved as Park.
    let mut pend_prev_ms: Option<f64> = None;
    // Whether the previous clip ended FSD-engaged across a whole-clip
    // boundary — gates `fsd_accel_pushes_early` (pushes inside a
    // continuation clip's start-anchored grace window are real).
    let mut prev_fsd_at_end = false;
    // Wall-clock end of the previous sub-clip when it ended at a whole-
    // clip boundary: the seam between it and the next clip is real drive
    // time that no per-clip aggregate covers. Sentry-Drive's merged walk
    // attributes that dt to the next point's autopilot state; we do the
    // same via the incoming clip's `ap_at_start`.
    let mut prev_end_ts: Option<chrono::NaiveDateTime> = None;
    let mut safety_grace_remaining_ms: i64 = 0;
    let mut prev_ap_at_end: Option<i32> = None;

    for clip in clips {
        let a = &clip.summary.aggregates;
        let f = clip.fraction;

        // Time-attributable aggregates scale by sub-clip fraction.
        speed_sum += a.avg_speed_mps * a.speed_sample_count as f64 * f;
        speed_count += a.speed_sample_count as f64 * f;
        point_count += a.valid_point_count as f64 * f;
        fsd_engaged_ms += a.fsd_engaged_ms as f64 * f;
        autosteer_engaged_ms += a.autosteer_engaged_ms as f64 * f;
        tacc_engaged_ms += a.tacc_engaged_ms as f64 * f;
        fsd_dist_m += a.fsd_distance_m * f;
        autosteer_dist_m += a.autosteer_distance_m * f;
        tacc_dist_m += a.tacc_distance_m * f;
        assisted_dist_m += a.assisted_distance_m * f;

        // If assistance disengaged at a clip seam, exclude factor time
        // from the next five seconds exactly as we do for an in-clip
        // transition. Prefix buckets let this stay BLOB-free.
        let mut prefix_excluded = crate::safety::SafetyPrefixBucket::default();
        if clip.start_frame == 0 {
            let seam_is_contiguous = prev_end_ts.is_none_or(|end| {
                (clip.timestamp - end).num_milliseconds().unsigned_abs() <= 5_000
            });
            if !seam_is_contiguous {
                safety_grace_remaining_ms = 0;
                prev_ap_at_end = None;
            }
            if prev_ap_at_end.is_some_and(|ap| ap != AUTOPILOT_OFF as i32)
                && a.ap_at_start == Some(AUTOPILOT_OFF as i32)
            {
                safety_grace_remaining_ms = 5_000;
            }
            if safety_grace_remaining_ms > 0 {
                if let Some(prefix) = a.safety_grace_prefix.as_ref() {
                    prefix_excluded = safety_prefix_for_grace(prefix, safety_grace_remaining_ms);
                }
            }
        }

        // Safety time totals (NULL columns = pre-v18 row = contribute 0).
        safety_hard_brake_ms +=
            (a.safety_hard_brake_ms.unwrap_or(0) - prefix_excluded.hard_brake_ms).max(0) as f64 * f;
        safety_aggr_turn_ms +=
            (a.safety_aggr_turn_ms.unwrap_or(0) - prefix_excluded.aggr_turn_ms).max(0) as f64 * f;
        safety_speeding_ms +=
            (a.safety_speeding_ms.unwrap_or(0) - prefix_excluded.speeding_ms).max(0) as f64 * f;
        safety_moving_ms += a.safety_moving_ms.unwrap_or(0) as f64 * f;
        safety_imu_moving_ms += a.safety_imu_moving_ms.unwrap_or(0) as f64 * f;
        safety_manual_moving_ms += a.safety_manual_moving_ms.unwrap_or(0) as f64 * f;
        safety_brake_any_ms +=
            (a.safety_brake_any_ms.unwrap_or(0) - prefix_excluded.brake_any_ms).max(0) as f64 * f;
        safety_turn_any_ms +=
            (a.safety_turn_any_ms.unwrap_or(0) - prefix_excluded.turn_any_ms).max(0) as f64 * f;
        safety_night_ms += a.safety_night_ms.unwrap_or(0) as f64 * f;
        safety_night_weighted_ms += a.safety_night_weighted_ms.unwrap_or(0) as f64 * f;
        if crate::safety::is_night_hour(clip.timestamp.hour()) {
            safety_night_m += a.distance_m * f;
            safety_night_weighted_m +=
                a.distance_m * f * crate::safety::night_weight(clip.timestamp.hour());
        }

        // Per-file (not per-sub-clip) aggregates.
        let is_first_subclip_of_file = seen_files.insert(clip.summary.file.as_str());
        if is_first_subclip_of_file {
            unique_clip_count += 1;
            if a.max_speed_mps > max_speed_mps {
                max_speed_mps = a.max_speed_mps;
            }
            fsd_disengagements += a.fsd_disengagements;
            fsd_accel_pushes += a.fsd_accel_pushes;
            safety_hard_brake_events += a.safety_hard_brake_events.unwrap_or(0) as i32;
            safety_aggr_turn_events += a.safety_aggr_turn_events.unwrap_or(0) as i32;
            if clip.start_frame == 0 {
                if let Some(p) = pend_prev_ms {
                    let rem = 2000.0 - p;
                    let parked = a.park_ms_start.is_some_and(|pk| pk <= rem);
                    if !parked {
                        fsd_disengagements += 1;
                    }
                }
                if prev_fsd_at_end {
                    fsd_accel_pushes += a.fsd_accel_pushes_early;
                }
                // Seam wall-time between the previous clip's end and this
                // clip's start, attributed to this clip's starting mode.
                if let Some(pe) = prev_end_ts {
                    let seam_ms = (clip.timestamp - pe).num_milliseconds().max(0) as f64;
                    match a.ap_at_start {
                        Some(m) if m == AUTOPILOT_FSD as i32 => fsd_engaged_ms += seam_ms,
                        Some(m) if m == AUTOPILOT_AUTOSTEER as i32 => {
                            autosteer_engaged_ms += seam_ms
                        }
                        Some(m) if m == AUTOPILOT_TACC as i32 => tacc_engaged_ms += seam_ms,
                        _ => {}
                    }
                }
            }
        }
        let whole_clip_end = clip.end_frame == clip.total_frames;
        pend_prev_ms = if whole_clip_end { a.fsd_pend_ms_end } else { None };
        prev_fsd_at_end = whole_clip_end && a.fsd_at_end;
        prev_end_ts = if whole_clip_end {
            let sub_len_ms = if clip.total_frames > 0 {
                ((clip.end_frame - clip.start_frame) as f64 * 60_000.0
                    / clip.total_frames as f64)
                    .round() as i64
            } else {
                60_000
            };
            Some(clip.timestamp + chrono::Duration::milliseconds(sub_len_ms))
        } else {
            None
        };
        if whole_clip_end {
            safety_grace_remaining_ms = a.safety_grace_ms_end.unwrap_or(0).clamp(0, 5_000);
            prev_ap_at_end = a.ap_at_end;
        } else {
            safety_grace_remaining_ms = 0;
            prev_ap_at_end = None;
        }

        if start_point.is_none() {
            if let (Some(lat), Some(lng)) = (a.start_lat, a.start_lng) {
                start_point = Some([lat, lng]);
            }
        }
        if let (Some(lat), Some(lng)) = (a.end_lat, a.end_lng) {
            end_point = Some([lat, lng]);
        }
    }

    // Pending still open after the drive's last clip: recording stopped
    // before any Park arrived, so it was a real driver disengagement —
    // matches Sentry-Drive's end-of-drive flush.
    if pend_prev_ms.is_some() {
        fsd_disengagements += 1;
    }

    // Inter-clip bridge distance per autopilot mode (already inside
    // total_dist_m): keeps per-mode percentages consistent with the
    // total they're divided by, and with Sentry-Drive's merged walk.
    fsd_dist_m += dists.fsd_bridge_m;
    autosteer_dist_m += dists.autosteer_bridge_m;
    tacc_dist_m += dists.tacc_bridge_m;
    assisted_dist_m += dists.fsd_bridge_m + dists.autosteer_bridge_m + dists.tacc_bridge_m;

    let avg_speed_mps = if speed_count > 0.0 {
        speed_sum / speed_count
    } else {
        0.0
    };
    // ── v6 BLE telemetry rollup across this drive's unique clips ──
    let telemetry = roll_up_telemetry(clips);

    // ── Summon detection ──
    // Mirrors Sentry-Drive buildDriveStats: evidence is each clip's
    // flag_runs plus its park-split segment bounds — raw SEI frame
    // space, immune to GPS dedup. Clips without flag_runs (pre-flags
    // extractions, imports) make the drive unverifiable and
    // detect_summon returns false. The speed gate is frame-accurate
    // inside detect_summon via per-run `max_mps`; these clip-level
    // values only feed its legacy fallback (rows extracted before
    // per-run maxima existed). There the v16 sei_speed_abs_max column is
    // used because the locked max_speed_mps drops negative (Reverse)
    // SEI samples — a reverse-only summon would read 0 — and can't
    // distinguish GPS-derived speed, which jitters past walking pace on
    // a car that barely moved. A whole-clip abs max over-reports a
    // shared clip's summon segment — conservative: the drive stays
    // unflagged until reprocessed.
    let mut has_sei_speeds = false;
    let mut summon_max_speed: f64 = 0.0;
    for clip in clips {
        if let Some(m) = clip.summary.aggregates.sei_speed_abs_max {
            if m > 0.0 {
                has_sei_speeds = true;
            }
            if m > summon_max_speed {
                summon_max_speed = m;
            }
        }
    }

    let summon_evidence: Vec<SummonClipEvidence> = clips
        .iter()
        .map(|c| {
            if c.total_frames > 1 {
                SummonClipEvidence {
                    flag_runs: &c.summary.flag_runs,
                    ap_runs: &c.summary.ap_runs,
                    gear_runs: &c.summary.gear_runs,
                    start_frame: c.start_frame,
                    end_frame: c.end_frame,
                    total_frames: c.total_frames,
                }
            } else {
                // Whole-clip wrap of a row without gear data carries the
                // total_frames=1 sentinel; fall back to raw_frame_count,
                // then to the flag-run sum (Sentry-Drive's evidence
                // fallback order).
                let mut total = c.summary.raw_frame_count;
                if total == 0 {
                    total = c.summary.flag_runs.iter().map(|r| r.frames).sum();
                }
                SummonClipEvidence {
                    flag_runs: &c.summary.flag_runs,
                    // Wired here too, not just on the split branch: a
                    // whole-clip drive is exactly where the Self Driving
                    // signature applies most often.
                    ap_runs: &c.summary.ap_runs,
                    gear_runs: &c.summary.gear_runs,
                    start_frame: 0,
                    end_frame: total,
                    total_frames: total,
                }
            }
        })
        .collect();
    let summon = detect_summon(
        &summon_evidence,
        if has_sei_speeds { summon_max_speed } else { 0.0 },
        duration_ms,
        has_sei_speeds,
    );

    // A Summon drive is driverless, so assistance analytics do not apply
    // to it. Hardware that reports Summon as Self Driving would
    // otherwise book the maneuver as FSD engagement and mark a phantom
    // disengagement where the car parked itself. Zeroing here — BEFORE
    // the percentages are derived from these distances — keeps every
    // consumer honest without checking the summon flag. Mirrors
    // Sentry-Drive's buildDriveStats; the per-point autopilot states
    // stay untouched as raw evidence, so a summon route ships
    // FSD-valued per-point states alongside a 0 % FSD aggregate. The
    // aggregates are authoritative; the per-point array is evidence.
    if summon {
        fsd_engaged_ms = 0.0;
        fsd_disengagements = 0;
        fsd_accel_pushes = 0;
        fsd_dist_m = 0.0;
        autosteer_engaged_ms = 0.0;
        autosteer_dist_m = 0.0;
        tacc_engaged_ms = 0.0;
        tacc_dist_m = 0.0;
        assisted_dist_m = 0.0;
    }

    let (fsd_percent, autosteer_percent, tacc_percent, assisted_percent) =
        compute_autopilot_percents(
            total_dist_m,
            fsd_dist_m,
            autosteer_dist_m,
            tacc_dist_m,
            assisted_dist_m,
        );

    let start_time_str = start_time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let drive_tags = tags.get(&start_time_str).cloned().unwrap_or_default();

    // ── Safety Score for this drive ──
    // Imported and summon drives carry no (or meaningless) safety data;
    // everything else scores through the shared formula, which itself
    // returns None below the minimum-mileage/-time floors.
    let source_is_sei = first_clip
        .summary
        .source
        .as_deref()
        .map_or(true, |s| s.is_empty() || s == "sei");
    let safety_totals = crate::safety::SafetyTotals {
        distance_mi: total_dist_m / calc::M_PER_MILE,
        assisted_mi: assisted_dist_m / calc::M_PER_MILE,
        night_mi: safety_night_m / calc::M_PER_MILE,
        night_weighted_mi: safety_night_weighted_m / calc::M_PER_MILE,
        moving_ms: safety_moving_ms.round() as i64,
        imu_moving_ms: safety_imu_moving_ms.round() as i64,
        night_ms: safety_night_ms.round() as i64,
        night_weighted_ms: safety_night_weighted_ms.round() as i64,
        manual_moving_ms: safety_manual_moving_ms.round() as i64,
        hard_brake_ms: safety_hard_brake_ms.round() as i64,
        hard_brake_events: safety_hard_brake_events,
        aggr_turn_ms: safety_aggr_turn_ms.round() as i64,
        aggr_turn_events: safety_aggr_turn_events,
        speeding_ms: safety_speeding_ms.round() as i64,
        brake_any_ms: safety_brake_any_ms.round() as i64,
        turn_any_ms: safety_turn_any_ms.round() as i64,
    };
    let safety_score = if source_is_sei && !summon {
        crate::safety::compute_safety_score(&safety_totals).map(|s| s.score)
    } else {
        None
    };

    DriveSummary {
        id: idx as i32,
        // See `build_summary` above — derive from start_time so the web
        // UI's `new Date(date + "T00:00:00")` parses cleanly.
        date: start_time.format("%Y-%m-%d").to_string(),
        start_time: start_time_str,
        end_time: end_time.format("%Y-%m-%dT%H:%M:%S").to_string(),
        duration_ms,
        distance_mi: round2(total_dist_m / calc::M_PER_MILE),
        distance_km: round2(total_dist_m / 1000.0),
        avg_speed_mph: round2(avg_speed_mps * calc::MPS_TO_MPH),
        max_speed_mph: round2(max_speed_mps * calc::MPS_TO_MPH),
        avg_speed_kmh: round2(avg_speed_mps * 3.6),
        max_speed_kmh: round2(max_speed_mps * 3.6),
        clip_count: unique_clip_count,
        point_count: point_count.round() as usize,
        start_point,
        end_point,
        tags: drive_tags,
        fsd_engaged_ms: fsd_engaged_ms.round() as i64,
        fsd_disengagements,
        fsd_accel_pushes,
        fsd_percent,
        fsd_distance_km: round2(fsd_dist_m / 1000.0),
        fsd_distance_mi: round2(fsd_dist_m / calc::M_PER_MILE),
        autosteer_engaged_ms: autosteer_engaged_ms.round() as i64,
        autosteer_percent,
        autosteer_distance_km: round2(autosteer_dist_m / 1000.0),
        autosteer_distance_mi: round2(autosteer_dist_m / calc::M_PER_MILE),
        tacc_engaged_ms: tacc_engaged_ms.round() as i64,
        tacc_percent,
        tacc_distance_km: round2(tacc_dist_m / 1000.0),
        tacc_distance_mi: round2(tacc_dist_m / calc::M_PER_MILE),
        assisted_percent,
        summon,
        battery_pct_start: telemetry.battery_pct_start,
        battery_pct_end: telemetry.battery_pct_end,
        battery_pct_used: telemetry.battery_pct_used,
        interior_temp_min_c: telemetry.interior_temp_min_c,
        interior_temp_max_c: telemetry.interior_temp_max_c,
        exterior_temp_avg_c: telemetry.exterior_temp_avg_c,
        hvac_runtime_s: telemetry.hvac_runtime_s,
        tire_fl_psi: telemetry.tire_fl_psi,
        tire_fr_psi: telemetry.tire_fr_psi,
        tire_rl_psi: telemetry.tire_rl_psi,
        tire_rr_psi: telemetry.tire_rr_psi,
        odometer_mi_start: telemetry.odometer_mi_start,
        odometer_mi_end: telemetry.odometer_mi_end,
        odometer_mi_driven: telemetry.odometer_mi_driven,
        // start/end_location populated from the rolled-up Tesla
        // address strings (first/last clip in the drive with a
        // non-null `location_name` BLE sample). Whatever Tesla's
        // own reverse-geocoder returned — no post-processing.
        start_location: telemetry.location_name_start,
        end_location: telemetry.location_name_end,
        // Match Go: null/empty source becomes "sei".
        source: Some(
            first_clip
                .summary
                .source
                .clone()
                .unwrap_or_else(|| "sei".to_string()),
        ),
        external_signature: first_clip.summary.external_signature.clone(),
        tessie_autopilot_percent: None,
        safety_hard_brake_ms: safety_totals.hard_brake_ms,
        safety_hard_brake_events,
        safety_aggr_turn_ms: safety_totals.aggr_turn_ms,
        safety_aggr_turn_events,
        safety_speeding_ms: safety_totals.speeding_ms,
        safety_moving_ms: safety_totals.moving_ms,
        safety_imu_moving_ms: safety_totals.imu_moving_ms,
        safety_manual_moving_ms: safety_totals.manual_moving_ms,
        safety_night_ms: safety_night_ms.round() as i64,
        safety_night_weighted_ms: safety_night_weighted_ms.round() as i64,
        safety_night_mi: round2(safety_night_m / calc::M_PER_MILE),
        safety_night_weighted_mi: round2(safety_night_weighted_m / calc::M_PER_MILE),
        safety_brake_any_ms: safety_totals.brake_any_ms,
        safety_turn_any_ms: safety_totals.turn_any_ms,
        safety_score,
    }
}

/// Drive-level telemetry rollup over the unique clips of a drive.
/// Iteration order matters: clips are time-ordered, so the first
/// clip with a populated `battery_pct_start` is the drive's start,
/// the last clip with a populated `battery_pct_end` is the drive's
/// end. The other fields are min/max/avg/sum semantically:
///   * battery_temp_avg, exterior_temp_avg: simple averages across
///     clips that have a value (each clip already averages its
///     samples — clip-of-clips average is approximate but fine for
///     a UI badge).
///   * interior_temp_min/max: extreme across all clips' extremes.
///   * hvac_runtime_s: sum of per-clip estimates (each clip's value
///     is in seconds within that clip's 60 s window).
///   * battery_pct_used: derived from start/end, rounded to two
///     decimals so the UI doesn't have to deal with FP precision.
struct DriveTelemetryRollup {
    battery_pct_start: Option<f64>,
    battery_pct_end: Option<f64>,
    battery_pct_used: Option<f64>,
    interior_temp_min_c: Option<f64>,
    interior_temp_max_c: Option<f64>,
    exterior_temp_avg_c: Option<f64>,
    hvac_runtime_s: Option<i64>,
    /// v7 TPMS — latest non-null reading per tire across the drive's
    /// clips (clips are time-ordered, so "latest" = last clip that
    /// had a value for that wheel).
    tire_fl_psi: Option<f64>,
    tire_fr_psi: Option<f64>,
    tire_rl_psi: Option<f64>,
    tire_rr_psi: Option<f64>,
    /// v9 odometer rollup.
    odometer_mi_start: Option<f64>,
    odometer_mi_end: Option<f64>,
    odometer_mi_driven: Option<f64>,
    /// v10 raw Tesla location-name strings from the first/last
    /// non-null clip in the drive. Stored verbatim — Tesla's
    /// reverse-geocoder picks the label (street address, business
    /// name, etc.). No post-processing or matching.
    location_name_start: Option<String>,
    location_name_end: Option<String>,
}

/// Rolls per-clip telemetry into per-drive scalars.
fn roll_up_telemetry(clips: &[SubClipSummary]) -> DriveTelemetryRollup {
    // Dedupe per parent file (same logic as the time-attributable
    // aggregates above) so a clip split into two sub-segments doesn't
    // get its telemetry counted twice.
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let mut battery_pct_start: Option<f64> = None;
    let mut battery_pct_end: Option<f64> = None;
    let mut interior_min: Option<f64> = None;
    let mut interior_max: Option<f64> = None;
    let mut ext_temp_sum = 0.0_f64;
    let mut ext_temp_n = 0_i64;
    let mut hvac_sum = 0_i64;
    let mut hvac_any = false;
    // TPMS — last non-null wins (clips iterate time-ordered).
    let mut tire_fl: Option<f64> = None;
    let mut tire_fr: Option<f64> = None;
    let mut tire_rl: Option<f64> = None;
    let mut tire_rr: Option<f64> = None;
    // Odometer: first non-null start, last non-null end.
    let mut odo_start: Option<f64> = None;
    let mut odo_end: Option<f64> = None;
    // Location names: first non-null start, last non-null end —
    // mirror the odometer logic. The matcher overrides these to
    // "Home"/"Work" downstream if the drive's GPS endpoints land
    // near the user's saved coords.
    let mut loc_start: Option<String> = None;
    let mut loc_end: Option<String> = None;

    for clip in clips {
        if !seen.insert(clip.summary.file.as_str()) {
            continue;
        }
        let t = &clip.summary.telemetry;
        if battery_pct_start.is_none() {
            battery_pct_start = t.battery_pct_start;
        }
        if let Some(v) = t.battery_pct_end {
            battery_pct_end = Some(v);
        }
        if let Some(v) = t.interior_temp_min {
            interior_min = Some(interior_min.map_or(v, |m| m.min(v)));
        }
        if let Some(v) = t.interior_temp_max {
            interior_max = Some(interior_max.map_or(v, |m| m.max(v)));
        }
        if let Some(v) = t.exterior_temp_avg {
            ext_temp_sum += v;
            ext_temp_n += 1;
        }
        if let Some(v) = t.hvac_runtime_s {
            hvac_sum += v;
            hvac_any = true;
        }
        if t.tire_fl_psi.is_some() { tire_fl = t.tire_fl_psi; }
        if t.tire_fr_psi.is_some() { tire_fr = t.tire_fr_psi; }
        if t.tire_rl_psi.is_some() { tire_rl = t.tire_rl_psi; }
        if t.tire_rr_psi.is_some() { tire_rr = t.tire_rr_psi; }
        if odo_start.is_none() {
            odo_start = t.odometer_mi_start;
        }
        if let Some(v) = t.odometer_mi_end {
            odo_end = Some(v);
        }
        if loc_start.is_none() {
            if let Some(v) = &t.location_name_start {
                loc_start = Some(v.clone());
            }
        }
        if let Some(v) = &t.location_name_end {
            loc_end = Some(v.clone());
        }
    }

    let battery_pct_used = match (battery_pct_start, battery_pct_end) {
        (Some(s), Some(e)) => Some(round2(s - e)),
        _ => None,
    };

    DriveTelemetryRollup {
        battery_pct_start: battery_pct_start.map(round2),
        battery_pct_end: battery_pct_end.map(round2),
        battery_pct_used,
        interior_temp_min_c: interior_min.map(round2),
        interior_temp_max_c: interior_max.map(round2),
        exterior_temp_avg_c: if ext_temp_n > 0 {
            Some(round2(ext_temp_sum / ext_temp_n as f64))
        } else {
            None
        },
        hvac_runtime_s: if hvac_any { Some(hvac_sum) } else { None },
        tire_fl_psi: tire_fl.map(round2),
        tire_fr_psi: tire_fr.map(round2),
        tire_rl_psi: tire_rl.map(round2),
        tire_rr_psi: tire_rr.map(round2),
        odometer_mi_start: odo_start.map(round2),
        odometer_mi_end: odo_end.map(round2),
        odometer_mi_driven: match (odo_start, odo_end) {
            (Some(s), Some(e)) if e >= s => Some(round2(e - s)),
            _ => None,
        },
        location_name_start: loc_start,
        location_name_end: loc_end,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cross_clip_grace_removes_only_the_covered_prefix() {
        let mut prefix = crate::safety::SafetyGracePrefix::default();
        for bucket in &mut prefix {
            bucket.hard_brake_ms = 1_000;
            bucket.brake_any_ms = 1_000;
            bucket.speeding_ms = 500;
        }

        let removed = safety_prefix_for_grace(&prefix, 2_500);

        assert_eq!(removed.hard_brake_ms, 2_500);
        assert_eq!(removed.brake_any_ms, 2_500);
        assert_eq!(removed.speeding_ms, 1_250);
    }

    #[test]
    fn period_breakdown_is_weighted_by_eligible_daily_miles() {
        let score = |value| crate::safety::SafetyScore {
            score: value,
            hard_brake_pct: value,
            aggr_turn_pct: value,
            speeding_pct: value,
            night_pct: value,
            hard_brake_penalty: value,
            aggr_turn_penalty: value,
            speeding_penalty: value,
            night_penalty: value,
            fsd_share_pct: value,
            fsd_relief_pct: 0.0,
        };

        let weighted = mileage_weighted_safety_breakdown(&[(score(90.0), 10.0), (score(70.0), 30.0)])
            .unwrap();

        assert_eq!(weighted.score, 75.0);
        assert_eq!(weighted.hard_brake_pct, 75.0);
        assert_eq!(weighted.fsd_relief_pct, 0.0);
    }

    /// Differential release gate for the streaming planner: over a
    /// fixture exercising every grouping rule, planning from metadata
    /// plus fragment assembly must serialize byte-identically to the
    /// in-memory oracle at several downsample limits. Single external
    /// signature only — multi-signature group order is nondeterministic
    /// in BOTH paths (HashMap buckets), so bytes can't be compared.
    #[test]
    fn planned_overviews_match_oracle() {
        let mk = |file: &str,
                  points: Vec<GpsPoint>,
                  gear_states: Vec<u8>,
                  gear_runs: Vec<GearRun>,
                  speeds: Vec<f32>,
                  raw_park: u32,
                  raw_frame: u32,
                  source: Option<&str>,
                  sig: Option<&str>| Route {
            file: file.to_string(),
            date: "d".to_string(),
            points,
            gear_states,
            autopilot_states: Vec::new(),
            speeds,
            accel_positions: Vec::new(),
            raw_park_count: raw_park,
            raw_frame_count: raw_frame,
            gear_runs,
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            source: source.map(|s| s.to_string()),
            external_signature: sig.map(|s| s.to_string()),
            ..Route::empty()
        };
        let p = |lat: f64, lng: f64| [lat, lng];

        let routes = vec![
            // Normal driving clip with a null island and a >1000km outlier.
            mk(
                "2025-01-01/2025-01-01_10-00-00-front.mp4",
                vec![p(37.7749, -122.4194), p(0.0, 0.0), p(37.7760, -122.4180), p(12.0, 30.0)],
                vec![4, 4, 4, 4],
                Vec::new(),
                vec![20.0, 21.0],
                0,
                4,
                Some("sei"),
                None,
            ),
            // Zero-point clip riding in the same drive.
            mk(
                "2025-01-01/2025-01-01_10-00-20-front.mp4",
                Vec::new(),
                vec![4],
                Vec::new(),
                Vec::new(),
                0,
                1,
                Some("sei"),
                None,
            ),
            // Park-split clip: D 300 / P 300 (=20s >= 2s) / D 300.
            mk(
                "2025-01-01/2025-01-01_10-00-40-front.mp4",
                (0..6).map(|i| p(37.7 + i as f64 * 0.001, -122.4)).collect(),
                vec![4, 4, 0, 0, 4, 4],
                vec![
                    GearRun { gear: 4, frames: 300 },
                    GearRun { gear: 0, frames: 300 },
                    GearRun { gear: 4, frames: 300 },
                ],
                vec![15.0; 6],
                300,
                900,
                Some("sei"),
                None,
            ),
            // Event clip interior to the 10:00:40 -> 10:08 hole, driving
            // proven ONLY by speeds (no gear, zero raw counts).
            mk(
                "SavedClips/2025-01-01_10-30-00/2025-01-01_10-04-00-front.mp4",
                vec![p(37.71, -122.41), p(37.712, -122.412)],
                Vec::new(),
                Vec::new(),
                vec![3.0, 3.0],
                0,
                0,
                Some("sei"),
                None,
            ),
            // Far anchor closing the hole.
            mk(
                "2025-01-01/2025-01-01_10-08-00-front.mp4",
                vec![p(37.72, -122.42), p(37.721, -122.421)],
                vec![4, 4],
                Vec::new(),
                vec![18.0, 18.0],
                0,
                2,
                Some("sei"),
                None,
            ),
            // Legacy group hours later: middle clip majority-park splits it.
            mk(
                "2025-01-01/2025-01-01_14-00-00-front.mp4",
                vec![p(37.8, -122.5), p(37.801, -122.501)],
                vec![4, 4],
                Vec::new(),
                Vec::new(),
                0,
                2,
                None,
                None,
            ),
            mk(
                "2025-01-01/2025-01-01_14-01-00-front.mp4",
                vec![p(37.802, -122.502)],
                vec![0, 0, 0],
                Vec::new(),
                Vec::new(),
                3,
                3,
                None,
                None,
            ),
            mk(
                "2025-01-01/2025-01-01_14-02-00-front.mp4",
                vec![p(37.803, -122.503), p(37.804, -122.504)],
                vec![4, 4],
                Vec::new(),
                Vec::new(),
                0,
                2,
                None,
                None,
            ),
            // Signature-bearing import in its own time window.
            mk(
                "2025-01-01/2025-01-01_18-00-00-front.mp4",
                vec![p(37.9, -122.6), p(37.901, -122.601)],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                0,
                0,
                Some("tessie"),
                Some("sig-a"),
            ),
            // Duplicate path (backslash variant) that dedup must drop.
            mk(
                "2025-01-01\\2025-01-01_10-00-00-front.mp4",
                vec![p(1.5, 1.5)],
                vec![4],
                Vec::new(),
                Vec::new(),
                0,
                1,
                Some("sei"),
                None,
            ),
        ];

        for max_points in [2usize, 20, 500] {
            let oracle = route_overviews(routes.clone(), max_points);

            let metas: Vec<OverviewClipMeta> =
                routes.iter().map(OverviewClipMeta::from_route).collect();
            let plans = plan_overviews(&metas);
            let planned: Vec<RouteOverview> = plans
                .iter()
                .enumerate()
                .map(|(idx, d)| {
                    overview_from_fragments(
                        idx as i32,
                        d,
                        &metas,
                        &mut |i| Ok(std::rc::Rc::new(routes[i].points.clone())),
                        max_points,
                    )
                    .unwrap()
                })
                .collect();

            assert_eq!(
                serde_json::to_string(&oracle).unwrap(),
                serde_json::to_string(&planned).unwrap(),
                "planner output diverged at max_points={max_points}"
            );
        }
    }

    #[test]
    fn test_parse_file_timestamp() {
        let ts =
            parse_file_timestamp("/mnt/usb/TeslaCam/2025-01-15_12-30-45-front.mp4").unwrap();
        assert_eq!(ts.format("%Y-%m-%dT%H:%M:%S").to_string(), "2025-01-15T12:30:45");
    }

    #[test]
    fn test_parse_file_timestamp_backslash() {
        let ts =
            parse_file_timestamp("C:\\TeslaCam\\2025-01-15_12-30-45-front.mp4").unwrap();
        assert_eq!(ts.format("%Y-%m-%dT%H:%M:%S").to_string(), "2025-01-15T12:30:45");
    }

    #[test]
    fn test_parse_file_timestamp_none() {
        assert!(parse_file_timestamp("no-timestamp-here.mp4").is_none());
    }

    #[test]
    fn test_haversine_m() {
        // New York to Los Angeles ~ 3,944 km
        let d = calc::geodesic_m(40.7128, -74.0060, 34.0522, -118.2437);
        assert!((d - 3_944_000.0).abs() < 50_000.0); // within 50km
    }

    #[test]
    fn test_haversine_m_same_point() {
        assert_eq!(calc::geodesic_m(37.7749, -122.4194, 37.7749, -122.4194), 0.0);
    }

    #[test]
    fn test_downsample_no_op() {
        let pts = vec![[1.0, 2.0], [3.0, 4.0]];
        assert_eq!(downsample(&pts, 10).len(), 2);
    }

    #[test]
    fn test_downsample_reduces() {
        let pts: Vec<GpsPoint> = (0..100).map(|i| [i as f64, i as f64]).collect();
        let ds = downsample(&pts, 10);
        assert_eq!(ds.len(), 11); // 10 + 1 (last point)
        assert_eq!(ds[0], [0.0, 0.0]);
        assert_eq!(*ds.last().unwrap(), [99.0, 99.0]);
    }

    #[test]
    fn test_round2() {
        // Not 3.14159: clippy's approx_constant refuses π look-alikes.
        assert_eq!(round2(1.23456), 1.23);
        assert_eq!(round2(0.005), 0.01);
    }

    #[test]
    fn test_round1() {
        assert_eq!(round1(1.24), 1.2);
        // 1.25 is exactly representable, so this pins half-away-from-zero.
        assert_eq!(round1(1.25), 1.3);
    }

    #[test]
    fn test_group_clips_empty() {
        let groups = group_clips(Vec::new());
        assert!(groups.is_empty());
    }

    fn test_route(file: &str, points: Vec<[f64; 2]>) -> Route {
        Route {
            file: file.to_string(),
            date: "2025-01-15".to_string(),
            points,
            gear_states: vec![1],
            autopilot_states: vec![0],
            speeds: vec![10.0],
            accel_positions: vec![0.0],
            raw_park_count: 0,
            raw_frame_count: 10,
            gear_runs: vec![GearRun { gear: 1, frames: 10 }],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_group_clips_single() {
        let routes = vec![test_route(
            "/cam/2025-01-15_12-30-45-front.mp4",
            vec![[37.0, -122.0]],
        )];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 1);
    }

    #[test]
    fn test_group_clips_time_gap_split() {
        let routes = vec![
            test_route("/cam/2025-01-15_12-00-00-front.mp4", vec![[37.0, -122.0]]),
            test_route("/cam/2025-01-15_13-00-00-front.mp4", vec![[37.1, -122.1]]),
        ];
        let groups = group_clips(routes);
        // 1 hour gap > 5 min => 2 groups
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn test_build_single_drive_dates_gapfill_by_clip_not_event_folder() {
        // Regression: a gap-fill clip lives at an event-folder path whose
        // FOLDER timestamp (12:40:00) is 38 min after the clip's own time
        // (12:02:00). build_single_drive_from_clips must date it by the clip
        // (basename) time — a left-to-right path scan grabs the folder time
        // first and drops the clip's points ~38 min late, blowing a phantom
        // gap in the drive's speed timeline (real 4C+ bug, drive 241).
        let native = test_route(
            "RecentClips/2026-01-15/2026-01-15_12-00-00-front.mp4",
            vec![[37.0, -122.0]],
        );
        let gapfill = test_route(
            "SentryClips/2026-01-15_12-40-00/2026-01-15_12-02-00-front.mp4",
            vec![[37.02, -122.0]],
        );
        let drive =
            build_single_drive_from_clips(&[native, gapfill], 0, &HashMap::new(), None)
                .expect("drive");
        assert_eq!(drive.clip_count, 2);
        let max_t = drive.points.iter().map(|p| p[2]).fold(0.0_f64, f64::max);
        // Clip time (12:02) => ~120 000 ms. Event-folder time (12:40) =>
        // ~2 280 000 ms. Anything past 5 min means the old path-scan bug.
        assert!(
            max_t < 300_000.0,
            "gap-fill clip placed at {max_t} ms — expected ~120 000 (clip time), not the event-folder time",
        );
    }

    #[test]
    fn test_single_drive_park_split_scopes_points_to_target_segment() {
        // A summon and the following drive sharing ONE clip, separated by
        // a 10 s mid-clip Park (the fused-summon shape). The detail
        // rebuild receives the whole parent clip; without the park split
        // + target pick it merged BOTH segments' points, so the detail
        // map drew the neighbor's route while the mini-map path
        // (route_overviews, which splits) drew only the drive's own —
        // the mismatch this test locks out.
        let mut shared = test_route(
            "RecentClips/2026-07-27/2026-07-27_20-04-00-front.mp4",
            (0..18).map(|i| [37.0 + i as f64 * 1e-4, -122.0]).collect(),
        );
        shared.raw_frame_count = 1800;
        shared.gear_runs = vec![
            GearRun { gear: 1, frames: 600 },          // summon crawl
            GearRun { gear: GEAR_PARK, frames: 300 },  // 10 s park
            GearRun { gear: 1, frames: 900 },          // human drives off
        ];
        let tags = HashMap::new();

        // Summary start of the summon = clip start (offset 0).
        let summon = build_single_drive_from_clips(
            &[shared.clone()],
            0,
            &tags,
            Some("2026-07-27T20:04:00"),
        )
        .expect("summon segment");
        assert_eq!(summon.point_count, 6, "first segment's points only");

        // Summary start of the following drive ≈ clip start + 30 s
        // (frame 900 of 1800). Nearest-start matching absorbs the
        // gear-frame vs point-fraction offset difference.
        let human = build_single_drive_from_clips(
            &[shared.clone()],
            1,
            &tags,
            Some("2026-07-27T20:04:33"),
        )
        .expect("human segment");
        assert_eq!(human.point_count, 9, "second segment's points only");

        // No target: first sub-drive wins.
        let default_pick =
            build_single_drive_from_clips(&[shared], 0, &tags, None).expect("drive");
        assert_eq!(default_pick.point_count, 6);
    }

    #[test]
    fn test_distance_from_summary_clips_includes_inter_clip_gap() {
        let mut a1 = RouteAggregates::default();
        a1.distance_m = 100.0;
        a1.start_lat = Some(37.0000);
        a1.start_lng = Some(-122.0000);
        a1.end_lat = Some(37.0009);
        a1.end_lng = Some(-122.0000);

        let mut a2 = RouteAggregates::default();
        a2.distance_m = 200.0;
        a2.start_lat = Some(37.0020);
        a2.start_lng = Some(-122.0000);
        a2.end_lat = Some(37.0030);
        a2.end_lng = Some(-122.0000);

        let s1 = RouteSummary {
            file: "/cam/2025-01-15_12-00-00-front.mp4".to_string(),
            date: "2025-01-15".to_string(),
            raw_park_count: 0,
            raw_frame_count: 0,
            gear_runs: Vec::new(),
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: a1,
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        };
        let s2 = RouteSummary {
            file: "/cam/2025-01-15_12-01-00-front.mp4".to_string(),
            date: "2025-01-15".to_string(),
            raw_park_count: 0,
            raw_frame_count: 0,
            gear_runs: Vec::new(),
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: a2,
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        };

        let ts = chrono::NaiveDateTime::parse_from_str("2025-01-15T12:00:00", "%Y-%m-%dT%H:%M:%S")
            .unwrap();
        let clips = vec![
            SubClipSummary::whole(TimedSummary { summary: &s1, timestamp: ts, clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s2, timestamp: ts + chrono::Duration::minutes(1), clip_span_ms: CLIP_DURATION_MS }),
        ];

        let d = distance_from_summary_clips(&clips).total_m;
        let gap = calc::geodesic_m(37.0009, -122.0000, 37.0020, -122.0000);
        assert!(
            (d - (300.0 + gap)).abs() < 0.1,
            "distance should include inter-clip gap"
        );
    }

    /// Build a RouteSummary with a single non-park gear run covering the
    /// whole clip. Useful for grouping tests that don't care about
    /// aggregates.
    fn drive_summary(file: &str) -> RouteSummary {
        RouteSummary {
            file: file.to_string(),
            date: "2025-01-15".to_string(),
            raw_park_count: 0,
            raw_frame_count: 60,
            gear_runs: vec![GearRun { gear: 1, frames: 60 }],
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: RouteAggregates::default(),
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        }
    }

    /// Build a RouteSummary that has internal park gaps at the supplied
    /// frame ranges. `runs` is a sequence of `(gear, frames)` pairs that
    /// must sum to 60. Aggregate distance is split across the segments
    /// so the fraction-aware aggregator has something to compare.
    fn clip_with_gear_runs(file: &str, runs: &[(u8, u32)], total_distance_m: f64) -> RouteSummary {
        let mut a = RouteAggregates::default();
        a.distance_m = total_distance_m;
        RouteSummary {
            file: file.to_string(),
            date: "2025-01-15".to_string(),
            raw_park_count: runs.iter().filter(|(g, _)| *g == GEAR_PARK).map(|(_, f)| f).sum(),
            raw_frame_count: runs.iter().map(|(_, f)| *f).sum(),
            gear_runs: runs.iter().map(|(g, f)| GearRun { gear: *g, frames: *f }).collect(),
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: a,
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        }
    }

    /// A single clip with TWO internal park gaps
    /// (drive/park/drive/park/drive) must split into three drives, each
    /// receiving its proportional slice of the clip's per-clip
    /// aggregates. Regression guard against the previous atomic-clip
    /// behavior that produced one drive boundary and dumped the full
    /// clip's aggregates into the first drive.
    #[test]
    fn test_split_summary_multi_park_gap_produces_three_drives() {
        // 60 frames total: drive 20, park 5, drive 15, park 5, drive 15.
        // Park runs (5 frames * 1s/frame = 5s) are > PARK_GAP_SECONDS (2s).
        let clip = clip_with_gear_runs(
            "/cam/2025-01-15_12-00-00-front.mp4",
            &[(1, 20), (GEAR_PARK, 5), (1, 15), (GEAR_PARK, 5), (1, 15)],
            600.0, // 600m total clip distance
        );
        let summaries = vec![clip];
        let groups = group_summary_clips(&summaries);
        assert_eq!(groups.len(), 3, "multi-park-gap clip should split into 3 drives");

        // Each drive should get its slice of the clip's distance.
        // drive 1: 20/60 = 0.333 → 200m
        // drive 2: 15/60 = 0.25  → 150m
        // drive 3: 15/60 = 0.25  → 150m
        // (Sub-clip totals = 50/60 of clip's distance = 500m, by design
        // — the 10/60 parked portion is dropped on the cutting-room floor.)
        let d1 = distance_from_summary_clips(&groups[0]).total_m;
        let d2 = distance_from_summary_clips(&groups[1]).total_m;
        let d3 = distance_from_summary_clips(&groups[2]).total_m;
        assert!((d1 - 200.0).abs() < 0.5, "drive 1 distance: {}", d1);
        assert!((d2 - 150.0).abs() < 0.5, "drive 2 distance: {}", d2);
        assert!((d3 - 150.0).abs() < 0.5, "drive 3 distance: {}", d3);
    }

    /// Sanity: a clip with NO internal park gap stays in one drive and
    /// keeps its full aggregates (fraction = 1.0 path).
    #[test]
    fn test_split_summary_no_park_gap_keeps_one_drive() {
        let clip = clip_with_gear_runs(
            "/cam/2025-01-15_12-00-00-front.mp4",
            &[(1, 60)],
            1000.0,
        );
        let summaries = vec![clip];
        let groups = group_summary_clips(&summaries);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 1);
        assert!((distance_from_summary_clips(&groups[0]).total_m - 1000.0).abs() < 0.5);
    }

    /// Sanity: a fully-parked clip closes the current drive and does not
    /// produce a sub-clip for itself. Drives count is 0 when ALL clips
    /// are parked.
    #[test]
    fn test_split_summary_all_parked_zero_drives() {
        let clip = clip_with_gear_runs(
            "/cam/2025-01-15_12-00-00-front.mp4",
            &[(GEAR_PARK, 60)],
            0.0,
        );
        let summaries = vec![clip];
        let groups = group_summary_clips(&summaries);
        assert_eq!(groups.len(), 0);
    }

    /// Drive-bounded scenario: two real drive clips bracketing a fully
    /// parked clip should split into two drives.
    #[test]
    fn test_split_summary_park_clip_between_drives() {
        let s1 = drive_summary("/cam/2025-01-15_12-00-00-front.mp4");
        let park = clip_with_gear_runs(
            "/cam/2025-01-15_12-01-00-front.mp4",
            &[(GEAR_PARK, 60)],
            0.0,
        );
        let s3 = drive_summary("/cam/2025-01-15_12-02-00-front.mp4");
        let summaries = vec![s1, park, s3];
        let groups = group_summary_clips(&summaries);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 1);
        assert_eq!(groups[1].len(), 1);
    }

    #[test]
    fn test_is_event_folder_path() {
        // Linux-style paths produced by scan_dir.
        assert!(is_event_folder_path("SavedClips/2026-05-17_18-47-59/2026-05-17_18-47-34-front.mp4"));
        assert!(is_event_folder_path("SentryClips/2026-05-17_18-46-39/2026-05-17_18-35-39-front.mp4"));
        // Windows-style paths from a Sentry-Drive drive-data.json import.
        assert!(is_event_folder_path("SavedClips\\2026-05-17_18-47-59\\2026-05-17_18-47-34-front.mp4"));
        assert!(is_event_folder_path("SentryClips\\foo\\bar-front.mp4"));
        // Real drive content stays in.
        assert!(!is_event_folder_path("RecentClips/2026-05-17/2026-05-17_18-47-34-front.mp4"));
        assert!(!is_event_folder_path("2026-05-17/2026-05-17_18-47-34-front.mp4"));
        assert!(!is_event_folder_path("2026-05-17\\2026-05-17_18-47-34-front.mp4"));
        assert!(!is_event_folder_path(""));
        // Substring matches don't count — must be a top-level segment.
        assert!(!is_event_folder_path("foo/SavedClips/x.mp4"));
        assert!(!is_event_folder_path("MySavedClips/x.mp4"));
    }

    /// Park-only gear run for one full-minute clip (60 raw frames). Used to
    /// model SentryClips event recordings where the car was parked the
    /// entire time.
    fn park_route(file: &str, lat: f64) -> Route {
        Route {
            file: file.to_string(),
            date: "SentryClips".to_string(),
            points: vec![[lat, -76.795]],
            gear_states: vec![GEAR_PARK; 60],
            autopilot_states: vec![AUTOPILOT_OFF; 60],
            speeds: vec![0.0; 60],
            accel_positions: vec![0.0; 60],
            raw_park_count: 60,
            raw_frame_count: 60,
            gear_runs: vec![GearRun {
                gear: GEAR_PARK,
                frames: 60,
            }],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
            ..Default::default()
        }
    }

    #[test]
    fn test_group_clips_filters_event_folder_routes() {
        // Three routes within the same minute — without filtering they'd all
        // land in one time group. With filtering only the RecentClips route
        // survives, and the group contains exactly one clip.
        let routes = vec![
            test_route(
                "RecentClips/2025-01-15/2025-01-15_12-30-00-front.mp4",
                vec![[37.0, -122.0]],
            ),
            test_route(
                "SavedClips/2025-01-15_12-30-30/2025-01-15_12-30-00-front.mp4",
                vec![[37.0, -122.0]],
            ),
            test_route(
                "SentryClips/2025-01-15_12-29-30/2025-01-15_12-30-00-front.mp4",
                vec![[37.0, -122.0]],
            ),
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1, "expected one drive after filtering");
        assert_eq!(groups[0].len(), 1, "expected one route in the drive");
        assert!(
            groups[0][0].route.file.starts_with("RecentClips/"),
            "the surviving route should be the RecentClips one, got {}",
            groups[0][0].route.file
        );
    }

    #[test]
    fn test_group_clips_may17_regression() {
        // Reproduces the user-reported May 17 6:47 PM scenario:
        //   - 11 SentryClips event recordings (car parked) from 18:35-18:45
        //   - 1 SavedClips duplicate of the 18:47:34 RecentClips file
        //   - 5 RecentClips files from the actual drive 18:47:34 - 18:51:34
        //
        // Before the fix this produced 2 drives: a fake "parked" drive built
        // from the SentryClips Park frames, then the real trip. With the fix
        // the event-folder routes are filtered, leaving a single drive of 5
        // RecentClips routes.
        let mut routes: Vec<Route> = Vec::new();

        // SentryClips: 11 minutes of parked recording, all Park gear.
        for minute in 35..=45 {
            let file = format!(
                "SentryClips/2026-05-17_18-46-39/2026-05-17_18-{:02}-{:02}-front.mp4",
                minute,
                39 + (minute - 35),
            );
            routes.push(park_route(&file, 39.198_8 + (minute as f64) * 1e-6));
        }

        // SavedClips: one duplicate of the 18:47:34 RecentClips file.
        routes.push(test_route(
            "SavedClips/2026-05-17_18-47-59/2026-05-17_18-47-34-front.mp4",
            vec![[39.198_835, -76.795_246]],
        ));

        // RecentClips: 5 minutes of actual driving.
        let drive_starts = [
            "RecentClips/2026-05-17/2026-05-17_18-47-34-front.mp4",
            "RecentClips/2026-05-17/2026-05-17_18-48-34-front.mp4",
            "RecentClips/2026-05-17/2026-05-17_18-49-34-front.mp4",
            "RecentClips/2026-05-17/2026-05-17_18-50-34-front.mp4",
            "RecentClips/2026-05-17/2026-05-17_18-51-34-front.mp4",
        ];
        for (i, f) in drive_starts.iter().enumerate() {
            routes.push(test_route(
                f,
                vec![[39.198_835 + (i as f64) * 1e-4, -76.795_246]],
            ));
        }

        let groups = group_clips(routes);
        assert_eq!(
            groups.len(),
            1,
            "May 17 trip must group into a single drive; got {} groups",
            groups.len()
        );
        assert_eq!(
            groups[0].len(),
            drive_starts.len(),
            "the drive should contain exactly the {} RecentClips routes",
            drive_starts.len()
        );
        for clip in &groups[0] {
            assert!(
                clip.route.file.starts_with("RecentClips/"),
                "unexpected non-RecentClips route in drive: {}",
                clip.route.file
            );
        }
    }

    // ── Event-clip gap-fill tests ────────────────────────────────────

    fn dts(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    #[test]
    fn test_parse_clip_timestamp_uses_filename_component() {
        // Event paths embed the event-folder timestamp FIRST — a full-path
        // scan would return 18:46:39 instead of the clip's 18:35:39.
        assert_eq!(
            parse_clip_timestamp("SentryClips/2026-05-17_18-46-39/2026-05-17_18-35-39-front.mp4"),
            Some(dts("2026-05-17 18:35:39"))
        );
        assert_eq!(
            parse_clip_timestamp("SavedClips\\2026-05-17_18-47-59\\2026-05-17_18-47-34-front.mp4"),
            Some(dts("2026-05-17 18:47:34"))
        );
        assert_eq!(
            parse_clip_timestamp("RecentClips/2026-05-17/2026-05-17_18-47-34-front.mp4"),
            Some(dts("2026-05-17 18:47:34"))
        );
        assert_eq!(parse_clip_timestamp("SentryClips/2026-05-17_18-46-39/event.json"), None);
    }

    #[test]
    fn test_fillable_holes_bounds() {
        // Normal 60s spacing → no hole. A 3-min and a 6-min gap are both
        // fillable holes: sentry-covered dropouts run long (the car parked,
        // so the gap spans arrival + a chain of ~10-min pre-roll events —
        // real data shows 9-19 min). A 35-min gap exceeds GAP_FILL_MAX_MS →
        // a genuine multi-drive boundary, not a recording dropout.
        let seq = vec![
            dts("2026-06-01 10:00:00"),
            dts("2026-06-01 10:01:00"),
            dts("2026-06-01 10:04:00"), // 3-min gap after 10:01
            dts("2026-06-01 10:05:00"),
            dts("2026-06-01 10:11:00"), // 6-min gap after 10:05
            dts("2026-06-01 10:46:00"), // 35-min gap after 10:11 (> GAP_FILL_MAX_MS)
        ];
        let holes = fillable_holes(&seq);
        assert_eq!(
            holes,
            vec![
                (dts("2026-06-01 10:01:00"), dts("2026-06-01 10:04:00")),
                (dts("2026-06-01 10:05:00"), dts("2026-06-01 10:11:00")),
            ]
        );
        // Strict interior: endpoints are occupied RecentClips slots.
        assert!(ts_in_holes(&holes, dts("2026-06-01 10:02:00")));
        assert!(ts_in_holes(&holes, dts("2026-06-01 10:07:00"))); // inside the 6-min hole
        assert!(!ts_in_holes(&holes, dts("2026-06-01 10:01:00")));
        assert!(!ts_in_holes(&holes, dts("2026-06-01 10:04:00")));
        assert!(!ts_in_holes(&holes, dts("2026-06-01 10:11:00")));
        // The 35-min gap is a drive boundary, never fillable.
        assert!(!ts_in_holes(&holes, dts("2026-06-01 10:30:00")));
    }

    #[test]
    fn test_select_gap_fill_events_dedup() {
        let recent = vec![
            dts("2026-06-01 10:00:00"),
            dts("2026-06-01 10:03:00"),
        ];
        // Same missing minute exists in both event folders (overlapping
        // events) → exactly one winner, lowest path.
        let cands = vec![
            (dts("2026-06-01 10:01:00"), "SentryClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"),
            (dts("2026-06-01 10:01:00"), "SavedClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"),
            (dts("2026-06-01 10:02:00"), "SentryClips/2026-06-01_10-02-30/2026-06-01_10-02-00-front.mp4"),
            // Outside any hole (parked recording) → rejected.
            (dts("2026-06-01 09:30:00"), "SentryClips/2026-06-01_09-31-00/2026-06-01_09-30-00-front.mp4"),
        ];
        let mut picked: Vec<&str> =
            select_gap_fill_events(&recent, &cands).into_iter().map(|i| cands[i].1).collect();
        picked.sort();
        assert_eq!(
            picked,
            vec![
                "SavedClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4",
                "SentryClips/2026-06-01_10-02-30/2026-06-01_10-02-00-front.mp4",
            ]
        );
    }

    #[test]
    fn test_select_interior_fill_admits_parked_hole_clips_only() {
        // A user save moved parked minutes 10:01-10:02 out of RecentClips.
        // No SEI/driving info exists at selection time — interior clips
        // must be admitted anyway (playback continuity), while anything
        // outside a hole stays out regardless.
        let recent = vec![
            dts("2026-06-01 10:00:00"),
            dts("2026-06-01 10:03:00"),
            dts("2026-06-01 10:04:00"),
        ];
        let cands = vec![
            // Interior hole fills → admitted.
            (dts("2026-06-01 10:01:00"), "SavedClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"),
            (dts("2026-06-01 10:02:00"), "SavedClips/2026-06-01_10-02-30/2026-06-01_10-02-00-front.mp4"),
            // Leading (before first recent clip) → chain shapes stay
            // driving-gated on the route path, never admitted here.
            (dts("2026-06-01 09:58:00"), "SavedClips/2026-06-01_10-02-30/2026-06-01_09-58-00-front.mp4"),
            // Trailing (after last recent clip) → same.
            (dts("2026-06-01 10:06:00"), "SentryClips/2026-06-01_10-07-00/2026-06-01_10-06-00-front.mp4"),
            // Duplicate of the occupied 10:04 slot (twin stamped 1s later) → rejected.
            (dts("2026-06-01 10:04:01"), "SavedClips/2026-06-01_10-05-00/2026-06-01_10-04-01-front.mp4"),
        ];
        let picked: Vec<&str> = select_interior_fill(&recent, &cands)
            .into_iter()
            .map(|i| cands[i].1)
            .collect();
        assert_eq!(
            picked,
            vec![
                "SavedClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4",
                "SavedClips/2026-06-01_10-02-30/2026-06-01_10-02-00-front.mp4",
            ]
        );
    }

    #[test]
    fn test_select_interior_fill_twin_dedup_lowest_path_wins() {
        let recent = vec![
            dts("2026-06-01 10:00:00"),
            dts("2026-06-01 10:03:00"),
        ];
        // The same missing minute exists in both event folders → exactly
        // one winner, lowest path (SavedClips sorts before SentryClips).
        let cands = vec![
            (dts("2026-06-01 10:01:00"), "SentryClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"),
            (dts("2026-06-01 10:01:00"), "SavedClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"),
        ];
        let picked: Vec<&str> = select_interior_fill(&recent, &cands)
            .into_iter()
            .map(|i| cands[i].1)
            .collect();
        assert_eq!(
            picked,
            vec!["SavedClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"]
        );
    }

    #[test]
    fn test_select_interior_fill_no_holes_or_empty_recent() {
        let cands = vec![
            (dts("2026-06-01 10:01:00"), "SavedClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"),
        ];
        // No recent timeline at all → an isolated event cluster, nothing
        // to anchor a hole → nothing admitted.
        assert!(select_interior_fill(&[], &cands).is_empty());
        // Contiguous recent timeline (no fillable hole) → nothing admitted.
        let recent = vec![dts("2026-06-01 10:00:00"), dts("2026-06-01 10:01:00")];
        assert!(select_interior_fill(&recent, &cands).is_empty());
    }

    #[test]
    fn test_group_clips_gap_fills_event_route_into_hole() {
        // RecentClips drive with one missing minute (10:01); the same
        // minute exists in a SentryClips pre-roll. It must be admitted
        // once; the duplicate of an occupied slot and the parked clip
        // must stay filtered.
        let routes = vec![
            test_route("RecentClips/2026-06-01/2026-06-01_10-00-00-front.mp4", vec![[37.0, -122.0]]),
            test_route("RecentClips/2026-06-01/2026-06-01_10-02-00-front.mp4", vec![[37.002, -122.0]]),
            test_route("RecentClips/2026-06-01/2026-06-01_10-03-00-front.mp4", vec![[37.003, -122.0]]),
            // Fills the hole.
            test_route("SentryClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4", vec![[37.001, -122.0]]),
            // Duplicate of the occupied 10:02 slot → filtered.
            test_route("SavedClips/2026-06-01_10-02-30/2026-06-01_10-02-00-front.mp4", vec![[37.002, -122.0]]),
            // Parked recording hours earlier, no surrounding drive → filtered.
            park_route("SentryClips/2026-06-01_02-00-00/2026-06-01_01-59-00-front.mp4", 37.0),
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1, "expected a single gap-filled drive");
        let files: Vec<&str> = groups[0].iter().map(|c| c.route.file.as_str()).collect();
        assert_eq!(
            files,
            vec![
                "RecentClips/2026-06-01/2026-06-01_10-00-00-front.mp4",
                "SentryClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4",
                "RecentClips/2026-06-01/2026-06-01_10-02-00-front.mp4",
                "RecentClips/2026-06-01/2026-06-01_10-03-00-front.mp4",
            ]
        );
    }

    #[test]
    fn test_group_summary_clips_gap_fill_and_event_filter() {
        let mk = |file: &str| RouteSummary {
            file: file.to_string(),
            date: "2026-06-01".to_string(),
            raw_park_count: 0,
            raw_frame_count: 60,
            gear_runs: vec![GearRun { gear: 1, frames: 60 }],
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: RouteAggregates::default(),
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        };
        // Parked event row far from any drive → filtered. Must carry
        // actual Park telemetry: a gear-verified DRIVING event row with
        // no anchor now legitimately forms its own drive (unanchored
        // driving cluster), so the old all-Drive fixture would assert
        // the wrong thing.
        let mut parked_far = mk("SentryClips/2026-06-01_02-00-00/2026-06-01_01-59-00-front.mp4");
        parked_far.raw_park_count = 60;
        parked_far.gear_runs = vec![GearRun { gear: GEAR_PARK, frames: 60 }];
        let summaries = vec![
            mk("2026-06-01/2026-06-01_10-00-00-front.mp4"),
            mk("2026-06-01/2026-06-01_10-02-00-front.mp4"),
            mk("SentryClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4"),
            parked_far,
        ];
        let groups = group_summary_clips(&summaries);
        assert_eq!(groups.len(), 1);
        let files: Vec<&str> = groups[0].iter().map(|c| c.summary.file.as_str()).collect();
        assert_eq!(
            files,
            vec![
                "2026-06-01/2026-06-01_10-00-00-front.mp4",
                "SentryClips/2026-06-01_10-02-30/2026-06-01_10-01-00-front.mp4",
                "2026-06-01/2026-06-01_10-02-00-front.mp4",
            ]
        );
    }

    // ── Comprehensive gap-fill: driving gate + trailing/leading chains ──

    #[test]
    fn test_telemetry_has_driving() {
        let d = |g| GearRun { gear: g, frames: 30 };
        // Non-park gear run → driving.
        assert!(telemetry_has_driving(&[d(GEAR_PARK), d(1)], &[], &[], 0, 0));
        // All-park gear runs, no speed → parked.
        assert!(!telemetry_has_driving(&[d(GEAR_PARK)], &[GEAR_PARK; 60], &[0.0; 60], 60, 60));
        // Reverse reports NEGATIVE speeds — abs() must count as driving.
        assert!(telemetry_has_driving(&[d(GEAR_PARK)], &[], &[-2.0], 0, 0));
        // Crawl below threshold is not driving.
        assert!(!telemetry_has_driving(&[], &[], &[0.2], 0, 0));
        // Legacy row: raw counts only, some non-park frames.
        assert!(telemetry_has_driving(&[], &[], &[], 40, 60));
        // No telemetry at all: no positive evidence → not driving.
        assert!(!telemetry_has_driving(&[], &[], &[], 0, 0));
    }

    #[test]
    fn test_gap_fill_trailing_chain_admits_driving_rejects_parked_tail() {
        // Drive 236 shape (Jul 4): RecentClips end mid-drive; the rest of
        // the drive lives in an event pre-roll, followed by parked pre-roll
        // clips after the car stopped. Driving clips chain in; parked ones
        // never do, so the fill can't bleed into the parked sentry bloat.
        let routes = vec![
            test_route("RecentClips/2026-07-04/2026-07-04_20-42-50-front.mp4", vec![[37.0, -122.0]]),
            test_route("RecentClips/2026-07-04/2026-07-04_20-43-50-front.mp4", vec![[37.001, -122.0]]),
            // Trailing fill: drive continues in the pre-roll.
            test_route("SentryClips/2026-07-04_20-55-50/2026-07-04_20-44-51-front.mp4", vec![[37.002, -122.0]]),
            test_route("SentryClips/2026-07-04_20-55-50/2026-07-04_20-45-51-front.mp4", vec![[37.003, -122.0]]),
            // Parked pre-roll clips after the car stopped → excluded.
            park_route("SentryClips/2026-07-04_20-55-50/2026-07-04_20-46-52-front.mp4", 37.003),
            park_route("SentryClips/2026-07-04_20-55-50/2026-07-04_20-47-52-front.mp4", 37.003),
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1, "expected a single extended drive");
        let files: Vec<&str> = groups[0].iter().map(|c| c.route.file.as_str()).collect();
        assert_eq!(
            files,
            vec![
                "RecentClips/2026-07-04/2026-07-04_20-42-50-front.mp4",
                "RecentClips/2026-07-04/2026-07-04_20-43-50-front.mp4",
                "SentryClips/2026-07-04_20-55-50/2026-07-04_20-44-51-front.mp4",
                "SentryClips/2026-07-04_20-55-50/2026-07-04_20-45-51-front.mp4",
            ]
        );
    }

    #[test]
    fn test_gap_fill_leading_chain_admits_departure_footage() {
        // Departure footage that only made an event pre-roll before the
        // first RecentClips clip: chains backwards off the drive start.
        let routes = vec![
            test_route("SentryClips/2026-06-27_10-05-00/2026-06-27_09-58-10-front.mp4", vec![[37.0, -122.0]]),
            test_route("SentryClips/2026-06-27_10-05-00/2026-06-27_09-59-10-front.mp4", vec![[37.001, -122.0]]),
            test_route("RecentClips/2026-06-27/2026-06-27_10-00-10-front.mp4", vec![[37.002, -122.0]]),
            test_route("RecentClips/2026-06-27/2026-06-27_10-01-10-front.mp4", vec![[37.003, -122.0]]),
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1, "expected a single drive starting at the leading fill");
        assert_eq!(groups[0].len(), 4);
        assert_eq!(
            groups[0][0].route.file,
            "SentryClips/2026-06-27_10-05-00/2026-06-27_09-58-10-front.mp4"
        );
    }

    #[test]
    fn test_gap_fill_isolated_driving_cluster_forms_own_drive() {
        // Driving footage with NO adjacent RecentClips anchor (a user
        // save / sentry event can swallow an ENTIRE short drive — Tesla
        // moves every minute of it out of RecentClips, so nothing is left
        // to anchor to). Ego SEI gear evidence already proves this is the
        // recording car moving, so the cluster must surface as its own
        // drive instead of vanishing (2026-08-08 honk-save incident).
        let routes = vec![
            test_route("RecentClips/2026-05-28/2026-05-28_10-00-00-front.mp4", vec![[37.0, -122.0]]),
            test_route("SentryClips/2026-05-28_14-05-00/2026-05-28_14-00-00-front.mp4", vec![[37.1, -122.1]]),
            test_route("SentryClips/2026-05-28_14-05-00/2026-05-28_14-01-00-front.mp4", vec![[37.1, -122.1]]),
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 2, "unanchored driving cluster should be its own drive");
        assert!(groups[0][0].route.file.starts_with("RecentClips/"));
        assert_eq!(groups[1].len(), 2);
        assert!(groups[1][0].route.file.starts_with("SentryClips/"));
    }

    #[test]
    fn test_gap_fill_honk_save_timeline_recovers_drive() {
        // Exact shape of the 2026-08-08 20:05:16 honk save: the whole
        // 19:55→20:00 drive was moved into SavedClips; only 4 of its
        // minutes carry SEI (the car's telemetry stream ramps up after
        // wake) and became routes. Nearest RecentClips route is 20:05:13
        // — 367 s away, past the 3-min chain hop — and the surrounding
        // RecentClips hole is hours wide, so interior fill can't apply
        // either. The driving cluster must still render as its own drive,
        // separate from the later 20:05 stub.
        let routes = vec![
            test_route("RecentClips/2026-08-08/2026-08-08_18-11-46-front.mp4", vec![[39.0, -76.9]]),
            test_route("SavedClips/2026-08-08_20-05-16/2026-08-08_19-56-05-front.mp4", vec![[39.1, -76.91]]),
            test_route("SavedClips/2026-08-08_20-05-16/2026-08-08_19-57-05-front.mp4", vec![[39.11, -76.92]]),
            test_route("SavedClips/2026-08-08_20-05-16/2026-08-08_19-58-06-front.mp4", vec![[39.12, -76.93]]),
            test_route("SavedClips/2026-08-08_20-05-16/2026-08-08_19-59-06-front.mp4", vec![[39.13, -76.93]]),
            test_route("RecentClips/2026-08-08/2026-08-08_20-05-13-front.mp4", vec![[39.13, -76.932]]),
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 3, "18:11 stub, recovered drive, 20:05 stub");
        assert_eq!(groups[1].len(), 4, "all four SavedClips driving routes form the drive");
        assert!(groups[1].iter().all(|c| c.route.file.starts_with("SavedClips/")));
    }

    #[test]
    fn test_gap_fill_event_only_routes_still_group() {
        // A store whose only routes live under event folders (every
        // RecentClips twin rotated off before ingest) must still produce
        // drives — group_clips used to bail out before the gap-fill pass
        // when no non-event route existed.
        let routes = vec![
            test_route("SavedClips/2026-05-28_14-10-00/2026-05-28_14-00-00-front.mp4", vec![[37.1, -122.1]]),
            test_route("SavedClips/2026-05-28_14-10-00/2026-05-28_14-01-00-front.mp4", vec![[37.11, -122.1]]),
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1, "event-only driving routes must form a drive");
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn test_gap_fill_unanchored_gear_evidence_matches_summary_path() {
        // The drives LIST is built from RouteSummary, which carries
        // gear_runs but NOT gear_states, so its unanchored gate can only
        // ever see gear_runs. The map/overview path walks full Routes and
        // could see gear_states too — so a legacy/imported row with
        // gear_states populated but gear_runs empty (gear_runs was added
        // later and is backfilled) would be admitted by the MAP while the
        // LIST rejected it: a drive visible on the map that does not
        // exist in the list, and a 404 when clicked. Both paths must
        // judge unanchored admission on the same evidence.
        let mut states_only = test_route(
            "SentryClips/2026-05-28_14-05-00/2026-05-28_14-00-00-front.mp4",
            vec![[37.1, -122.1]],
        );
        states_only.gear_runs = Vec::new(); // legacy row: never RLE'd
        states_only.gear_states = vec![1]; // non-Park frames present
        let routes = vec![
            test_route("RecentClips/2026-05-28/2026-05-28_10-00-00-front.mp4", vec![[37.0, -122.0]]),
            states_only,
        ];
        let groups = group_clips(routes);
        assert_eq!(
            groups.len(),
            1,
            "map path must not admit what the summary path cannot see",
        );
        assert!(groups[0][0].route.file.starts_with("RecentClips/"));

        // Prove PARITY, not just the map path's verdict: build the same
        // timeline as summaries (which structurally cannot carry
        // gear_states) and assert both groupers agree on drive count.
        let mut states_only_summary = clip_with_gear_runs(
            "SentryClips/2026-05-28_14-05-00/2026-05-28_14-00-00-front.mp4",
            &[(1, 60)],
            400.0,
        );
        states_only_summary.gear_runs = Vec::new(); // same legacy shape
        states_only_summary.raw_park_count = 0;
        states_only_summary.raw_frame_count = 60;
        let summaries = vec![
            clip_with_gear_runs(
                "RecentClips/2026-05-28/2026-05-28_10-00-00-front.mp4",
                &[(1, 60)],
                500.0,
            ),
            states_only_summary,
        ];
        let summary_groups = group_summary_clips(&summaries);
        assert_eq!(
            summary_groups.len(),
            groups.len(),
            "list and map must agree on unanchored admission: map={} list={}",
            groups.len(),
            summary_groups.len(),
        );
    }

    #[test]
    fn test_gap_fill_unanchored_rejects_raw_count_only_evidence() {
        // A row with NO gear telemetry at all (empty gear runs and
        // states) but raw frame counters implying motion passes the
        // ordinary driving gate via its raw-count fallback. That is not
        // gear PROOF — legacy/imported rows and partial extracts land
        // here — so it must not mint an unanchored drive on its own.
        let mut raw_only = test_route(
            "SentryClips/2026-05-28_14-05-00/2026-05-28_14-00-00-front.mp4",
            vec![[37.1, -122.1]],
        );
        raw_only.gear_states = Vec::new();
        raw_only.gear_runs = Vec::new();
        raw_only.speeds = Vec::new();
        raw_only.raw_park_count = 0;
        raw_only.raw_frame_count = 10;
        let routes = vec![
            test_route("RecentClips/2026-05-28/2026-05-28_10-00-00-front.mp4", vec![[37.0, -122.0]]),
            raw_only,
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1, "raw-count-only event clip must stay unanchored-ineligible");
        assert!(groups[0][0].route.file.starts_with("RecentClips/"));
    }

    #[test]
    fn test_gap_fill_unanchored_requires_gear_evidence() {
        // Unanchored admission demands POSITIVE gear-based SEI evidence.
        // A speed-only outlier (all-Park gear, one bogus speed sample)
        // passes the ordinary driving gate but must NOT mint a drive
        // without an anchor — provenance-weak rows stay anchored.
        let mut speed_only = test_route(
            "SentryClips/2026-05-28_14-05-00/2026-05-28_14-00-00-front.mp4",
            vec![[37.1, -122.1]],
        );
        speed_only.gear_states = vec![GEAR_PARK];
        speed_only.gear_runs = vec![GearRun { gear: GEAR_PARK, frames: 10 }];
        speed_only.raw_park_count = 10;
        speed_only.speeds = vec![2.0];
        let routes = vec![
            test_route("RecentClips/2026-05-28/2026-05-28_10-00-00-front.mp4", vec![[37.0, -122.0]]),
            speed_only,
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1, "speed-only unanchored event clip stays out");
        assert!(groups[0][0].route.file.starts_with("RecentClips/"));
    }

    #[test]
    fn test_select_gap_fill_unanchored_ingest_candidates_stay_out() {
        // The ingest scan proposes candidates with driving == None (no SEI
        // extracted yet). An unanchored cluster of those must stay out —
        // only post-extraction, gear-verified clusters qualify.
        let recent = vec![dts("2026-06-01 10:00:00")];
        let cands = vec![
            GapFillCandidate {
                ts: dts("2026-06-01 14:00:00"),
                file: "SentryClips/2026-06-01_14-05-00/a-front.mp4",
                driving: None,
                gear_driving: false,
            },
            GapFillCandidate {
                ts: dts("2026-06-01 14:01:00"),
                file: "SentryClips/2026-06-01_14-05-00/b-front.mp4",
                driving: None,
                gear_driving: false,
            },
        ];
        assert!(select_gap_fill(&recent, &cands).is_empty());
    }

    #[test]
    fn test_group_summary_clips_unanchored_driving_cluster() {
        // Summary-path parity with group_clips: the drive-list cache is
        // built from RouteSummary rows, so the unanchored driving cluster
        // must surface there too.
        let summaries = vec![
            clip_with_gear_runs("RecentClips/2026-05-28/2026-05-28_10-00-00-front.mp4", &[(1, 60)], 500.0),
            clip_with_gear_runs("SavedClips/2026-05-28_14-10-00/2026-05-28_14-00-00-front.mp4", &[(1, 60)], 400.0),
            clip_with_gear_runs("SavedClips/2026-05-28_14-10-00/2026-05-28_14-01-00-front.mp4", &[(1, 60)], 400.0),
        ];
        let groups = group_summary_clips(&summaries);
        assert_eq!(groups.len(), 2, "summary path must admit the unanchored driving cluster");
        assert_eq!(groups[1].len(), 2);
        assert!(groups[1][0].summary.file.starts_with("SavedClips/"));
    }

    #[test]
    fn test_select_gap_fill_chain_cap_and_hop_window() {
        let recent = vec![dts("2026-06-01 10:00:00")];
        let mk = |s: &str| dts(s);
        let files: Vec<(NaiveDateTime, String)> = vec![
            // Chained trailing clips every ~61s...
            (mk("2026-06-01 10:01:01"), "a"),
            (mk("2026-06-01 10:02:02"), "b"),
            // ...across a real-data intra-chain gap (~152s ≤ 3-min hop)...
            (mk("2026-06-01 10:04:34"), "c"),
            // ...far link still inside the 30-min cap...
            (mk("2026-06-01 10:07:00"), "d"),
            // Beyond a 3-min hop from anything → chain broken.
            (mk("2026-06-01 10:12:00"), "e"),
        ]
        .into_iter()
        .map(|(ts, n)| (ts, format!("SentryClips/2026-06-01_10-20-00/{}-front.mp4", n)))
        .collect();
        // gear_driving stays false here: these candidates model driving
        // verdicts WITHOUT strict gear provenance, so the broken-hop clip
        // can't ride the unanchored-cluster admission and the test keeps
        // exercising pure chain semantics.
        let cands: Vec<GapFillCandidate> = files
            .iter()
            .map(|(ts, f)| GapFillCandidate { ts: *ts, file: f.as_str(), driving: Some(true), gear_driving: false })
            .collect();
        let picked = select_gap_fill(&recent, &cands);
        assert_eq!(picked, vec![0, 1, 2, 3], "chain must stop at the broken hop");

        // Cap: a chain of driving clips can't extend past GAP_FILL_MAX_MS
        // from the nearest RecentClips clip.
        let long: Vec<(NaiveDateTime, String)> = (1..=35)
            .map(|m| {
                (
                    dts("2026-06-01 10:00:00") + chrono::Duration::seconds(m * 61),
                    format!("SentryClips/2026-06-01_11-00-00/clip{:02}-front.mp4", m),
                )
            })
            .collect();
        let cands: Vec<GapFillCandidate> = long
            .iter()
            .map(|(ts, f)| GapFillCandidate { ts: *ts, file: f.as_str(), driving: Some(true), gear_driving: false })
            .collect();
        let picked = select_gap_fill(&recent, &cands);
        assert!(!picked.is_empty());
        for &i in &picked {
            assert!(
                (cands[i].ts - recent[0]).num_milliseconds() <= GAP_FILL_MAX_MS,
                "{} exceeds the chain cap",
                cands[i].file
            );
        }
        assert!(picked.len() < cands.len(), "cap must cut the tail of the chain");
    }

    #[test]
    fn test_select_gap_fill_rejects_recent_twin_and_overlap_dup() {
        // Tesla stamps the Saved/Sentry twin of an occupied segment 0-1 s
        // after the RecentClips original (drive 236: 20:44:50 vs
        // 20:44:51) — a duplicate, never a fill. A second candidate
        // within the dup window of a kept one is an overlapping segment
        // of the same footage — one winner.
        let recent = vec![dts("2026-07-04 20:43:50"), dts("2026-07-04 20:44:50")];
        let cands = vec![
            GapFillCandidate {
                ts: dts("2026-07-04 20:44:51"), // 1s after occupied slot → twin
                file: "SentryClips/e/2026-07-04_20-44-51-front.mp4",
                driving: Some(true),
                gear_driving: true,
            },
            GapFillCandidate {
                ts: dts("2026-07-04 20:45:51"), // real trailing fill
                file: "SentryClips/e/2026-07-04_20-45-51-front.mp4",
                driving: Some(true),
                gear_driving: true,
            },
            GapFillCandidate {
                ts: dts("2026-07-04 20:46:11"), // 20s after the kept fill → overlap dup
                file: "SavedClips/e2/2026-07-04_20-46-11-front.mp4",
                driving: Some(true),
                gear_driving: true,
            },
        ];
        assert_eq!(select_gap_fill(&recent, &cands), vec![1]);
    }

    #[test]
    fn test_select_gap_fill_events_superset_without_sei() {
        // Processor-side scan has no SEI yet: trailing/leading candidates
        // are selected on timestamps alone (the post-extraction driving
        // gate discards the parked ones later); isolated clusters still
        // never qualify.
        let recent = vec![dts("2026-06-01 10:00:00"), dts("2026-06-01 10:01:00")];
        let cands = vec![
            (dts("2026-06-01 10:02:01"), "SentryClips/e/2026-06-01_10-02-01-front.mp4"),
            (dts("2026-06-01 09:58:30"), "SentryClips/e/2026-06-01_09-58-30-front.mp4"),
            (dts("2026-06-01 12:00:00"), "SentryClips/far/2026-06-01_12-00-00-front.mp4"),
        ];
        let mut picked = select_gap_fill_events(&recent, &cands);
        picked.sort();
        assert_eq!(picked, vec![0, 1], "trailing+leading in, isolated out");
    }

    #[test]
    fn test_gap_fill_trailing_clip_park_tail_is_trimmed() {
        // The last admitted trailing clip typically decelerates and parks
        // mid-clip. The gear-state splitter must trim the drive at the
        // Park transition — points from the parked tail don't survive.
        let mut mixed = test_route(
            "SentryClips/2026-07-04_20-55-50/2026-07-04_20-45-51-front.mp4",
            (0..10).map(|i| [37.002 + i as f64 * 1e-4, -122.0]).collect(),
        );
        mixed.gear_runs = vec![
            GearRun { gear: 1, frames: 30 },
            GearRun { gear: GEAR_PARK, frames: 30 },
        ];
        mixed.gear_states = vec![1; 10];
        mixed.speeds = vec![5.0; 10];
        let routes = vec![
            test_route("RecentClips/2026-07-04/2026-07-04_20-44-50-front.mp4", vec![[37.001, -122.0]]),
            mixed,
        ];
        let groups = group_clips(routes);
        assert_eq!(groups.len(), 1);
        let fill = groups[0]
            .iter()
            .find(|c| c.route.file.starts_with("SentryClips/"))
            .expect("trailing fill admitted");
        assert_eq!(
            fill.route.points.len(),
            5,
            "parked half of the trailing clip must be trimmed off"
        );
    }

    #[test]
    fn test_group_summary_clips_trailing_chain() {
        let mk = |file: &str| RouteSummary {
            file: file.to_string(),
            date: "2026-07-04".to_string(),
            raw_park_count: 0,
            raw_frame_count: 60,
            gear_runs: vec![GearRun { gear: 1, frames: 60 }],
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: RouteAggregates::default(),
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        };
        let parked = |file: &str| RouteSummary {
            file: file.to_string(),
            date: "2026-07-04".to_string(),
            raw_park_count: 60,
            raw_frame_count: 60,
            gear_runs: vec![GearRun { gear: GEAR_PARK, frames: 60 }],
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: RouteAggregates::default(),
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        };
        let summaries = vec![
            mk("2026-07-04/2026-07-04_20-43-50-front.mp4"),
            mk("SentryClips/2026-07-04_20-55-50/2026-07-04_20-44-51-front.mp4"),
            mk("SentryClips/2026-07-04_20-55-50/2026-07-04_20-45-51-front.mp4"),
            parked("SentryClips/2026-07-04_20-55-50/2026-07-04_20-46-52-front.mp4"),
        ];
        let groups = group_summary_clips(&summaries);
        assert_eq!(groups.len(), 1);
        let files: Vec<&str> = groups[0].iter().map(|c| c.summary.file.as_str()).collect();
        assert_eq!(
            files,
            vec![
                "2026-07-04/2026-07-04_20-43-50-front.mp4",
                "SentryClips/2026-07-04_20-55-50/2026-07-04_20-44-51-front.mp4",
                "SentryClips/2026-07-04_20-55-50/2026-07-04_20-45-51-front.mp4",
            ]
        );
    }

    // ── Telemetry rollup tests ───────────────────────────────────────
    //
    // Cover the per-drive aggregation logic in `roll_up_telemetry` so
    // future changes to the per-clip semantics don't silently regress
    // the drives-tab badges.

    fn clip_with_telemetry(
        file: &str,
        battery_start: Option<f64>,
        battery_end: Option<f64>,
        interior_min: Option<f64>,
        interior_max: Option<f64>,
        exterior_temp_avg: Option<f64>,
        hvac_runtime_s: Option<i64>,
    ) -> RouteSummary {
        RouteSummary {
            file: file.to_string(),
            date: "2025-01-15".to_string(),
            raw_park_count: 0,
            raw_frame_count: 60,
            gear_runs: vec![GearRun { gear: 1, frames: 60 }],
            flag_runs: Vec::new(),
            ap_runs: Vec::new(),
            aggregates: RouteAggregates::default(),
            source: None,
            external_signature: None,
            telemetry: crate::types::RouteTelemetryAggregates {
                battery_pct_start: battery_start,
                battery_pct_end: battery_end,
                interior_temp_min: interior_min,
                interior_temp_max: interior_max,
                exterior_temp_avg,
                hvac_runtime_s,
                ..Default::default()
            },
        }
    }

    fn ts(offset_min: i64) -> chrono::NaiveDateTime {
        let base = chrono::NaiveDateTime::parse_from_str(
            "2025-01-15T12:00:00",
            "%Y-%m-%dT%H:%M:%S",
        )
        .unwrap();
        base + chrono::Duration::minutes(offset_min)
    }

    #[test]
    fn rollup_empty_drive_returns_all_none() {
        let r = roll_up_telemetry(&[]);
        assert!(r.battery_pct_start.is_none());
        assert!(r.battery_pct_end.is_none());
        assert!(r.battery_pct_used.is_none());
        assert!(r.interior_temp_min_c.is_none());
        assert!(r.interior_temp_max_c.is_none());
        assert!(r.exterior_temp_avg_c.is_none());
        assert!(r.hvac_runtime_s.is_none());
    }

    #[test]
    fn rollup_battery_start_end_derived_from_first_and_last_clips() {
        let s1 = clip_with_telemetry(
            "/cam/2025-01-15_12-00-00-front.mp4",
            Some(80.0), Some(79.5), None, None, None, None,
        );
        let s2 = clip_with_telemetry(
            "/cam/2025-01-15_12-01-00-front.mp4",
            Some(79.5), Some(78.8), None, None, None, None,
        );
        let s3 = clip_with_telemetry(
            "/cam/2025-01-15_12-02-00-front.mp4",
            Some(78.8), Some(78.0), None, None, None, None,
        );
        let clips = vec![
            SubClipSummary::whole(TimedSummary { summary: &s1, timestamp: ts(0), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s2, timestamp: ts(1), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s3, timestamp: ts(2), clip_span_ms: CLIP_DURATION_MS }),
        ];
        let r = roll_up_telemetry(&clips);
        assert_eq!(r.battery_pct_start, Some(80.0));
        assert_eq!(r.battery_pct_end, Some(78.0));
        assert_eq!(r.battery_pct_used, Some(2.0));
    }

    #[test]
    fn rollup_interior_temp_uses_extremes_across_clips() {
        let s1 = clip_with_telemetry(
            "/cam/2025-01-15_12-00-00-front.mp4",
            None, None, Some(18.0), Some(22.0), None, None,
        );
        let s2 = clip_with_telemetry(
            "/cam/2025-01-15_12-01-00-front.mp4",
            None, None, Some(20.0), Some(28.0), None, None,
        );
        let s3 = clip_with_telemetry(
            "/cam/2025-01-15_12-02-00-front.mp4",
            None, None, Some(17.0), Some(25.0), None, None,
        );
        let clips = vec![
            SubClipSummary::whole(TimedSummary { summary: &s1, timestamp: ts(0), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s2, timestamp: ts(1), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s3, timestamp: ts(2), clip_span_ms: CLIP_DURATION_MS }),
        ];
        let r = roll_up_telemetry(&clips);
        assert_eq!(r.interior_temp_min_c, Some(17.0));
        assert_eq!(r.interior_temp_max_c, Some(28.0));
    }

    #[test]
    fn rollup_hvac_runtime_sums_per_clip_estimates() {
        let s1 = clip_with_telemetry(
            "/cam/2025-01-15_12-00-00-front.mp4",
            None, None, None, None, None, Some(30),
        );
        let s2 = clip_with_telemetry(
            "/cam/2025-01-15_12-01-00-front.mp4",
            None, None, None, None, None, Some(60),
        );
        let s3 = clip_with_telemetry(
            "/cam/2025-01-15_12-02-00-front.mp4",
            None, None, None, None, None, Some(45),
        );
        let clips = vec![
            SubClipSummary::whole(TimedSummary { summary: &s1, timestamp: ts(0), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s2, timestamp: ts(1), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s3, timestamp: ts(2), clip_span_ms: CLIP_DURATION_MS }),
        ];
        let r = roll_up_telemetry(&clips);
        assert_eq!(r.hvac_runtime_s, Some(135));
    }

    #[test]
    fn rollup_partial_telemetry_handles_missing_clips() {
        // First clip has battery start, middle clip is bare, last
        // clip has battery end. Verify start/end pick the populated
        // values across the gap rather than the bare clip's None.
        let s1 = clip_with_telemetry(
            "/cam/2025-01-15_12-00-00-front.mp4",
            Some(60.0), None, None, None, None, None,
        );
        let s2 = clip_with_telemetry(
            "/cam/2025-01-15_12-01-00-front.mp4",
            None, None, None, None, None, None,
        );
        let s3 = clip_with_telemetry(
            "/cam/2025-01-15_12-02-00-front.mp4",
            None, Some(55.0), None, None, None, None,
        );
        let clips = vec![
            SubClipSummary::whole(TimedSummary { summary: &s1, timestamp: ts(0), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s2, timestamp: ts(1), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s3, timestamp: ts(2), clip_span_ms: CLIP_DURATION_MS }),
        ];
        let r = roll_up_telemetry(&clips);
        assert_eq!(r.battery_pct_start, Some(60.0));
        assert_eq!(r.battery_pct_end, Some(55.0));
        assert_eq!(r.battery_pct_used, Some(5.0));
    }

    #[test]
    fn rollup_dedupes_per_parent_file_across_sub_clips() {
        // A single parent clip split into two sub-segments should
        // count telemetry once, not twice. (A real-world case is a
        // clip with an internal park gap.)
        let s = clip_with_telemetry(
            "/cam/2025-01-15_12-00-00-front.mp4",
            Some(70.0), Some(69.0), None, None, None, Some(30),
        );
        let clips = vec![
            SubClipSummary::whole(TimedSummary { summary: &s, timestamp: ts(0), clip_span_ms: CLIP_DURATION_MS }),
            SubClipSummary::whole(TimedSummary { summary: &s, timestamp: ts(0), clip_span_ms: CLIP_DURATION_MS }),
        ];
        let r = roll_up_telemetry(&clips);
        assert_eq!(r.hvac_runtime_s, Some(30), "dedupe: hvac counted once, not twice");
        assert_eq!(r.battery_pct_start, Some(70.0));
        assert_eq!(r.battery_pct_end, Some(69.0));
    }

    // ── Summon detection ─────────────────────────────────────────────
    // Test vectors ported from Sentry-Drive's drive-calc.test.js
    // ("Summon detection" section) and grouper.test.js ("summon:"
    // tests). Fixtures mirror real probed footage: the 2026-07-15
    // Actually Smart Summon (two clips, ~29.9 fps) and the 2026-07-28
    // human parking-lot reposition that superficially matches summon on
    // gear/speed but fails every no-human tell.

    // Legacy-shape runs (no per-run max_mps) — these exercise the
    // detector's stats-fallback speed path, mirroring Drive's fixtures.
    fn fr(pairs: &[(u8, u32)]) -> Vec<FlagRun> {
        pairs
            .iter()
            .map(|&(flags, frames)| FlagRun { flags, frames, max_mps: None })
            .collect()
    }

    // Modern-shape runs carrying max_mps — the frame-accurate speed path.
    fn frm(triples: &[(u8, u32, f64)]) -> Vec<FlagRun> {
        triples
            .iter()
            .map(|&(flags, frames, m)| FlagRun { flags, frames, max_mps: Some(m) })
            .collect()
    }

    /// Hazard-signature evidence: no autopilot/gear runs, because that
    /// path never reads them (HW3 has none and still detects).
    fn ev(runs: &[FlagRun], start: u32, end: u32, total: u32) -> SummonClipEvidence<'_> {
        SummonClipEvidence {
            flag_runs: runs,
            ap_runs: &[],
            gear_runs: &[],
            start_frame: start,
            end_frame: end,
            total_frames: total,
        }
    }

    /// Self-Driving-signature evidence: the clip plus its autopilot and
    /// gear RLEs.
    fn ev_ap<'a>(
        runs: &'a [FlagRun],
        ap_runs: &'a [ApRun],
        gear_runs: &'a [GearRun],
        start: u32,
        end: u32,
        total: u32,
    ) -> SummonClipEvidence<'a> {
        SummonClipEvidence {
            flag_runs: runs,
            ap_runs,
            gear_runs,
            start_frame: start,
            end_frame: end,
            total_frames: total,
        }
    }

    /// ApRun list from (ap, frames) pairs.
    fn ar(pairs: &[(u8, u32)]) -> Vec<ApRun> {
        pairs.iter().map(|&(ap, frames)| ApRun { ap, frames }).collect()
    }

    /// GearRun list from (gear, frames) pairs.
    fn gr(pairs: &[(u8, u32)]) -> Vec<GearRun> {
        pairs.iter().map(|&(gear, frames)| GearRun { gear, frames }).collect()
    }

    // Probe shape of 2026-07-15_20-49-54 (start clip): hazards through
    // the P→D shift, a 29 s left-signal run while maneuvering, no pedal
    // bits anywhere.
    fn ass_start_runs() -> Vec<FlagRun> {
        fr(&[(0, 27), (3, 123), (0, 17), (1, 873), (0, 746)])
    }

    // Probe shape of 2026-07-15_20-50-43 (end clip): hazards through the
    // stop and D→P shift at the tail.
    fn ass_end_runs() -> Vec<FlagRun> {
        fr(&[(0, 471), (3, 82)])
    }

    const ASS_MAX_SPEED: f64 = 2.7;
    const ASS_DURATION_MS: i64 = 78_000;

    #[test]
    fn summon_flag_constants_and_thresholds_are_locked() {
        assert_eq!(FLAG_BLINKER_LEFT, 1);
        assert_eq!(FLAG_BLINKER_RIGHT, 2);
        assert_eq!(FLAG_BRAKE, 4);
        assert_eq!(FLAG_ACCEL, 8);
        // 8 mph newer-car summon cap + margin.
        assert_eq!(SUMMON_MAX_SPEED_MPS, 4.5);
        assert_eq!(SUMMON_BOOKEND_SECONDS, 10.0);
        assert_eq!(SUMMON_MAX_DURATION_MS, 600_000);
        assert_eq!(CLIP_DURATION_MS, 60_000);
    }

    #[test]
    fn flag_runs_overlap_require_all_needs_both_bits_in_same_run() {
        const HAZARD: u8 = FLAG_BLINKER_LEFT | FLAG_BLINKER_RIGHT;
        // Hazards: one run carrying both bits.
        assert!(flag_runs_overlap(&fr(&[(3, 10)]), 0, 10, HAZARD, true));
        // A left-only run followed by a right-only run must NOT read as
        // hazards…
        let alternating = fr(&[(1, 10), (2, 10)]);
        assert!(!flag_runs_overlap(&alternating, 0, 20, HAZARD, true));
        // …but require_all=false (any-bit) sees them.
        assert!(flag_runs_overlap(&alternating, 0, 20, HAZARD, false));
    }

    #[test]
    fn flag_runs_overlap_honors_from_to_frame_bounds() {
        let runs = fr(&[
            (3, 60),   // frames 0-59
            (0, 540),  // frames 60-599
            (3, 60),   // frames 600-659
            (0, 1140), // frames 660-1799
        ]);
        assert!(flag_runs_overlap(&runs, 0, 60, 3, true));
        assert!(!flag_runs_overlap(&runs, 60, 600, 3, true));
        // Run ending exactly at `from` is outside the window…
        assert!(!flag_runs_overlap(&runs, 660, 1800, 3, true));
        // …and a window ending exactly at a run start excludes it too.
        assert!(!flag_runs_overlap(&runs, 360, 600, 3, true));
        assert!(flag_runs_overlap(&runs, 360, 601, 3, true));
    }

    // ── Clip span ──
    // Ported from Sentry-Drive's grouper.test.js. A clip is nominally a
    // minute, but a recording that stopped early keeps its frames and
    // loses its duration; the next native clip's start is the ground
    // truth for when it ended.

    /// Route with `n` evenly-spaced points and a single Drive gear run,
    /// shaped like a real clip of `frames` raw SEI frames.
    fn span_route(file: &str, frames: u32, n: usize, lat: f64) -> Route {
        Route {
            file: file.to_string(),
            date: file.split('/').next().unwrap_or("").to_string(),
            points: (0..n).map(|i| [lat + i as f64 * 1e-4, -122.0]).collect(),
            gear_states: vec![1; n],
            autopilot_states: vec![0; n],
            speeds: vec![10.0; n],
            accel_positions: vec![0.0; n],
            raw_park_count: 0,
            raw_frame_count: frames,
            gear_runs: vec![GearRun { gear: 1, frames }],
            ..Default::default()
        }
    }

    #[test]
    fn clip_that_stopped_early_stamps_points_inside_the_drive() {
        // The next clip starts 20 s later, so clip A's recording covered
        // 20 s — its 700 frames are not a minute's worth. Spreading them
        // across a nominal minute pushed points past the drive's own end
        // and made the next clip's first point step backwards, inflating
        // every duration-weighted statistic built from them.
        let a = span_route("2026-08-11/2026-08-11_00-33-32-front.mp4", 700, 20, 37.0);
        let b = span_route("2026-08-11/2026-08-11_00-33-52-front.mp4", 2100, 60, 37.1);

        let drive = build_single_drive_from_clips(&[a, b], 0, &HashMap::new(), None)
            .expect("one drive");
        // Point times are monotonic — the next clip never steps back.
        for w in drive.points.windows(2) {
            assert!(
                w[1][2] >= w[0][2],
                "point steps backwards: {} then {}",
                w[0][2],
                w[1][2]
            );
        }
        let span = drive.points.last().unwrap()[2] - drive.points[0][2];
        assert!(
            span <= drive.duration_ms as f64,
            "point span {span} exceeds duration {}",
            drive.duration_ms
        );
        // 20 s of clip A, then clip B runs to its own nominal end.
        assert_eq!(drive.duration_ms, 80_000);
    }

    #[test]
    fn clip_spans_do_not_stretch_across_a_recording_gap() {
        // Ten minutes apart is not one clip lasting ten minutes; each
        // keeps the nominal minute, and the drive splitter separates
        // them anyway.
        let a = span_route("2026-08-11/2026-08-11_00-33-32-front.mp4", 2100, 60, 37.0);
        let b = span_route("2026-08-11/2026-08-11_00-43-32-front.mp4", 2100, 60, 37.1);
        let groups = group_clips(vec![a, b]);
        assert_eq!(groups.len(), 2, "a ten-minute gap splits the drives");
        for g in &groups {
            assert_eq!(g[0].clip_span_ms, CLIP_DURATION_MS);
        }
    }

    #[test]
    fn clip_spans_only_bounded_by_native_clips() {
        // An imported provider synthesises clips on an exact minute grid
        // and gap-fill invents bridge routes, so neither may declare a
        // minute of real dashcam video to have lasted seconds.
        let t = |m: u32, sec: u32| {
            NaiveDateTime::parse_from_str(
                &format!("2026-08-11T00:{m:02}:{sec:02}"),
                "%Y-%m-%dT%H:%M:%S",
            )
            .unwrap()
        };
        // native, then an import 20 s later, then native 60 s on.
        let entries = vec![(t(0, 0), true), (t(0, 20), false), (t(1, 0), true)];
        let spans = clip_spans(&entries);
        assert_eq!(
            spans[0], CLIP_DURATION_MS,
            "the import must not shorten the native clip before it"
        );
        assert_eq!(spans[1], CLIP_DURATION_MS, "imports keep the nominal minute");
        assert_eq!(spans[2], CLIP_DURATION_MS, "last clip has nothing after it");

        // Two natives 20 s apart: the first really did stop early.
        let entries = vec![(t(0, 0), true), (t(0, 20), true)];
        assert_eq!(clip_spans(&entries)[0], 20_000);
    }

    // ── Park splitting: short segments survive GPS deduplication ──
    // Ported from Sentry-Drive's grouper.test.js (commit "Addresses HW
    // differences in SEI data"). The frame→point mapping rounds both
    // ends of a short segment onto the same index once dedup has thinned
    // the points, which used to delete the segment outright — taking the
    // flag/gear evidence Summon detection reads from it. Whether a
    // maneuver was detected then depended on how much the car had moved.

    #[test]
    fn park_split_keeps_leading_and_middle_segments_at_any_point_density() {
        // Four motion runs separated by park gaps. At six points the
        // leading and middle 30-frame runs both collapse to a single
        // index; only the trailing one was rescued, by the clamp that
        // pulls start_idx back inside the array.
        let runs = gr(&[
            (1, 30),
            (GEAR_PARK, 120),
            (1, 420),
            (GEAR_PARK, 120),
            (1, 30),
            (GEAR_PARK, 120),
            (1, 960),
        ]);

        for n_points in [6usize, 100, 1800] {
            let plan = plan_clip_at_park_gaps(&runs, n_points).expect("clip has park gaps");
            let motion: Vec<&PlannedSeg> = plan.iter().filter(|s| !s.parked).collect();
            assert_eq!(
                motion.len(),
                4,
                "n={n_points}: every motion run must survive the point mapping"
            );
            for seg in &motion {
                assert!(
                    seg.range.end > seg.range.start,
                    "n={n_points}: a motion segment must keep at least one point"
                );
                assert!(seg.range.end <= n_points, "n={n_points}: range stays in bounds");
            }
            // Offsets come from the FRAME bounds, so they do not move
            // with the point count — a one-point segment still reports
            // its true position in the clip.
            let offsets: Vec<i64> = motion.iter().map(|s| s.offset_secs).collect();
            assert_eq!(offsets, vec![0, 5, 23, 28], "n={n_points}");
        }
    }

    #[test]
    fn park_split_drops_segments_only_when_the_clip_has_no_points() {
        // The one case that still skips: nothing to slice at all. The
        // guard must not panic or produce an out-of-range slice.
        let runs = gr(&[(1, 30), (GEAR_PARK, 120), (1, 960)]);
        let plan = plan_clip_at_park_gaps(&runs, 0).expect("clip has a park gap");
        assert!(
            plan.iter().all(|s| s.parked),
            "a point-less clip yields no motion segments to slice"
        );
    }

    // ── Self Driving signature vectors ──
    // Ported from Sentry-Drive's drive-calc.test.js (SD_RUNS /
    // SD_AP_RUNS / SD_GEAR_RUNS / SD_CLIP / SD_STATS).
    fn sd_flag_runs() -> Vec<FlagRun> {
        frm(&[(0, 27, 0.0), (0, 923, 2.7), (0, 100, 0.0)])
    }
    fn sd_ap_runs() -> Vec<ApRun> {
        ar(&[
            (AUTOPILOT_OFF, 27),   // parked, before the maneuver engages
            (AUTOPILOT_FSD, 923),  // Self Driving
            (AUTOPILOT_OFF, 100),  // parked itself
        ])
    }
    /// Park to park: the shape every Summon has, and the shape a
    /// fragment of a longer trip does not.
    fn sd_gear_runs() -> Vec<GearRun> {
        gr(&[(GEAR_PARK, 27), (1, 923), (GEAR_PARK, 100)])
    }
    const SD_DURATION_MS: i64 = 35_000;
    const SD_MAX_SPEED: f64 = 2.7;

    #[test]
    fn detect_summon_self_driving_without_hazards_is_summon() {
        let (f, a, g) = (sd_flag_runs(), sd_ap_runs(), sd_gear_runs());
        assert!(detect_summon(&[ev_ap(&f, &a, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        // The same drive without autopilot evidence has no signature
        // left — there are no hazards anywhere in these flag runs.
        assert!(!detect_summon(&[ev_ap(&f, &[], &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_self_driving_still_obeys_pedal_speed_and_duration_gates() {
        let (a, g) = (sd_ap_runs(), sd_gear_runs());
        let pedal = frm(&[(0, 500, 2.7), (8, 50, 2.0), (0, 500, 1.0)]);
        assert!(!detect_summon(&[ev_ap(&pedal, &a, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        let fast = frm(&[(0, 1050, 12.0)]);
        assert!(!detect_summon(&[ev_ap(&fast, &a, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        let f = sd_flag_runs();
        assert!(!detect_summon(&[ev_ap(&f, &a, &g, 0, 1050, 1050)], SD_MAX_SPEED, 601_000, true));
    }

    #[test]
    fn detect_summon_mostly_off_drive_is_not_self_driving() {
        // A driver rolling through a lot with FSD engaged only briefly:
        // 250 of 1050 frames is under the 50 % share.
        let (f, g) = (sd_flag_runs(), sd_gear_runs());
        let brief = ar(&[(AUTOPILOT_OFF, 800), (AUTOPILOT_FSD, 250)]);
        assert!(!detect_summon(&[ev_ap(&f, &brief, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_exact_half_fsd_share_passes() {
        // The share boundary: `fsd >= ap * 0.5` in Drive, `fsd*2 >= ap`
        // here — 525 of 1050 must pass in both.
        let (f, g) = (sd_flag_runs(), sd_gear_runs());
        let half = ar(&[(AUTOPILOT_OFF, 525), (AUTOPILOT_FSD, 525)]);
        assert!(detect_summon(&[ev_ap(&f, &half, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        let under = ar(&[(AUTOPILOT_OFF, 526), (AUTOPILOT_FSD, 524)]);
        assert!(!detect_summon(&[ev_ap(&f, &under, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_autosteer_or_tacc_rules_out_self_driving() {
        let (f, g) = (sd_flag_runs(), sd_gear_runs());
        let (start, end) = (ass_start_runs(), ass_end_runs());
        for ap in [AUTOPILOT_AUTOSTEER, AUTOPILOT_TACC] {
            let assisted = ar(&[(AUTOPILOT_FSD, 900), (ap, 150)]);
            assert!(!detect_summon(&[ev_ap(&f, &assisted, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
            // Hazard bookends are decided FIRST and stand on their own,
            // so hardware reporting the maneuver as some other assisted
            // mode cannot un-detect it. Gate order, not an accident.
            let hazard_ap = ar(&[(ap, 1786)]);
            let clips = vec![ev_ap(&start, &hazard_ap, &[], 0, 1786, 1786), ev(&end, 0, 553, 553)];
            assert!(detect_summon(&clips, ASS_MAX_SPEED, ASS_DURATION_MS, true));
        }
    }

    #[test]
    fn detect_summon_autopilot_evidence_required_on_every_clip() {
        let (f, a, g) = (sd_flag_runs(), sd_ap_runs(), sd_gear_runs());
        let clips = vec![
            ev_ap(&f, &a, &g, 0, 1050, 1050),
            ev_ap(&f, &[], &g, 0, 1050, 1050),
        ];
        assert!(!detect_summon(&clips, SD_MAX_SPEED, SD_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_park_split_bounds_gate_the_self_driving_share() {
        // Only the first segment is the maneuver; the rest of the clip
        // is the manual drive that followed it.
        let flag_runs = frm(&[(0, 1800, 2.7)]);
        let ap_runs = ar(&[(AUTOPILOT_FSD, 600), (AUTOPILOT_OFF, 1200)]);
        let gear_runs = gr(&[(GEAR_PARK, 30), (1, 600), (GEAR_PARK, 1170)]);
        assert!(detect_summon(&[ev_ap(&flag_runs, &ap_runs, &gear_runs, 0, 660, 1800)], SD_MAX_SPEED, SD_DURATION_MS, true));
        assert!(!detect_summon(&[ev_ap(&flag_runs, &ap_runs, &gear_runs, 660, 1800, 1800)], SD_MAX_SPEED, SD_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_self_driving_requires_park_bracketing() {
        // A fragment of a longer FSD trip: an unfilled clip gap cut it
        // out, so it begins and ends mid-motion. Pedal-free (the car is
        // driving) and slow (a parking structure) — everything else the
        // fallback asks for. Without this gate the fragment is tagged
        // Summon and, because a detected Summon zeroes its own FSD
        // stats, a genuine FSD drive is ERASED from lifetime numbers.
        let f = sd_flag_runs();
        let rolling_ap = ar(&[(AUTOPILOT_FSD, 1050)]);
        let rolling_gear = gr(&[(1, 1050)]);
        assert!(!detect_summon(&[ev_ap(&f, &rolling_ap, &rolling_gear, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        // Ends in Park but starts mid-motion — still a fragment.
        let half_parked = gr(&[(1, 950), (GEAR_PARK, 100)]);
        assert!(!detect_summon(&[ev_ap(&f, &rolling_ap, &half_parked, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        // Missing gear evidence reads as not bracketed.
        let a = sd_ap_runs();
        assert!(!detect_summon(&[ev_ap(&f, &a, &[], 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        // Hazard bookends never needed gear evidence and still do not.
        let (start, end) = (ass_start_runs(), ass_end_runs());
        assert!(detect_summon(&[ev(&start, 0, 1786, 1786), ev(&end, 0, 553, 553)], ASS_MAX_SPEED, ASS_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_self_driving_rejects_runs_from_another_index_space() {
        // Runs that don't span the clip's raw frames came from a
        // different index space (the classic bug: building them from
        // the DEDUPED point arrays). Walking them with raw-frame bounds
        // reads the wrong frames, so such evidence is treated as absent.
        let (f, a, g) = (sd_flag_runs(), sd_ap_runs(), sd_gear_runs());
        let deduped_gears = gr(&[(GEAR_PARK, 3), (1, 40), (GEAR_PARK, 5)]);
        assert!(!detect_summon(&[ev_ap(&f, &a, &deduped_gears, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        let deduped_ap = ar(&[(AUTOPILOT_FSD, 48)]);
        assert!(!detect_summon(&[ev_ap(&f, &deduped_ap, &g, 0, 1050, 1050)], SD_MAX_SPEED, SD_DURATION_MS, true));
        // The hazard signature reads flag_runs only and is unaffected.
        let (start, end) = (ass_start_runs(), ass_end_runs());
        let clips = vec![ev_ap(&start, &[], &deduped_gears, 0, 1786, 1786), ev(&end, 0, 553, 553)];
        assert!(detect_summon(&clips, ASS_MAX_SPEED, ASS_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_flag_runs_must_span_the_clip_too() {
        // The span rule now guards the SHARED vetoes: bookend windows
        // are sized from total_frames but walked over flag_runs
        // offsets, and that path returns true — failure-open.
        let short = fr(&[(3, 100)]);
        assert!(!detect_summon(&[ev(&short, 0, 1786, 1786)], 2.7, 35_000, true));
        // …and end_frame past total_frames is rejected outright.
        let (start, end) = (ass_start_runs(), ass_end_runs());
        assert!(!detect_summon(
            &[ev(&start, 0, 1900, 1786), ev(&end, 0, 553, 553)],
            ASS_MAX_SPEED,
            ASS_DURATION_MS,
            true
        ));
    }

    #[test]
    fn gear_at_frame_and_segment_bounded_by_park() {
        let runs = gr(&[(GEAR_PARK, 30), (1, 600), (GEAR_PARK, 1170)]);
        assert_eq!(gear_at_frame(&runs, 0), Some(GEAR_PARK));
        assert_eq!(gear_at_frame(&runs, 29), Some(GEAR_PARK));
        assert_eq!(gear_at_frame(&runs, 30), Some(1));
        assert_eq!(gear_at_frame(&runs, 629), Some(1));
        assert_eq!(gear_at_frame(&runs, 630), Some(GEAR_PARK));
        assert_eq!(gear_at_frame(&runs, 1800), None, "past the evidence is unknown");
        assert_eq!(gear_at_frame(&[], 0), None);

        let f = sd_flag_runs();
        // Park-split segment: the frames either side belong to park runs.
        let split = ev_ap(&f, &[], &runs, 30, 630, 1800);
        assert!(segment_bounded_by_park(&split, false));
        assert!(segment_bounded_by_park(&split, true));
        // Unsplit clip opening and closing parked: the boundary frames
        // themselves are read.
        let whole = ev_ap(&f, &[], &runs, 0, 1800, 1800);
        assert!(segment_bounded_by_park(&whole, false));
        assert!(segment_bounded_by_park(&whole, true));
        // Rolling at both ends.
        let rolling_gear = gr(&[(1, 1800)]);
        let rolling = ev_ap(&f, &[], &rolling_gear, 0, 1800, 1800);
        assert!(!segment_bounded_by_park(&rolling, false));
        assert!(!segment_bounded_by_park(&rolling, true));
    }

    #[test]
    fn segment_ap_frames_counts_overlap_by_mode() {
        let f = sd_flag_runs();
        let ap = ar(&[(AUTOPILOT_OFF, 100), (AUTOPILOT_FSD, 400), (AUTOPILOT_AUTOSTEER, 500)]);
        let full = segment_ap_frames(&ev_ap(&f, &ap, &[], 0, 500, 1000)).unwrap();
        assert_eq!((full.fsd, full.other, full.total), (400, 0, 500));
        // A window straddling the FSD→Autosteer boundary counts the
        // partial overlap of each run.
        let straddle = segment_ap_frames(&ev_ap(&f, &ap, &[], 300, 700, 1000)).unwrap();
        assert_eq!((straddle.fsd, straddle.other, straddle.total), (200, 200, 400));
        // Absent evidence, and a window past the end of it, are both
        // "unknown" rather than "no autopilot".
        assert!(segment_ap_frames(&ev_ap(&f, &[], &[], 0, 500, 1000)).is_none());
        assert!(segment_ap_frames(&ev_ap(&f, &ap, &[], 1000, 1200, 1200)).is_none());
    }

    // ── Golden vectors: the two real maneuvers, one per hardware gen ──
    // Ported from Sentry-Drive's drive-calc.test.js. Between them they
    // prove why BOTH signatures are load-bearing: neither detects both
    // clips, and requiring both would detect neither.

    /// Newer hardware, clip 2026-08-14_21-55-07 — 919 raw frames,
    /// reverse then forward. Hazards ARE flashed at both ends, but the
    /// closing flash starts 4 frames AFTER the D→P shift at frame 835,
    /// so the park splitter's segment [0, 835) excludes it. The hazard
    /// signature fails; only Self Driving detects this drive.
    fn sd_real_clip() -> (Vec<FlagRun>, Vec<ApRun>, Vec<GearRun>) {
        (
            frm(&[
                (3, 44, 0.0),   // hazards, frames 0-43
                (0, 795, 2.0),
                (3, 70, 0.0),   // hazards, frames 839-908 — after Park
                (0, 10, 0.0),
            ]),
            ar(&[(AUTOPILOT_FSD, 894), (AUTOPILOT_OFF, 25)]),
            gr(&[
                (GEAR_PARK, 25),
                (2, 296), // R
                (1, 514), // D
                (GEAR_PARK, 84), // splits here, at frame 835
            ]),
        )
    }
    const SD_REAL_MAX_SPEED: f64 = 2.0;
    const SD_REAL_DURATION_MS: i64 = 54_500;

    /// HW3, clip 2026-08-15_00-33-27 — 1214 raw frames, forward only.
    /// HW3 reports autopilot_state = Off for the WHOLE maneuver, so the
    /// Self Driving signature does not exist here — but the hazards
    /// bookend properly, the closing flash starting 93 frames BEFORE the
    /// D→P shift and spanning it.
    fn hw3_real_clip() -> (Vec<FlagRun>, Vec<ApRun>, Vec<GearRun>) {
        (
            frm(&[
                (0, 14, 0.0),
                (3, 144, 0.3),  // hazards, frames 14-157
                (0, 15, 0.4),
                (1, 566, 2.2),  // a normal left signal mid-summon
                (0, 335, 2.2),
                (3, 140, 1.9),  // hazards, frames 1074-1213
            ]),
            ar(&[(AUTOPILOT_OFF, 1214)]),
            gr(&[(GEAR_PARK, 60), (1, 1107), (GEAR_PARK, 47)]), // splits at 1167
        )
    }
    const HW3_REAL_MAX_SPEED: f64 = 2.2;
    const HW3_REAL_DURATION_MS: i64 = 54_700;

    #[test]
    fn golden_new_hw_summon_whose_closing_hazards_fall_past_the_park_split() {
        let (f, a, g) = sd_real_clip();
        let clip = ev_ap(&f, &a, &g, 0, 835, 919);
        assert!(detect_summon(
            &[clip],
            SD_REAL_MAX_SPEED,
            SD_REAL_DURATION_MS,
            true
        ));

        const HAZARD: u8 = FLAG_BLINKER_LEFT | FLAG_BLINKER_RIGHT;
        let bookend = (919u64 * 10 * 1000).div_ceil(CLIP_DURATION_MS as u64) as u32;
        assert!(
            flag_runs_overlap(&f, 0, bookend, HAZARD, true),
            "opening hazards are inside the segment"
        );
        // The closing flash begins 4 frames after the shift into Park at
        // 835, so it is outside the drive's own segment — only the
        // window's reach past the boundary finds it.
        assert!(
            !flag_runs_overlap(&f, 835 - bookend, 835, HAZARD, true),
            "not inside the segment"
        );
        assert!(
            flag_runs_overlap(&f, 835 - bookend, 919u32.min(835 + bookend), HAZARD, true),
            "the widened window reaches it"
        );

        // Both signatures carry this clip independently. Strip the
        // hazards and autopilot state alone still identifies the
        // maneuver…
        let no_hazards = frm(&[(0, 919, 2.0)]);
        assert!(detect_summon(
            &[ev_ap(&no_hazards, &a, &g, 0, 835, 919)],
            SD_REAL_MAX_SPEED,
            SD_REAL_DURATION_MS,
            true
        ));
        // …and with neither, it is not a Summon.
        let all_off = ar(&[(AUTOPILOT_OFF, 919)]);
        assert!(!detect_summon(
            &[ev_ap(&no_hazards, &all_off, &g, 0, 835, 919)],
            SD_REAL_MAX_SPEED,
            SD_REAL_DURATION_MS,
            true
        ));
    }

    /// Real HW3 summon whose opening flash falls INSIDE the leading Park
    /// run (clips 2026-08-11_00-33-32 and _00-33-51). The splitter starts
    /// the drive at the shift out of Park, frame 210, while the hazards
    /// ran 22-139 — so the opening bookend has to look back past the
    /// segment boundary to see them. The second clip carries the mirror
    /// detail: the brake run at 683-750 is the owner returning to the car
    /// AFTER the maneuver parked, which is why the pedal veto must stay
    /// inside the segment.
    #[test]
    fn golden_hazards_in_the_park_run_bracketing_the_maneuver_still_count() {
        let a_flags = frm(&[
            (0, 22, 0.0),
            (3, 118, 0.0),  // hazards, frames 22-139, all in Park
            (0, 644, 1.8),
        ]);
        let a_ap = ar(&[(AUTOPILOT_OFF, 784)]);
        let a_gear = gr(&[(GEAR_PARK, 210), (2, 430), (1, 144)]);

        let b_flags = frm(&[
            (0, 546, 2.8),
            (3, 137, 1.7),  // hazards, 546-682, spanning the shift into Park
            (4, 68, 0.0),   // the owner's brake press, after the maneuver
            (0, 8, 0.0),
            (8, 450, 4.5),
            (0, 185, 1.5),
            (8, 35, 0.9),
            (0, 12, 1.0),
            (8, 103, 1.7),
            (0, 183, 1.7),
            (8, 269, 7.2),
        ]);
        let b_ap = ar(&[(AUTOPILOT_OFF, 1996)]);
        let b_gear = gr(&[(1, 636), (GEAR_PARK, 105), (1, 1255)]);

        let clips = vec![
            ev_ap(&a_flags, &a_ap, &a_gear, 210, 784, 784),
            ev_ap(&b_flags, &b_ap, &b_gear, 0, 636, 1996),
        ];
        assert!(detect_summon(&clips, 2.8, 22_000, true));

        // The opening flash is entirely outside the drive's own segment,
        // so a window anchored at the segment boundary cannot see it.
        const HAZARD: u8 = FLAG_BLINKER_LEFT | FLAG_BLINKER_RIGHT;
        let bookend = (784u64 * 10 * 1000).div_ceil(CLIP_DURATION_MS as u64) as u32;
        assert!(!flag_runs_overlap(&a_flags, 210, 210 + bookend, HAZARD, true));
        assert!(flag_runs_overlap(&a_flags, 210 - bookend, 210 + bookend, HAZARD, true));

        // And the owner's brake press sits in the parked frames after the
        // maneuver, where the pedal veto must not look.
        const PEDAL: u8 = FLAG_BRAKE | FLAG_ACCEL;
        assert!(!flag_runs_overlap(&b_flags, 0, 636, PEDAL, false));
        assert!(flag_runs_overlap(&b_flags, 0, 969, PEDAL, false));
    }

    #[test]
    fn detect_summon_unrepresented_wall_clock_disqualifies() {
        // Every other gate only sees the frames the clips cover, so a
        // drive whose middle is missing from RecentClips is an ordinary
        // trip wearing a maneuver's ends: slow, pedal-free and
        // park-bracketed at both visible ends.
        let departure_flags = frm(&[(3, 300, 2.0), (0, 600, 2.0)]);
        let departure_gear = gr(&[(GEAR_PARK, 60), (1, 840)]);
        let arrival_flags = frm(&[(0, 600, 2.0), (3, 300, 2.0)]);
        let arrival_gear = gr(&[(1, 840), (GEAR_PARK, 60)]);
        let ap = ar(&[(AUTOPILOT_FSD, 900)]);

        let clips = || {
            vec![
                ev_ap(&departure_flags, &ap, &departure_gear, 0, 900, 900),
                ev_ap(&arrival_flags, &ap, &arrival_gear, 0, 900, 900),
            ]
        };
        // Two minutes of clips accounting for a four-minute drive.
        assert!(!detect_summon(&clips(), 2.0, 240_000, true));
        // The same evidence for a drive its own length.
        assert!(detect_summon(&clips(), 2.0, 120_000, true));

        // Both real maneuvers sit well inside the tolerance.
        let (f, a, g) = sd_real_clip();
        assert!(detect_summon(
            &[ev_ap(&f, &a, &g, 0, 835, 919)],
            SD_REAL_MAX_SPEED,
            SD_REAL_DURATION_MS,
            true
        ));
        let (f, a, g) = hw3_real_clip();
        assert!(detect_summon(
            &[ev_ap(&f, &a, &g, 60, 1167, 1214)],
            HW3_REAL_MAX_SPEED,
            HW3_REAL_DURATION_MS,
            true
        ));
    }

    #[test]
    fn detect_summon_bookend_window_is_ten_seconds_at_each_end_not_the_whole_drive() {
        // Hazards in the MIDDLE of a slow pedal-free drive are a human
        // idling with the flashers on, not a maneuver. Widening the
        // window to the whole drive would tag them.
        let gear = gr(&[(GEAR_PARK, 60), (1, 780), (GEAR_PARK, 60)]);
        let ap = ar(&[(AUTOPILOT_OFF, 900)]);
        let middle_only = frm(&[
            (0, 350, 2.0),
            (3, 100, 2.0), // hazards only at 350-449
            (0, 450, 2.0),
        ]);
        assert!(!detect_summon(
            &[ev_ap(&middle_only, &ap, &gear, 0, 900, 900)],
            2.0,
            60_000,
            true
        ));

        // 900 frames over a 60 s clip puts the window at 150 frames.
        // Hazards ending one frame inside it qualify; one frame outside
        // it do not.
        // An empty run is not an RLE, so zero-length gaps are dropped.
        let at_edge = |start: u32, len: u32| -> Vec<FlagRun> {
            let mut out: Vec<(u8, u32, f64)> = vec![(3, 150, 2.0)]; // opening bookend
            if start > 150 {
                out.push((0, start - 150, 2.0));
            }
            out.push((3, len, 2.0));
            if 900 - start - len > 0 {
                out.push((0, 900 - start - len, 2.0));
            }
            frm(&out)
        };
        let inside = at_edge(750, 150);
        assert!(
            detect_summon(&[ev_ap(&inside, &ap, &gear, 0, 900, 900)], 2.0, 60_000, true),
            "closing hazards inside the window"
        );
        let outside = at_edge(700, 50);
        assert!(
            !detect_summon(&[ev_ap(&outside, &ap, &gear, 0, 900, 900)], 2.0, 60_000, true),
            "closing hazards end before it"
        );
    }

    #[test]
    fn golden_hw3_summon_reports_off_and_detects_on_hazards_alone() {
        let (f, a, g) = hw3_real_clip();
        assert!(detect_summon(
            &[ev_ap(&f, &a, &g, 60, 1167, 1214)],
            HW3_REAL_MAX_SPEED,
            HW3_REAL_DURATION_MS,
            true
        ));

        // No FSD frames anywhere: the Self Driving signature cannot
        // carry this drive, so the hazard path must stay decisive alone.
        let ap = segment_ap_frames(&ev_ap(&f, &a, &g, 60, 1167, 1214)).unwrap();
        assert_eq!((ap.fsd, ap.other, ap.total), (0, 0, 1107));

        // Strip the hazards and it must come back undetected. The
        // 566-frame left signal survives: a lone blinker is not a
        // hazard, and its presence must not disqualify the drive.
        let no_hazards = frm(&[(0, 648, 2.2), (1, 566, 2.2)]);
        assert!(!detect_summon(
            &[ev_ap(&no_hazards, &a, &g, 60, 1167, 1214)],
            HW3_REAL_MAX_SPEED,
            HW3_REAL_DURATION_MS,
            true
        ));
    }

    #[test]
    fn golden_vectors_survive_the_full_summary_path() {
        // The same two maneuvers end to end through the grouper, so the
        // evidence plumbing (park split → frame bounds → SummonClipEvidence)
        // is exercised, not just the detector.
        let (f, a, g) = sd_real_clip();
        let gear_pairs: Vec<(u8, u32)> = g.iter().map(|r| (r.gear, r.frames)).collect();
        let ap_pairs: Vec<(u8, u32)> = a.iter().map(|r| (r.ap, r.frames)).collect();
        let clip = summon_summary_ap(
            "2026-08-14/2026-08-14_21-55-07-front.mp4",
            &gear_pairs,
            f,
            &ap_pairs,
            Some(SD_REAL_MAX_SPEED),
        );
        let drives = group_summaries_fast(&[clip], &HashMap::new());
        assert_eq!(drives.len(), 1);
        assert!(drives[0].summon, "newer-HW maneuver detected via Self Driving");

        let (f, a, g) = hw3_real_clip();
        let gear_pairs: Vec<(u8, u32)> = g.iter().map(|r| (r.gear, r.frames)).collect();
        let ap_pairs: Vec<(u8, u32)> = a.iter().map(|r| (r.ap, r.frames)).collect();
        let clip = summon_summary_ap(
            "2026-08-15/2026-08-15_00-33-27-front.mp4",
            &gear_pairs,
            f,
            &ap_pairs,
            Some(HW3_REAL_MAX_SPEED),
        );
        let drives = group_summaries_fast(&[clip], &HashMap::new());
        assert_eq!(drives.len(), 1);
        assert!(drives[0].summon, "HW3 maneuver detected via hazards");
        // Driverless either way: the assistance stats are zeroed even
        // though HW3 reported autopilot Off the whole time.
        assert_eq!(drives[0].fsd_percent, 0.0);
    }

    #[test]
    fn detect_summon_real_ass_two_clip_shape_is_summon() {
        let (start, end) = (ass_start_runs(), ass_end_runs());
        let clips = vec![ev(&start, 0, 1786, 1786), ev(&end, 0, 553, 553)];
        assert!(detect_summon(&clips, ASS_MAX_SPEED, ASS_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_single_clip_dumb_summon_shape_is_summon() {
        let runs = fr(&[(3, 90), (0, 300), (3, 60)]);
        let clips = vec![ev(&runs, 0, 450, 450)];
        assert!(detect_summon(&clips, 1.0, 15_000, true));
    }

    #[test]
    fn detect_summon_pedal_input_anywhere_disqualifies() {
        // 2026-07-28_21-05-46 shape: brake to shift, accel to creep, no
        // hazards.
        let human = fr(&[(0, 1200), (4, 12), (8, 230), (4, 40), (0, 674)]);
        let clips = vec![ev(&human, 0, 2156, 2156)];
        assert!(!detect_summon(&clips, 1.3, 10_000, true));
        // Even WITH hazard bookends, a single accel frame anywhere kills
        // it.
        let hazards_but_pedal = fr(&[(3, 90), (0, 100), (8, 1), (0, 199), (3, 60)]);
        let clips = vec![ev(&hazards_but_pedal, 0, 450, 450)];
        assert!(!detect_summon(&clips, 1.0, 15_000, true));
    }

    #[test]
    fn detect_summon_hazards_must_bookend_both_ends() {
        let start = ass_start_runs();
        let no_hazard_end = fr(&[(0, 553)]);
        let clips = vec![ev(&start, 0, 1786, 1786), ev(&no_hazard_end, 0, 553, 553)];
        assert!(!detect_summon(&clips, ASS_MAX_SPEED, ASS_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_individual_turn_signals_never_read_as_hazards() {
        let runs = fr(&[
            (1, 90),  // left only at start
            (0, 300),
            (2, 60),  // right only at end
        ]);
        let clips = vec![ev(&runs, 0, 450, 450)];
        assert!(!detect_summon(&clips, 1.0, 15_000, true));
    }

    #[test]
    fn detect_summon_speed_duration_and_sei_speed_gates() {
        let (start, end) = (ass_start_runs(), ass_end_runs());
        let clips = vec![ev(&start, 0, 1786, 1786), ev(&end, 0, 553, 553)];
        assert!(!detect_summon(&clips, 5.0, ASS_DURATION_MS, true));
        assert!(!detect_summon(&clips, 0.0, ASS_DURATION_MS, true));
        assert!(!detect_summon(&clips, ASS_MAX_SPEED, 601_000, true));
        assert!(!detect_summon(&clips, ASS_MAX_SPEED, ASS_DURATION_MS, false));
    }

    #[test]
    fn detect_summon_any_clip_without_flag_runs_makes_drive_unverifiable() {
        let start = ass_start_runs();
        let legacy: Vec<FlagRun> = Vec::new();
        let clips = vec![ev(&start, 0, 1786, 1786), ev(&legacy, 0, 553, 553)];
        assert!(!detect_summon(&clips, ASS_MAX_SPEED, ASS_DURATION_MS, true));
        assert!(!detect_summon(&[], ASS_MAX_SPEED, ASS_DURATION_MS, true));
    }

    #[test]
    fn detect_summon_park_split_segment_bounds_gate_the_bookend_windows() {
        // Full clip has hazards at frames 0-59 and 600-659; a 2 s park
        // gap split the clip at frame 660. The FIRST segment sees both
        // hazard windows; the SECOND segment (frames 660+) contains none
        // and must not inherit them.
        let runs = fr(&[(3, 60), (0, 540), (3, 60), (0, 1140)]);
        let first = vec![ev(&runs, 0, 660, 1800)];
        let second = vec![ev(&runs, 660, 1800, 1800)];
        assert!(detect_summon(&first, 1.5, 22_000, true));
        assert!(!detect_summon(&second, 1.5, 22_000, true));
    }

    #[test]
    fn segment_max_speed_frame_space_max_over_segment_null_on_legacy_runs() {
        // Ported from drive-calc.test.js "segmentMaxSpeed: frame-space
        // max over the segment, null on legacy runs".
        let runs = frm(&[(3, 100, 0.5), (0, 400, 2.7), (8, 500, 4.7)]);
        // Segment covering only the first two runs never sees the fast
        // run.
        assert_eq!(segment_max_speed(&ev(&runs, 0, 500, 1000)), Some(2.7));
        // Full clip includes it.
        assert_eq!(segment_max_speed(&ev(&runs, 0, 1000, 1000)), Some(4.7));
        // A run merely straddling the segment boundary still
        // contributes.
        assert_eq!(segment_max_speed(&ev(&runs, 450, 550, 1000)), Some(4.7));
        // Any overlapping run without max_mps makes the answer
        // unknowable.
        let legacy = vec![
            FlagRun { flags: 3, frames: 100, max_mps: None },
            FlagRun { flags: 0, frames: 100, max_mps: Some(1.0) },
        ];
        assert_eq!(segment_max_speed(&ev(&legacy, 0, 200, 200)), None);
        // …but only if it actually overlaps.
        assert_eq!(segment_max_speed(&ev(&legacy, 100, 200, 200)), Some(1.0));
    }

    #[test]
    fn detect_summon_frame_space_speed_evidence_overrides_polluted_drive_stats() {
        // Ported from drive-calc.test.js. Real 2026-07-27 00:34 failure:
        // the park splitter's fraction→point slice overshoots on deduped
        // points, so the summon drive's stats inherited the following
        // drive's 4.05 m/s samples. Per-run max_mps confines the gate to
        // the summon's own frames.
        let clip_a_runs = frm(&[(0, 7, 0.0), (3, 144, 0.1), (0, 766, 2.7)]);
        let clip_b_runs = frm(&[
            (0, 143, 2.7),
            (3, 219, 2.6),
            (0, 105, 2.0),
            (3, 191, 1.8),
            (4, 50, 0.0),
            (0, 10, 0.0),
            (8, 521, 4.7),
            (0, 16, 4.0),
            (8, 545, 4.7),
            (0, 55, 1.0),
        ]);
        let clips = vec![
            ev(&clip_a_runs, 0, 917, 917),
            ev(&clip_b_runs, 0, 579, 1855),
        ];
        // Polluted stats say 4.05 m/s — over the cap — yet the drive is
        // summon.
        assert!(detect_summon(&clips, 4.05, 42_000, true));
        // The inverse guard: fast frames INSIDE the segment still reject
        // on speed alone (hazard bookends present, zero pedal bits),
        // even when the sliced stats happen to look slow.
        let fast_no_pedals = frm(&[(3, 100, 0.5), (0, 700, 4.7), (3, 100, 0.3)]);
        let fast_clips = vec![ev(&fast_no_pedals, 0, 900, 900)];
        assert!(!detect_summon(&fast_clips, 2.0, 42_000, true));
    }

    // ── End-to-end through the summary grouper (grouper.test.js port) ──

    /// RouteSummary with the raw-frame RLEs + SEI abs-max speed the way
    /// the v16 extractor writes them: flag totals match gear totals
    /// exactly (same raw SEI frame sequence). Takes prebuilt flag runs
    /// so fixtures can be legacy-shape (`fr`, stats-fallback speed path)
    /// or modern (`frm`, frame-accurate per-run maxima).
    fn summon_summary_runs(
        file: &str,
        gear_runs: &[(u8, u32)],
        flag_runs: Vec<FlagRun>,
        sei_speed_abs_max: Option<f64>,
    ) -> RouteSummary {
        let aggregates = RouteAggregates {
            sei_speed_abs_max,
            ..Default::default()
        };
        RouteSummary {
            file: file.to_string(),
            date: "2026-07-15".to_string(),
            raw_park_count: gear_runs
                .iter()
                .filter(|(g, _)| *g == GEAR_PARK)
                .map(|(_, f)| f)
                .sum(),
            raw_frame_count: gear_runs.iter().map(|(_, f)| *f).sum(),
            gear_runs: gear_runs.iter().map(|&(gear, frames)| GearRun { gear, frames }).collect(),
            flag_runs,
            // Hazard-signature fixtures carry no autopilot evidence;
            // Self Driving fixtures use `summon_summary_ap`.
            ap_runs: Vec::new(),
            aggregates,
            source: None,
            external_signature: None,
            telemetry: Default::default(),
        }
    }

    /// v21-shaped summary: adds the autopilot RLE. Frame totals across
    /// gear/flag/ap runs must agree — that IS the span check the
    /// detector enforces, so a fixture that disagrees is the bug.
    fn summon_summary_ap(
        file: &str,
        gear_runs: &[(u8, u32)],
        flag_runs: Vec<FlagRun>,
        ap_runs: &[(u8, u32)],
        sei_speed_abs_max: Option<f64>,
    ) -> RouteSummary {
        let mut s = summon_summary_runs(file, gear_runs, flag_runs, sei_speed_abs_max);
        s.ap_runs = ar(ap_runs);
        s
    }

    fn summon_summary(
        file: &str,
        gear_runs: &[(u8, u32)],
        flag_runs: &[(u8, u32)],
        sei_speed_abs_max: Option<f64>,
    ) -> RouteSummary {
        summon_summary_runs(file, gear_runs, fr(flag_runs), sei_speed_abs_max)
    }

    #[test]
    fn summon_hazard_bookended_pedal_free_crawl_across_two_clips_is_flagged() {
        // The real 2026-07-15 ASS shape: leading Park trimmed by the
        // splitter on clip A, trailing Park on clip B (the run the
        // pre-fix extractor dropped — without it these clips fuse with
        // the following drive and the flag is lost).
        let clip_a = summon_summary(
            "2026-07-15/2026-07-15_20-49-54-front.mp4",
            &[(GEAR_PARK, 60), (1, 1726)],
            &[(0, 27), (3, 123), (0, 17), (1, 873), (0, 746)],
            Some(2.7),
        );
        let clip_b = summon_summary(
            "2026-07-15/2026-07-15_20-50-43-front.mp4",
            &[(1, 500), (GEAR_PARK, 53)],
            &[(0, 471), (3, 82)],
            Some(2.7),
        );
        let drives = group_summaries_fast(&[clip_a, clip_b], &HashMap::new());
        assert_eq!(drives.len(), 1);
        assert!(drives[0].summon);
        // Serialized shape matches Sentry-Drive: key present iff true.
        let json = serde_json::to_string(&drives[0]).unwrap();
        assert!(json.contains(r#""summon":true"#), "json: {json}");
    }

    #[test]
    fn summon_missing_flag_runs_or_pedal_input_never_flags() {
        // Same drive shape without flag evidence (pre-flags extraction)
        // — the summary must omit the flag entirely rather than guess.
        let bare_a = summon_summary(
            "2026-07-15/2026-07-15_20-49-54-front.mp4",
            &[(GEAR_PARK, 60), (1, 1726)],
            &[],
            Some(2.7),
        );
        let bare_b = summon_summary(
            "2026-07-15/2026-07-15_20-50-43-front.mp4",
            &[(1, 500), (GEAR_PARK, 53)],
            &[],
            Some(2.7),
        );
        let bare_drives = group_summaries_fast(&[bare_a, bare_b], &HashMap::new());
        assert_eq!(bare_drives.len(), 1);
        assert!(!bare_drives[0].summon);
        let json = serde_json::to_string(&bare_drives[0]).unwrap();
        assert!(!json.contains("summon"), "false must serialize as absent: {json}");

        // Hazard bookends but a human touched the accelerator mid-drive.
        let pedal_clip = summon_summary(
            "2026-07-16/2026-07-16_09-00-00-front.mp4",
            &[(GEAR_PARK, 60), (1, 1726)],
            &[(3, 150), (0, 800), (8, 36), (0, 718), (3, 82)],
            Some(2.0),
        );
        let pedal_drives = group_summaries_fast(&[pedal_clip], &HashMap::new());
        assert_eq!(pedal_drives.len(), 1);
        assert!(!pedal_drives[0].summon);
    }

    #[test]
    fn summon_reverse_only_negative_sei_speeds_is_flagged() {
        // Backing out of a garage: P → R → P with hazard bookends.
        // Reverse gear reports NEGATIVE SEI speeds, which the locked
        // display stats drop (stored max_speed_mps stays 0) — the
        // detector must still see the movement via the v16 abs-max
        // column.
        let clip = summon_summary(
            "2026-07-20/2026-07-20_09-00-00-front.mp4",
            &[(GEAR_PARK, 90), (2 /* GEAR_REVERSE */, 300), (GEAR_PARK, 60)],
            &[(3, 120), (0, 240), (3, 90)],
            Some(1.5),
        );
        assert_eq!(
            clip.aggregates.max_speed_mps, 0.0,
            "precondition: locked stats dropped the negative samples"
        );
        let drives = group_summaries_fast(&[clip], &HashMap::new());
        assert_eq!(drives.len(), 1);
        assert!(drives[0].summon);
    }

    #[test]
    fn summon_trailing_park_run_isolates_summon_from_following_drive() {
        // The bug this whole change fixes, end-to-end: with the trailing
        // Park run present on 20-50-43, the splitter ends the summon
        // there and a drive starting two minutes later stays separate;
        // both classify correctly. (Pre-fix, gearRuns=[Drive] fused all
        // three clips into one 4-minute drive → no summon flag.)
        let clip_a = summon_summary(
            "2026-07-15/2026-07-15_20-49-54-front.mp4",
            &[(GEAR_PARK, 60), (1, 1726)],
            &[(0, 27), (3, 123), (0, 17), (1, 873), (0, 746)],
            Some(2.7),
        );
        let clip_b = summon_summary(
            "2026-07-15/2026-07-15_20-50-43-front.mp4",
            &[(1, 500), (GEAR_PARK, 53)],
            &[(0, 471), (3, 82)],
            Some(2.7),
        );
        // Human drive 2 minutes later: no hazards, pedal input, normal
        // speed.
        let clip_c = summon_summary(
            "2026-07-15/2026-07-15_20-52-45-front.mp4",
            &[(1, 1790)],
            &[(8, 900), (4, 90), (8, 800)],
            Some(12.0),
        );
        let drives =
            group_summaries_fast(&[clip_a, clip_b, clip_c], &HashMap::new());
        assert_eq!(drives.len(), 2, "park gap must split summon from the next drive");
        assert!(drives[0].summon, "the summon drive keeps its flag");
        assert!(!drives[1].summon, "the human drive must not inherit it");
    }

    #[test]
    fn summon_reverse_summon_ending_seconds_before_human_drives_off_splits_and_flags() {
        // Real 2026-07-27 20:04 shape (ported from Sentry-Drive
        // grouper.test.js). Clip A: hazards in P, P→R under hazards
        // (leading P too short to split), backs out, shifts D, creeps
        // toward the user. Clip B: creep finishes, hazards through the
        // stop and D→P, ~3 s of Park, then the human gets in (brake,
        // accel) and drives off. The mid-clip park must split the summon
        // into its own drive; pedal input stays outside the summon
        // segment's frame range — and so does the human's SPEED: clip
        // B's whole-clip abs max is 5.4 m/s (over the cap), but the
        // per-run maxima scope the summon segment to its own crawl.
        let clip_a = summon_summary_runs(
            "2026-07-27/2026-07-27_20-04-00-front.mp4",
            &[(GEAR_PARK, 46), (2 /* GEAR_REVERSE */, 727), (1, 917)],
            frm(&[
                (0, 11, 0.0),
                (3, 113, 0.1),  // hazards spanning P→R
                (0, 394, 2.0),
                (2, 256, 2.0),  // right signal while maneuvering
                (0, 301, 1.5),
                (2, 615, 2.0),
            ]),
            Some(2.0),
        );
        let clip_b = summon_summary_runs(
            "2026-07-27/2026-07-27_20-04-46-front.mp4",
            &[(1, 120), (GEAR_PARK, 84), (1, 1469)],
            frm(&[
                (2, 81, 0.3),
                (3, 89, 0.3),   // hazards through the stop and D→P
                (4, 70, 0.0),   // human brake to shift (after the park)
                (0, 200, 0.0),
                (8, 690, 5.4),  // human accelerator
                (0, 112, 4.0),
                (1, 431, 5.4),
            ]),
            Some(5.4),
        );
        let drives = group_summaries_fast(&[clip_a, clip_b], &HashMap::new());
        assert_eq!(drives.len(), 2, "mid-clip park must split the summon from the departure");
        assert!(
            drives[0].summon,
            "summon keeps its flag despite sharing clip B with 5.4 m/s driving"
        );
        assert!(!drives[1].summon, "the human drive after the park is not summon");
    }

    #[test]
    fn summon_point_slice_speed_pollution_does_not_reject_frame_slow_summon() {
        // Real 2026-07-27 00:34 failure shape (ported from Sentry-Drive
        // grouper.test.js). In Drive, the park splitter's fraction→point
        // slice overshoots on deduped points and the summon's STATS
        // inherit the following drive's fast samples (a 6 mph summon
        // read 9 mph). Rusty's summary path has no point slices, but the
        // equivalent pollution exists: clip B's whole-clip abs max
        // (4.7 m/s, over the 4.5 gate) is the only clip-level speed —
        // per-run maxMps must confine the gate to the summon's own
        // frames ([0, 579) of clip B: 2.7 max) and flag the drive.
        let clip_a = summon_summary_runs(
            "2026-07-27/2026-07-27_00-34-31-front.mp4",
            &[(GEAR_PARK, 68), (2 /* GEAR_REVERSE */, 538), (1, 311)],
            frm(&[
                (0, 7, 0.0),
                (3, 144, 0.1),  // hazards spanning P→R
                (0, 766, 2.7),
            ]),
            Some(2.7),
        );
        let clip_b = summon_summary_runs(
            "2026-07-27/2026-07-27_00-34-59-front.mp4",
            &[(1, 579), (GEAR_PARK, 114), (1, 1162)],
            frm(&[
                (0, 143, 2.7),
                (3, 219, 2.6),  // hazard stop
                (0, 105, 2.0),
                (3, 191, 1.8),  // hazards through D→P
                (4, 50, 0.0),   // human brake (after the park)
                (0, 10, 0.0),
                (8, 521, 4.7),  // human accelerator
                (0, 16, 4.0),
                (8, 545, 4.7),
                (0, 55, 1.0),
            ]),
            Some(4.7),
        );
        let drives = group_summaries_fast(&[clip_a, clip_b], &HashMap::new());
        assert_eq!(drives.len(), 2);
        assert!(
            drives[0].summon,
            "frame-space speed evidence must override the polluted clip-level max"
        );
        assert!(!drives[1].summon, "the human drive after the park is not summon");
    }

    #[test]
    fn summon_whole_clip_abs_max_fallback_stays_conservative_on_shared_clip() {
        // The same 07-27 20:04 drive WITHOUT per-run maxima (pre-maxMps
        // row, import): the fallback sees clip B's whole-clip 5.4 m/s
        // and must reject rather than guess — no flag beats a wrong
        // flag.
        let clip_a = summon_summary(
            "2026-07-27/2026-07-27_20-04-00-front.mp4",
            &[(GEAR_PARK, 46), (2, 727), (1, 917)],
            &[(0, 11), (3, 113), (0, 394), (2, 256), (0, 301), (2, 615)],
            Some(2.0),
        );
        let clip_b = summon_summary(
            "2026-07-27/2026-07-27_20-04-46-front.mp4",
            &[(1, 120), (GEAR_PARK, 84), (1, 1469)],
            &[(2, 81), (3, 89), (4, 70), (0, 200), (8, 690), (0, 112), (1, 431)],
            Some(5.4),
        );
        let drives = group_summaries_fast(&[clip_a, clip_b], &HashMap::new());
        assert_eq!(drives.len(), 2);
        assert!(
            !drives[0].summon,
            "without per-run maxima the whole-clip 5.4 m/s must reject the summon"
        );
        assert!(!drives[1].summon);
    }

    #[test]
    fn self_driving_summon_zeroes_its_own_assistance_stats() {
        // End-to-end stats parity with Sentry-Drive's buildDriveStats.
        // A Summon reported by newer hardware reads FSD for the whole
        // maneuver, so WITHOUT the zeroing this drive would book FSD
        // miles, 100 % FSD, and a phantom disengagement where the car
        // parked itself. One whole clip, park-to-park, pedal-free, 2.7
        // m/s — the Self Driving signature with no hazards at all.
        let mut clip = summon_summary_ap(
            "2026-08-15/2026-08-15_10-00-00-front.mp4",
            &[(GEAR_PARK, 27), (1, 923), (GEAR_PARK, 100)],
            frm(&[(0, 27, 0.0), (0, 923, 2.7), (0, 100, 0.0)]),
            &[
                (AUTOPILOT_OFF, 27),
                (AUTOPILOT_FSD, 923),
                (AUTOPILOT_OFF, 100),
            ],
            Some(2.7),
        );
        // The clip really did record FSD engagement and distance.
        clip.aggregates.distance_m = 120.0;
        clip.aggregates.fsd_distance_m = 120.0;
        clip.aggregates.assisted_distance_m = 120.0;
        clip.aggregates.fsd_engaged_ms = 31_000;
        clip.aggregates.fsd_disengagements = 1;
        clip.aggregates.fsd_accel_pushes = 2;
        clip.aggregates.autosteer_engaged_ms = 500;
        clip.aggregates.autosteer_distance_m = 5.0;
        clip.aggregates.tacc_engaged_ms = 700;
        clip.aggregates.tacc_distance_m = 7.0;

        let drives = group_summaries_fast(&[clip], &HashMap::new());
        assert_eq!(drives.len(), 1);
        let d = &drives[0];
        assert!(d.summon, "precondition: the Self Driving signature fires");

        // Exactly Drive's zeroed set.
        assert_eq!(d.fsd_engaged_ms, 0);
        assert_eq!(d.fsd_disengagements, 0);
        assert_eq!(d.fsd_accel_pushes, 0);
        assert_eq!(d.fsd_distance_km, 0.0);
        assert_eq!(d.fsd_distance_mi, 0.0);
        assert_eq!(d.autosteer_engaged_ms, 0);
        assert_eq!(d.autosteer_distance_km, 0.0);
        assert_eq!(d.tacc_engaged_ms, 0);
        assert_eq!(d.tacc_distance_km, 0.0);
        // Percentages are DERIVED from those distances, so they must be
        // computed after the zeroing — this is the ordering assertion.
        assert_eq!(d.fsd_percent, 0.0);
        assert_eq!(d.autosteer_percent, 0.0);
        assert_eq!(d.tacc_percent, 0.0);
        assert_eq!(d.assisted_percent, 0.0);

        // …and NOTHING else. The drive itself still happened.
        assert!(d.distance_km > 0.0, "distance is not assistance data");
        assert!(d.duration_ms > 0);
        assert_eq!(d.clip_count, 1);
    }

    #[test]
    fn summon_drives_do_not_count_toward_fsd_analytics() {
        // A 100%-FSD commute in the morning, a detected summon in the
        // evening. The summon is driverless with autopilot_state unset,
        // so counting it would read as a fake "0% FSD" drive — the
        // score must stay 100%, the per-drive averages must not gain a
        // phantom denominator, and the summon's distance must stay out
        // of the analytics totals (mirrors Sentry-Drive's aggregate
        // builder; top-line drive totals elsewhere still include it).
        let mut fsd_commute = summon_summary(
            "2026-07-15/2026-07-15_08-00-00-front.mp4",
            &[(1, 1800)],
            &[],
            Some(20.0),
        );
        fsd_commute.aggregates.distance_m = 9850.0;
        fsd_commute.aggregates.fsd_distance_m = 9850.0;
        fsd_commute.aggregates.fsd_engaged_ms = 300_000;
        fsd_commute.aggregates.fsd_disengagements = 1;

        let mut summon_a = summon_summary(
            "2026-07-15/2026-07-15_20-49-54-front.mp4",
            &[(GEAR_PARK, 60), (1, 1726)],
            &[(0, 27), (3, 123), (0, 17), (1, 873), (0, 746)],
            Some(2.7),
        );
        summon_a.aggregates.distance_m = 100.0;
        let mut summon_b = summon_summary(
            "2026-07-15/2026-07-15_20-50-43-front.mp4",
            &[(1, 500), (GEAR_PARK, 53)],
            &[(0, 471), (3, 82)],
            Some(2.7),
        );
        summon_b.aggregates.distance_m = 50.0;

        let drives =
            group_summaries_fast(&[fsd_commute, summon_a, summon_b], &HashMap::new());
        assert_eq!(drives.len(), 2);
        assert!(!drives[0].summon);
        assert_eq!(drives[0].fsd_percent, 100.0);
        assert!(drives[1].summon, "precondition: the evening drive is a summon");
        assert!(drives[1].distance_km > 0.0, "precondition: it carries distance");

        let fsd = build_fsd_analytics(&drives, "all");
        assert_eq!(fsd.total_drives, 1, "summon drive must not count");
        assert_eq!(fsd.fsd_percent, 100.0, "score undiluted by the summon's distance");
        assert_eq!(fsd.total_distance_km, 9.85, "summon distance stays out of analytics");
        assert_eq!(
            fsd.avg_disengagements_per_drive, 1.0,
            "per-drive averages must not gain a phantom summon denominator"
        );
        assert_eq!(fsd.best_day_percent, 100.0);
    }

    // ── check-summon candidate selection ─────────────────────────────
    // Ports the selection rules of Sentry-Drive's `check-summon` repair
    // (electron-main.cjs): envelope drives whole, low-speed edges +
    // one boundary clip for fused drives, skip rows whose evidence is
    // already current.

    #[test]
    fn summon_check_selects_whole_envelope_drive_lacking_evidence() {
        // Slow 2-clip drive from a pre-flags extraction (no flag_runs
        // at all — unverifiable, so it can't be flagged yet): whole
        // drive inside the envelope → both clips selected.
        let clip_a = summon_summary(
            "2026-07-15/2026-07-15_20-49-54-front.mp4",
            &[(GEAR_PARK, 60), (1, 1726)],
            &[],
            Some(2.7),
        );
        let clip_b = summon_summary(
            "2026-07-15/2026-07-15_20-50-43-front.mp4",
            &[(1, 500), (GEAR_PARK, 53)],
            &[],
            Some(2.7),
        );
        let cands = summon_check_candidates(&[clip_a, clip_b]);
        assert_eq!(cands.candidate_drives, 1);
        assert_eq!(
            cands.files,
            vec![
                "2026-07-15/2026-07-15_20-49-54-front.mp4".to_string(),
                "2026-07-15/2026-07-15_20-50-43-front.mp4".to_string(),
            ]
        );
    }

    #[test]
    fn summon_check_skips_rows_with_current_evidence_and_flagged_drives() {
        // Same envelope drive with fully current evidence — per-run
        // maxima AND autopilot runs. The drive both counts as a
        // candidate AND needs no re-read; since its evidence already
        // flags it as summon, it is skipped entirely (JS: `d.summon →
        // continue`). Frame totals agree across all three RLEs (1786,
        // 553), which is what the detector's span check demands.
        let clip_a = summon_summary_ap(
            "2026-07-15/2026-07-15_20-49-54-front.mp4",
            &[(GEAR_PARK, 60), (1, 1726)],
            frm(&[(0, 27, 0.0), (3, 123, 0.1), (0, 1636, 2.7)]),
            &[(AUTOPILOT_OFF, 1786)],
            Some(2.7),
        );
        let clip_b = summon_summary_ap(
            "2026-07-15/2026-07-15_20-50-43-front.mp4",
            &[(1, 500), (GEAR_PARK, 53)],
            frm(&[(0, 471, 2.7), (3, 82, 0.3)]),
            &[(AUTOPILOT_OFF, 553)],
            Some(2.7),
        );
        let cands = summon_check_candidates(&[clip_a.clone(), clip_b]);
        assert_eq!(cands.candidate_drives, 0, "already-flagged drive is done");
        assert!(cands.files.is_empty(), "current evidence never re-reads");

        // v21 currency: maxMps-bearing flagRuns are NOT enough on their
        // own. Strip the autopilot runs and the drive must be re-read,
        // or every pre-v21 row would look up to date and never gain the
        // Self Driving evidence.
        let mut stale = clip_a.clone();
        stale.ap_runs.clear();
        let cands = summon_check_candidates(&[stale]);
        assert_eq!(
            cands.files,
            vec!["2026-07-15/2026-07-15_20-49-54-front.mp4".to_string()],
            "flagRuns with maxMps but no apRuns is stale evidence"
        );

        // A pedal-disqualified envelope drive with current evidence:
        // still a candidate drive (shape matches), zero re-reads.
        let clip_c = summon_summary_ap(
            "2026-07-16/2026-07-16_09-00-00-front.mp4",
            &[(1, 1800)],
            frm(&[(8, 900, 2.0), (0, 900, 2.0)]),
            &[(AUTOPILOT_OFF, 1800)],
            Some(2.0),
        );
        let cands = summon_check_candidates(&[clip_c]);
        assert_eq!(cands.candidate_drives, 1);
        assert!(cands.files.is_empty());
    }

    #[test]
    fn summon_check_selects_slow_edges_and_boundary_clips_of_fast_drive() {
        // 4-clip drive, slow-fast-fast-slow, all rows evidence-less.
        // Head scan takes the slow head + one boundary clip; tail scan
        // takes the slow tail + one boundary clip → all four here.
        let mk = |file: &str, abs_max: f64| {
            let mut c = summon_summary(file, &[(1, 1800)], &[], Some(abs_max));
            c.aggregates.max_speed_mps = abs_max;
            c
        };
        let clips = [
            mk("2026-07-16/2026-07-16_09-00-00-front.mp4", 2.0),
            mk("2026-07-16/2026-07-16_09-01-00-front.mp4", 20.0),
            mk("2026-07-16/2026-07-16_09-02-00-front.mp4", 20.0),
            mk("2026-07-16/2026-07-16_09-03-00-front.mp4", 2.0),
        ];
        let cands = summon_check_candidates(&clips);
        assert_eq!(cands.candidate_drives, 1);
        assert_eq!(
            cands.files,
            vec![
                "2026-07-16/2026-07-16_09-00-00-front.mp4".to_string(),
                "2026-07-16/2026-07-16_09-01-00-front.mp4".to_string(),
                "2026-07-16/2026-07-16_09-03-00-front.mp4".to_string(),
                "2026-07-16/2026-07-16_09-02-00-front.mp4".to_string(),
            ]
        );

        // All-fast drive: no slow edge → no boundary read, no candidate.
        let fast = [
            mk("2026-07-17/2026-07-17_09-00-00-front.mp4", 20.0),
            mk("2026-07-17/2026-07-17_09-01-00-front.mp4", 20.0),
        ];
        let cands = summon_check_candidates(&fast);
        assert_eq!(cands.candidate_drives, 0);
        assert!(cands.files.is_empty());
    }

    #[test]
    fn summon_check_pre_v16_rows_fall_back_to_locked_max_speed() {
        // Pre-v16 row: no sei_speed_abs_max. Slow needs speed samples
        // plus a locked max under the cap; a sample-less row can't be
        // verified slow (matches the JS empty-speeds bail).
        let mk = |file: &str, max_mps: f64, samples: i64| {
            let mut c = summon_summary(file, &[(1, 1800)], &[], None);
            c.aggregates.max_speed_mps = max_mps;
            c.aggregates.speed_sample_count = samples;
            c
        };
        // Slow verified head on a fast drive → head + boundary selected.
        let clips = [
            mk("2026-07-18/2026-07-18_09-00-00-front.mp4", 2.0, 100),
            mk("2026-07-18/2026-07-18_09-01-00-front.mp4", 20.0, 100),
            mk("2026-07-18/2026-07-18_09-02-00-front.mp4", 20.0, 100),
        ];
        let cands = summon_check_candidates(&clips);
        assert_eq!(cands.candidate_drives, 1);
        assert_eq!(
            cands.files,
            vec![
                "2026-07-18/2026-07-18_09-00-00-front.mp4".to_string(),
                "2026-07-18/2026-07-18_09-01-00-front.mp4".to_string(),
            ]
        );

        // Same head with zero speed samples → unverifiable, no scan.
        let clips = [
            mk("2026-07-19/2026-07-19_09-00-00-front.mp4", 0.0, 0),
            mk("2026-07-19/2026-07-19_09-01-00-front.mp4", 20.0, 100),
            mk("2026-07-19/2026-07-19_09-02-00-front.mp4", 20.0, 100),
        ];
        let cands = summon_check_candidates(&clips);
        assert_eq!(cands.candidate_drives, 0);
        assert!(cands.files.is_empty());
    }

    #[test]
    fn summon_check_skips_imported_drives_and_bridge_files() {
        let mut imported = summon_summary(
            "2026-07-20/2026-07-20_09-00-00-front.mp4",
            &[(1, 1800)],
            &[],
            Some(2.0),
        );
        imported.source = Some("tessie".to_string());
        let cands = summon_check_candidates(&[imported]);
        assert_eq!(cands.candidate_drives, 0, "imported drives can't gain SEI evidence");
        assert!(cands.files.is_empty());

        // Synthetic gap-fill bridge rows have no MP4 on disk — an
        // envelope drive containing one selects only the real clip.
        let real = summon_summary(
            "2026-07-21/2026-07-21_09-00-00-front.mp4",
            &[(1, 1800)],
            &[],
            Some(2.0),
        );
        let bridge = summon_summary(
            "2026-07-21/2026-07-21_09-01-00-front-bridge.mp4",
            &[(1, 1800)],
            &[],
            Some(2.0),
        );
        let cands = summon_check_candidates(&[real, bridge]);
        assert_eq!(cands.candidate_drives, 1);
        assert_eq!(
            cands.files,
            vec!["2026-07-21/2026-07-21_09-00-00-front.mp4".to_string()]
        );
    }
}
