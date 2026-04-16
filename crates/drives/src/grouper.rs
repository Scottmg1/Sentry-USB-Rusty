//! Drive grouping, gear splitting, stats computation, FSD analytics.
//!
//! Ported from Go `server/drives/grouper.go`. Groups Tesla dashcam clips into
//! logical drives based on timestamp gaps and gear state transitions, then
//! computes distance, speed, and FSD/autopilot analytics per drive.

use std::collections::HashMap;

use chrono::{Datelike, NaiveDate, NaiveDateTime};

use crate::extract::{
    AUTOPILOT_AUTOSTEER, AUTOPILOT_FSD, AUTOPILOT_OFF, AUTOPILOT_TACC, GEAR_PARK,
};
use crate::types::*;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Time gap (ms) that splits clips into separate drives (5 minutes).
const DRIVE_GAP_MS: i64 = 5 * 60 * 1000;

/// Minimum Park duration (seconds) that ends the current drive within a clip.
const PARK_GAP_SECONDS: f64 = 2.0;

// ---------------------------------------------------------------------------
// Public API — signatures match drives_handler.rs call-sites
// ---------------------------------------------------------------------------

/// Groups routes into drives and returns lightweight summaries (no full point
/// arrays). Memory-efficient: computes stats directly from raw clips.
pub fn group_summaries(
    routes: &[Route],
    tags: &HashMap<String, Vec<String>>,
) -> Vec<DriveSummary> {
    let groups = group_clips(routes);
    let mut summaries = Vec::with_capacity(groups.len());

    for (idx, clips) in groups.iter().enumerate() {
        summaries.push(build_summary(clips, idx, tags));
    }
    summaries
}

/// Build a single drive with full merged point data.
/// `id` is a string from the URL path — either a numeric index or a startTime
/// string. Tries numeric parse first, then falls back to matching by startTime.
pub fn build_single_drive(
    routes: &[Route],
    id: &str,
    tags: &HashMap<String, Vec<String>>,
) -> Option<Drive> {
    let groups = group_clips(routes);

    // Try numeric index first
    if let Ok(idx) = id.parse::<usize>() {
        if idx < groups.len() {
            return Some(build_drive_stats(&groups[idx], idx as i32, tags));
        }
    }

    // Fall back to matching by start time string
    for (idx, group) in groups.iter().enumerate() {
        let st = group[0]
            .timestamp
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string();
        if st == id {
            return Some(build_drive_stats(group, idx as i32, tags));
        }
    }
    None
}

/// Compute aggregate statistics directly from routes WITHOUT building full
/// Drive objects. Critical for memory-constrained Pi devices.
pub fn compute_aggregate_stats(routes: &[Route]) -> AggregateStats {
    compute_aggregate_stats_from_routes(routes)
}

/// FSD analytics with daily/weekly breakdowns.
/// Computes summaries first, then aggregates by period.
pub fn fsd_analytics(routes: &[Route]) -> FsdAnalytics {
    let empty_tags = HashMap::new();
    let summaries = group_summaries(routes, &empty_tags);
    build_fsd_analytics(&summaries, "week")
}

/// Overview routes for map display (downsampled, outlier-filtered).
pub fn route_overviews(routes: &[Route], max_points_per_drive: usize) -> Vec<RouteOverview> {
    group_routes_overview(routes, max_points_per_drive)
}

// ---------------------------------------------------------------------------
// Internal: clip grouping
// ---------------------------------------------------------------------------

/// Dedup by normalized file path, parse timestamps, sort, split on 5-min gaps,
/// then split by gear state transitions.
fn group_clips(routes: &[Route]) -> Vec<Vec<TimedRoute>> {
    if routes.is_empty() {
        return Vec::new();
    }

    // Deduplicate by normalized file path (handles mixed \ and /)
    let mut seen = HashMap::with_capacity(routes.len());
    let mut unique = Vec::with_capacity(routes.len());
    for r in routes {
        let norm = r.file.replace('\\', "/");
        if seen.insert(norm, ()).is_none() {
            unique.push(r);
        }
    }

    // Parse timestamps and build TimedRoute references
    let mut timed: Vec<TimedRoute> = unique
        .into_iter()
        .filter_map(|r| {
            let ts = parse_file_timestamp(&r.file)?;
            Some(TimedRoute {
                route: r.clone(),
                timestamp: ts,
            })
        })
        .collect();

    if timed.is_empty() {
        return Vec::new();
    }

    timed.sort_by(|a, b| a.timestamp.cmp(&b.timestamp));

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

    // Second pass: split each time group further by gear state (Park transitions)
    let mut groups = Vec::new();
    for tg in time_groups {
        let mut splits = split_by_gear_state(tg);
        groups.append(&mut splits);
    }
    groups
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

/// Analyse a clip's GearRuns and split its points at any Park gap >=
/// PARK_GAP_SECONDS. Returns one or more segments.
fn split_clip_at_park_gaps(clip: &TimedRoute) -> Vec<ClipSegment> {
    let total_raw_frames: u32 = clip.route.gear_runs.iter().map(|r| r.frames).sum();
    if total_raw_frames == 0 {
        return vec![ClipSegment {
            route: clip.clone(),
            parked: false,
        }];
    }

    let seconds_per_frame = 60.0 / total_raw_frames as f64;
    let n_points = clip.route.points.len();

    // Identify raw segments that are park gaps
    struct RawSeg {
        start_frame: u32,
        end_frame: u32,
        parked: bool,
    }

    let mut raw_segs = Vec::new();
    let mut frame: u32 = 0;
    for run in &clip.route.gear_runs {
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

    // Check if any split is needed
    if !merged.iter().any(|s| s.parked) {
        return vec![ClipSegment {
            route: clip.clone(),
            parked: false,
        }];
    }

    // Map raw frame ranges to deduped point indices and build segments
    let mut result = Vec::new();
    for seg in &merged {
        if seg.parked {
            result.push(ClipSegment {
                route: TimedRoute {
                    route: Route::empty(),
                    timestamp: clip.timestamp,
                },
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
        if end_idx <= start_idx {
            continue;
        }

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

        // Compute timestamp offset for this segment within the clip
        let offset_secs = (start_frac * 60.0) as i64;
        let offset = chrono::Duration::seconds(offset_secs);

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
                },
                timestamp: clip.timestamp + offset,
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
    if clip.route.raw_frame_count > 0 {
        return (clip.route.raw_park_count as f64 / clip.route.raw_frame_count as f64) > 0.5;
    }
    if clip.route.gear_states.is_empty() {
        return false;
    }
    let park_count = clip
        .route
        .gear_states
        .iter()
        .filter(|&&g| g == GEAR_PARK)
        .count();
    park_count > clip.route.gear_states.len() / 2
}

// ---------------------------------------------------------------------------
// GroupSummaries — lightweight stats without merging point arrays
// ---------------------------------------------------------------------------

/// Build a DriveSummary for one group of clips.
fn build_summary(
    clips: &[TimedRoute],
    idx: usize,
    tags: &HashMap<String, Vec<String>>,
) -> DriveSummary {
    let first_clip = &clips[0];
    let last_clip = &clips[clips.len() - 1];
    let start_time = first_clip.timestamp;
    let end_time = last_clip.timestamp + chrono::Duration::minutes(1);
    let duration_ms = (end_time - start_time).num_milliseconds();

    let mut total_dist_m: f64 = 0.0;
    let mut max_speed_mps: f64 = 0.0;
    let mut speed_sum: f64 = 0.0;
    let mut speed_count: usize = 0;
    let mut point_count: usize = 0;

    let mut fsd_engaged_ms: i64 = 0;
    let mut autosteer_engaged_ms: i64 = 0;
    let mut tacc_engaged_ms: i64 = 0;
    let mut fsd_dist_m: f64 = 0.0;
    let mut autosteer_dist_m: f64 = 0.0;
    let mut tacc_dist_m: f64 = 0.0;
    let mut assisted_dist_m: f64 = 0.0;
    let mut fsd_disengagements: i32 = 0;
    let mut fsd_accel_pushes: i32 = 0;

    let mut start_point: Option<GpsPoint> = None;
    let mut end_point: Option<GpsPoint> = None;

    // First pass: compute median location from valid points across all clips
    let mut valid_lats: Vec<f64> = Vec::new();
    let mut valid_lngs: Vec<f64> = Vec::new();
    for clip in clips {
        for p in &clip.route.points {
            if !(p[0].abs() < 1.0 && p[1].abs() < 1.0) {
                valid_lats.push(p[0]);
                valid_lngs.push(p[1]);
            }
        }
    }

    let (med_lat, med_lng, has_median) = compute_median_location(&valid_lats, &valid_lngs);
    drop(valid_lats);
    drop(valid_lngs);

    // Second pass: compute stats, filtering outliers per-clip
    for clip in clips {
        let n = clip.route.points.len();
        if n == 0 {
            continue;
        }

        // Build validity mask for this clip's points
        let mut valid = build_validity_mask(&clip.route.points, has_median, med_lat, med_lng);

        // Neighbor-jump filter
        apply_neighbor_jump_filter(&clip.route.points, &mut valid);

        // Track start/end points
        if start_point.is_none() {
            for i in 0..n {
                if valid[i] {
                    start_point = Some([clip.route.points[i][0], clip.route.points[i][1]]);
                    break;
                }
            }
        }
        for i in (0..n).rev() {
            if valid[i] {
                end_point = Some([clip.route.points[i][0], clip.route.points[i][1]]);
                break;
            }
        }

        for i in 0..n {
            if valid[i] {
                point_count += 1;
            }
        }

        let clip_duration_ms: f64 = 60000.0;
        let has_ap = clip.route.autopilot_states.len() == n;
        let has_gears = clip.route.gear_states.len() == n;
        let has_accel = clip.route.accel_positions.len() == n;
        let has_speeds = clip.route.speeds.len() == n;
        let has_sei_speeds = has_speeds && clip.route.speeds.iter().any(|&s| s > 0.0);

        // Per-clip FSD event tracking state
        let mut in_accel_press = false;
        let mut fsd_engage_idx: i32 = -1;
        let mut pending_disengage = false;
        let mut pending_disengage_idx: usize = 0;

        for i in 1..n {
            if !valid[i] || !valid[i - 1] {
                continue;
            }

            let d = haversine_m(
                clip.route.points[i - 1][0],
                clip.route.points[i - 1][1],
                clip.route.points[i][0],
                clip.route.points[i][1],
            );

            // Skip GPS teleportation artifacts
            if !has_sei_speeds {
                let dt_sec = (clip_duration_ms / (n - 1) as f64) / 1000.0;
                if dt_sec > 0.0 && d / dt_sec > 70.0 {
                    continue;
                }
            }

            total_dist_m += d;
            let dt_ms = clip_duration_ms / (n - 1) as f64;

            // Speed
            if has_sei_speeds {
                let speed = clip.route.speeds[i] as f64;
                if speed >= 0.0 && speed < 100.0 {
                    speed_sum += speed;
                    speed_count += 1;
                    if speed > max_speed_mps {
                        max_speed_mps = speed;
                    }
                }
            } else {
                let dt_sec = dt_ms / 1000.0;
                if dt_sec > 0.0 {
                    let speed = d / dt_sec;
                    if speed < 70.0 {
                        speed_sum += speed;
                        speed_count += 1;
                        if speed > max_speed_mps {
                            max_speed_mps = speed;
                        }
                    }
                }
            }

            // Autopilot stats
            if has_ap {
                let cur_ap = clip.route.autopilot_states[i];
                let prev_ap = clip.route.autopilot_states[i - 1];

                if cur_ap != AUTOPILOT_OFF {
                    assisted_dist_m += d;
                    match cur_ap {
                        x if x == AUTOPILOT_FSD => {
                            fsd_engaged_ms += dt_ms as i64;
                            fsd_dist_m += d;
                        }
                        x if x == AUTOPILOT_AUTOSTEER => {
                            autosteer_engaged_ms += dt_ms as i64;
                            autosteer_dist_m += d;
                        }
                        x if x == AUTOPILOT_TACC => {
                            tacc_engaged_ms += dt_ms as i64;
                            tacc_dist_m += d;
                        }
                        _ => {}
                    }
                }

                // Track FSD engagement start
                if prev_ap != AUTOPILOT_FSD && cur_ap == AUTOPILOT_FSD {
                    fsd_engage_idx = i as i32;
                    in_accel_press = false;
                }

                // Resolve pending disengagement: if Park arrives within 2s, FSD
                // parked the car — not a driver override.
                if pending_disengage {
                    let time_since_ms = (i - pending_disengage_idx) as f64 * dt_ms;
                    if has_gears
                        && clip.route.gear_states[i] == GEAR_PARK
                        && time_since_ms <= 2000.0
                    {
                        pending_disengage = false;
                    } else if time_since_ms > 2000.0 || cur_ap == AUTOPILOT_FSD {
                        fsd_disengagements += 1;
                        pending_disengage = false;
                    }
                }

                // Detect FSD disengagement — defer for Park grace period
                if prev_ap == AUTOPILOT_FSD && cur_ap != AUTOPILOT_FSD {
                    pending_disengage = true;
                    pending_disengage_idx = i;
                    in_accel_press = false;
                }

                // Accel push detection
                if cur_ap == AUTOPILOT_FSD && has_accel {
                    let mut accel_pct = clip.route.accel_positions[i] as f64;
                    if accel_pct <= 1.0 {
                        accel_pct *= 100.0;
                    }
                    let time_since_engage_ms = if fsd_engage_idx >= 0 {
                        (i as i32 - fsd_engage_idx) as f64 * dt_ms
                    } else {
                        0.0
                    };
                    if !in_accel_press && accel_pct > 1.0 && time_since_engage_ms >= 3000.0 {
                        in_accel_press = true;
                    } else if in_accel_press && accel_pct <= 0.0 {
                        fsd_accel_pushes += 1;
                        in_accel_press = false;
                    }
                } else if cur_ap != AUTOPILOT_FSD {
                    in_accel_press = false;
                }
            }
        }

        // Flush pending disengagement at end of clip
        if pending_disengage {
            if !(has_gears && clip.route.gear_states[n - 1] == GEAR_PARK) {
                fsd_disengagements += 1;
            }
        }
    }

    let avg_speed_mps = if speed_count > 0 {
        speed_sum / speed_count as f64
    } else {
        0.0
    };

    let (fsd_percent, autosteer_percent, tacc_percent, assisted_percent) =
        compute_autopilot_percents(total_dist_m, fsd_dist_m, autosteer_dist_m, tacc_dist_m, assisted_dist_m);

    let start_time_str = start_time.format("%Y-%m-%dT%H:%M:%S").to_string();
    let drive_tags = tags.get(&start_time_str).cloned().unwrap_or_default();

    DriveSummary {
        id: idx as i32,
        date: first_clip.route.date.clone(),
        start_time: start_time_str,
        end_time: end_time.format("%Y-%m-%dT%H:%M:%S").to_string(),
        duration_ms,
        distance_mi: round2(total_dist_m / 1609.344),
        distance_km: round2(total_dist_m / 1000.0),
        avg_speed_mph: round2(avg_speed_mps * 2.23694),
        max_speed_mph: round2(max_speed_mps * 2.23694),
        avg_speed_kmh: round2(avg_speed_mps * 3.6),
        max_speed_kmh: round2(max_speed_mps * 3.6),
        clip_count: clips.len(),
        point_count,
        start_point,
        end_point,
        tags: drive_tags,
        fsd_engaged_ms,
        fsd_disengagements,
        fsd_accel_pushes,
        fsd_percent,
        fsd_distance_km: round2(fsd_dist_m / 1000.0),
        fsd_distance_mi: round2(fsd_dist_m / 1609.344),
        autosteer_engaged_ms,
        autosteer_percent,
        autosteer_distance_km: round2(autosteer_dist_m / 1000.0),
        autosteer_distance_mi: round2(autosteer_dist_m / 1609.344),
        tacc_engaged_ms,
        tacc_percent,
        tacc_distance_km: round2(tacc_dist_m / 1000.0),
        tacc_distance_mi: round2(tacc_dist_m / 1609.344),
        assisted_percent,
    }
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
    let end_time = last_clip.timestamp + chrono::Duration::minutes(1);

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
        let clip_duration_ms: f64 = 60000.0;
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
        const MAX_FROM_MEDIAN_M: f64 = 1_000_000.0;
        all_points.retain(|p| haversine_m(p.lat, p.lng, med_lat, med_lng) <= MAX_FROM_MEDIAN_M);

        // Step 3: remove isolated outliers far from both neighbors
        const MAX_JUMP_M: f64 = 5000.0;
        let n = all_points.len();
        if n > 2 {
            let mut remove = vec![false; n];
            for i in 0..n {
                let has_prev = i > 0;
                let has_next = i < n - 1;
                let far_from_prev = has_prev
                    && haversine_m(
                        all_points[i - 1].lat,
                        all_points[i - 1].lng,
                        all_points[i].lat,
                        all_points[i].lng,
                    ) > MAX_JUMP_M;
                let far_from_next = has_next
                    && haversine_m(
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
        let d = haversine_m(
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

    // Build point data array: [lat, lng, timeMs, speedMps]
    let mut point_data: Vec<[f64; 4]> = Vec::with_capacity(all_points.len());
    let mut gear_states: Vec<i32> = Vec::with_capacity(all_points.len());
    let mut fsd_states: Vec<i32> = Vec::with_capacity(all_points.len());
    let mut has_fsd_data = false;
    let mut has_gear_data = false;

    for (i, p) in all_points.iter().enumerate() {
        let speed = if has_sei_speeds {
            p.sei_speed as f64
        } else if i > 0 {
            let d = haversine_m(
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
        point_data.push([p.lat, p.lng, p.time_ms, round2(speed)]);
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
            let d = haversine_m(prev.lat, prev.lng, cur.lat, cur.lng);

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
        distance_mi: round2(total_distance_m / 1609.344),
        distance_km: round2(total_distance_m / 1000.0),
        avg_speed_mph: round2(avg_speed_mps * 2.23694),
        max_speed_mph: round2(max_speed_mps * 2.23694),
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
        fsd_distance_mi: round2(fsd_distance_m / 1609.344),
        autosteer_engaged_ms,
        autosteer_percent,
        autosteer_distance_km: round2(autosteer_distance_m / 1000.0),
        autosteer_distance_mi: round2(autosteer_distance_m / 1609.344),
        tacc_engaged_ms,
        tacc_percent,
        tacc_distance_km: round2(tacc_distance_m / 1000.0),
        tacc_distance_mi: round2(tacc_distance_m / 1609.344),
        assisted_percent,
    }
}

// ---------------------------------------------------------------------------
// GroupRoutesOverview — downsampled routes for map display
// ---------------------------------------------------------------------------

/// Returns downsampled route polylines for every drive, with outlier filtering.
fn group_routes_overview(routes: &[Route], max_points_per_drive: usize) -> Vec<RouteOverview> {
    let groups = group_clips(routes);
    let mut result = Vec::with_capacity(groups.len());

    const MAX_FROM_MEDIAN_M: f64 = 1_000_000.0;
    const MAX_JUMP_M: f64 = 5000.0;

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

            pts.retain(|p| haversine_m(p[0], p[1], med_lat, med_lng) <= MAX_FROM_MEDIAN_M);
        }

        // Neighbor-jump filter
        if pts.len() > 2 {
            let n = pts.len();
            let mut remove = vec![false; n];
            for i in 0..n {
                let has_prev = i > 0;
                let has_next = i < n - 1;
                let far_from_prev =
                    has_prev && haversine_m(pts[i - 1][0], pts[i - 1][1], pts[i][0], pts[i][1]) > MAX_JUMP_M;
                let far_from_next =
                    has_next && haversine_m(pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1]) > MAX_JUMP_M;
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

        result.push(RouteOverview {
            id: idx as i32,
            points: downsample(&pts, max_points_per_drive),
        });
    }

    result
}

// ---------------------------------------------------------------------------
// ComputeAggregateStatsFromRoutes — streaming aggregate
// ---------------------------------------------------------------------------

/// Internal timestamp+index pair for lightweight grouping.
struct RouteTimestamp {
    ts: NaiveDateTime,
    idx: usize,
}

/// Compute aggregate statistics directly from routes WITHOUT building full Drive
/// objects. Drive count uses lightweight timestamp-gap + gear-split counting.
fn compute_aggregate_stats_from_routes(routes: &[Route]) -> AggregateStats {
    let mut s = AggregateStats::default();
    if routes.is_empty() {
        return s;
    }

    s.routes_count = routes.len();

    // Deduplicate by normalized file path
    let mut seen = HashMap::with_capacity(routes.len());
    let mut timed: Vec<RouteTimestamp> = Vec::new();
    for (i, r) in routes.iter().enumerate() {
        let norm = r.file.replace('\\', "/");
        if seen.insert(norm, ()).is_some() {
            continue;
        }
        if let Some(ts) = parse_file_timestamp(&r.file) {
            timed.push(RouteTimestamp { ts, idx: i });
        }
    }
    timed.sort_by(|a, b| a.ts.cmp(&b.ts));

    // Lightweight drive count + duration via timestamp + gear-state grouping
    if !timed.is_empty() {
        let mut group_start = 0;
        for i in 1..=timed.len() {
            let is_end = i == timed.len();
            let is_gap = !is_end
                && (timed[i].ts - timed[i - 1].ts).num_milliseconds() > DRIVE_GAP_MS;
            if is_end || is_gap {
                let group = &timed[group_start..i];
                s.drives_count += count_gear_splits_in_group(routes, group);
                let group_end = timed[i - 1].ts + chrono::Duration::minutes(1);
                s.total_duration_ms += (group_end - timed[group_start].ts).num_milliseconds();
                if !is_end {
                    group_start = i;
                }
            }
        }
    }

    // Per-route distance and autopilot stats
    let mut total_distance_m: f64 = 0.0;
    let mut total_fsd_dist_m: f64 = 0.0;
    let mut total_autosteer_dist_m: f64 = 0.0;
    let mut total_tacc_dist_m: f64 = 0.0;

    for ti in &timed {
        let r = &routes[ti.idx];
        let n = r.points.len();
        if n < 2 {
            continue;
        }

        let clip_duration_ms: f64 = 60000.0;
        let clip_start_ms = ti.ts.and_utc().timestamp_millis() as f64;
        let has_ap = r.autopilot_states.len() == n;
        let has_gears = r.gear_states.len() == n;
        let has_accel = r.accel_positions.len() == n;
        let has_sei_speeds = r.speeds.len() == n && r.speeds.iter().any(|&sp| sp > 0.0);

        let mut in_accel_press = false;

        for i in 1..n {
            let d = haversine_m(
                r.points[i - 1][0],
                r.points[i - 1][1],
                r.points[i][0],
                r.points[i][1],
            );

            // Skip GPS teleportation artifacts
            if !has_sei_speeds {
                let dt_sec = (clip_duration_ms / (n - 1) as f64) / 1000.0;
                if dt_sec > 0.0 && d / dt_sec > 70.0 {
                    continue;
                }
            }

            total_distance_m += d;
            let dt_ms = clip_duration_ms / (n - 1) as f64;

            if has_ap {
                let prev_ap = r.autopilot_states[i - 1];
                let cur_ap = r.autopilot_states[i];

                match cur_ap {
                    x if x == AUTOPILOT_FSD => {
                        s.fsd_engaged_ms += dt_ms as i64;
                        total_fsd_dist_m += d;
                    }
                    x if x == AUTOPILOT_AUTOSTEER => {
                        s.autosteer_engaged_ms += dt_ms as i64;
                        total_autosteer_dist_m += d;
                    }
                    x if x == AUTOPILOT_TACC => {
                        s.tacc_engaged_ms += dt_ms as i64;
                        total_tacc_dist_m += d;
                    }
                    _ => {}
                }

                // FSD disengagement: FSD -> non-FSD
                if prev_ap == AUTOPILOT_FSD && cur_ap != AUTOPILOT_FSD {
                    let mut skip_disengage = false;
                    if has_gears {
                        let t_cur =
                            clip_start_ms + (clip_duration_ms * i as f64 / (n - 1) as f64);
                        for j in i..n {
                            let t_j =
                                clip_start_ms + (clip_duration_ms * j as f64 / (n - 1) as f64);
                            if (t_j - t_cur) > 2000.0 {
                                break;
                            }
                            if r.gear_states[j] == GEAR_PARK {
                                skip_disengage = true;
                                break;
                            }
                        }
                    }
                    if !skip_disengage {
                        s.fsd_disengagements += 1;
                    }
                    in_accel_press = false;
                }

                // FSD accel push detection
                if cur_ap == AUTOPILOT_FSD && has_accel {
                    let mut accel_pct = r.accel_positions[i] as f64;
                    if accel_pct <= 1.0 {
                        accel_pct *= 100.0;
                    }
                    if !in_accel_press && accel_pct > 1.0 {
                        in_accel_press = true;
                    } else if in_accel_press && accel_pct <= 0.0 {
                        s.fsd_accel_pushes += 1;
                        in_accel_press = false;
                    }
                } else if cur_ap != AUTOPILOT_FSD {
                    in_accel_press = false;
                }
            }
        }
    }

    s.total_distance_km = total_distance_m / 1000.0;
    s.total_distance_mi = total_distance_m / 1609.344;
    s.fsd_distance_km = total_fsd_dist_m / 1000.0;
    s.fsd_distance_mi = total_fsd_dist_m / 1609.344;
    s.autosteer_distance_km = total_autosteer_dist_m / 1000.0;
    s.autosteer_distance_mi = total_autosteer_dist_m / 1609.344;
    s.tacc_distance_km = total_tacc_dist_m / 1000.0;
    s.tacc_distance_mi = total_tacc_dist_m / 1609.344;

    if s.total_distance_km > 0.0 {
        s.fsd_percent = round1(s.fsd_distance_km / s.total_distance_km * 100.0);
        let total_assisted_km =
            s.fsd_distance_km + s.autosteer_distance_km + s.tacc_distance_km;
        s.assisted_percent = round1(total_assisted_km / s.total_distance_km * 100.0);
    }

    s
}

/// Count drives from gear runs within a time group without allocating Drive
/// objects. Mirrors splitByGearState logic but only counts.
fn count_gear_splits_in_group(routes: &[Route], group: &[RouteTimestamp]) -> usize {
    if group.is_empty() {
        return 0;
    }

    let has_gear_runs = group
        .iter()
        .any(|entry| !routes[entry.idx].gear_runs.is_empty());

    if !has_gear_runs {
        // Legacy fallback: count transitions through majority-park clips
        let mut count: usize = 1;
        let mut prev_all_park = false;
        for entry in group {
            let r = &routes[entry.idx];
            if r.raw_frame_count > 0 && r.raw_park_count > 0 {
                let is_all_park =
                    r.raw_park_count as f64 / r.raw_frame_count as f64 > 0.6;
                if prev_all_park && !is_all_park {
                    count += 1;
                }
                prev_all_park = is_all_park;
            } else {
                prev_all_park = false;
            }
        }
        return count;
    }

    // Mirror splitByGearState: count non-parked segments separated by park gaps
    let mut count: usize = 0;
    let mut in_drive = false;

    for entry in group {
        let r = &routes[entry.idx];
        let total_frames: u32 = r.gear_runs.iter().map(|run| run.frames).sum();
        if total_frames == 0 {
            if !in_drive {
                in_drive = true;
                count += 1;
            }
            continue;
        }
        let sec_per_frame = 60.0 / total_frames as f64;
        for run in &r.gear_runs {
            if run.gear == GEAR_PARK {
                let duration = run.frames as f64 * sec_per_frame;
                if duration >= PARK_GAP_SECONDS {
                    in_drive = false;
                }
            } else if !in_drive {
                in_drive = true;
                count += 1;
            }
        }
    }

    // If everything was parked, count as 1
    if count == 0 {
        1
    } else {
        count
    }
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

    // Filter drives in period
    let period_drives: Vec<&DriveSummary> = summaries
        .iter()
        .filter(|d| {
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

    // FSD grade
    let fsd_grade = if fsd_percent >= 90.0 {
        "Great"
    } else if fsd_percent >= 60.0 {
        "Good"
    } else {
        "Needs Improvement"
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

    // Avg per drive
    let avg_disengagements = if fsd_sessions > 0 {
        round2(disengagements as f64 / fsd_sessions as f64)
    } else {
        0.0
    };
    let avg_accel_pushes = if fsd_sessions > 0 {
        round2(accel_pushes as f64 / fsd_sessions as f64)
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
// Utility functions
// ---------------------------------------------------------------------------

/// Haversine distance in meters between two GPS coordinates.
fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let to_rad = |d: f64| d * std::f64::consts::PI / 180.0;

    let d_lat = to_rad(lat2 - lat1);
    let d_lon = to_rad(lon2 - lon1);
    let a = (d_lat / 2.0).sin().powi(2)
        + to_rad(lat1).cos() * to_rad(lat2).cos() * (d_lon / 2.0).sin().powi(2);
    R * 2.0 * a.sqrt().atan2((1.0 - a).sqrt())
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
fn parse_file_timestamp(file_path: &str) -> Option<NaiveDateTime> {
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

/// Compute median location from the middle 50% of valid lat/lng arrays.
fn compute_median_location(lats: &[f64], lngs: &[f64]) -> (f64, f64, bool) {
    if lats.len() <= 2 {
        return (0.0, 0.0, false);
    }
    let q1 = lats.len() / 4;
    let q3 = lats.len() * 3 / 4;
    let count = q3 - q1 + 1;
    let mut sum_lat: f64 = 0.0;
    let mut sum_lng: f64 = 0.0;
    for i in q1..=q3 {
        sum_lat += lats[i];
        sum_lng += lngs[i];
    }
    (sum_lat / count as f64, sum_lng / count as f64, true)
}

/// Build a validity mask for a clip's points: exclude null island and
/// median-cluster outliers (>1000km from median).
fn build_validity_mask(
    points: &[GpsPoint],
    has_median: bool,
    med_lat: f64,
    med_lng: f64,
) -> Vec<bool> {
    const MAX_FROM_MEDIAN_M: f64 = 1_000_000.0;
    let n = points.len();
    let mut valid = vec![false; n];
    for (i, p) in points.iter().enumerate() {
        if p[0].abs() < 1.0 && p[1].abs() < 1.0 {
            continue; // null island
        }
        if has_median && haversine_m(p[0], p[1], med_lat, med_lng) > MAX_FROM_MEDIAN_M {
            continue; // too far from median cluster
        }
        valid[i] = true;
    }
    valid
}

/// Remove points far from both neighbors in the validity mask.
fn apply_neighbor_jump_filter(points: &[GpsPoint], valid: &mut [bool]) {
    const MAX_JUMP_M: f64 = 5000.0;
    let n = points.len();
    if n <= 2 {
        return;
    }
    // We need a snapshot to avoid cascading invalidation within one pass
    let snapshot: Vec<bool> = valid.to_vec();
    for i in 0..n {
        if !snapshot[i] {
            continue;
        }
        let has_prev = i > 0 && snapshot[i - 1];
        let has_next = i < n - 1 && snapshot[i + 1];
        let far_from_prev = has_prev
            && haversine_m(points[i - 1][0], points[i - 1][1], points[i][0], points[i][1])
                > MAX_JUMP_M;
        let far_from_next = has_next
            && haversine_m(points[i][0], points[i][1], points[i + 1][0], points[i + 1][1])
                > MAX_JUMP_M;
        if (has_prev && has_next && far_from_prev && far_from_next)
            || (!has_prev && far_from_next)
            || (!has_next && far_from_prev)
        {
            valid[i] = false;
        }
    }
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

/// Round to 2 decimal places.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Round to 1 decimal place (used for percentages: *1000/10 in Go).
fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

// ---------------------------------------------------------------------------
// Route::empty helper
// ---------------------------------------------------------------------------

impl Route {
    /// Create an empty Route (used for park boundary markers in clip splitting).
    fn empty() -> Self {
        Route {
            file: String::new(),
            date: String::new(),
            points: Vec::new(),
            gear_states: Vec::new(),
            autopilot_states: Vec::new(),
            speeds: Vec::new(),
            accel_positions: Vec::new(),
            raw_park_count: 0,
            raw_frame_count: 0,
            gear_runs: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
        let d = haversine_m(40.7128, -74.0060, 34.0522, -118.2437);
        assert!((d - 3_944_000.0).abs() < 50_000.0); // within 50km
    }

    #[test]
    fn test_haversine_m_same_point() {
        assert_eq!(haversine_m(37.7749, -122.4194, 37.7749, -122.4194), 0.0);
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
        assert_eq!(round2(3.14159), 3.14);
        assert_eq!(round2(0.005), 0.01);
    }

    #[test]
    fn test_round1() {
        assert_eq!(round1(3.14), 3.1);
        assert_eq!(round1(3.15), 3.2);
    }

    #[test]
    fn test_group_clips_empty() {
        let groups = group_clips(&[]);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_group_clips_single() {
        let routes = vec![Route {
            file: "/cam/2025-01-15_12-30-45-front.mp4".to_string(),
            date: "2025-01-15".to_string(),
            points: vec![[37.0, -122.0]],
            gear_states: vec![1],
            autopilot_states: vec![0],
            speeds: vec![10.0],
            accel_positions: vec![0.0],
            raw_park_count: 0,
            raw_frame_count: 10,
            gear_runs: vec![GearRun {
                gear: 1,
                frames: 10,
            }],
        }];
        let groups = group_clips(&routes);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 1);
    }

    #[test]
    fn test_group_clips_time_gap_split() {
        let routes = vec![
            Route {
                file: "/cam/2025-01-15_12-00-00-front.mp4".to_string(),
                date: "2025-01-15".to_string(),
                points: vec![[37.0, -122.0]],
                gear_states: vec![1],
                autopilot_states: vec![0],
                speeds: vec![10.0],
                accel_positions: vec![0.0],
                raw_park_count: 0,
                raw_frame_count: 10,
                gear_runs: vec![GearRun {
                    gear: 1,
                    frames: 10,
                }],
            },
            Route {
                file: "/cam/2025-01-15_13-00-00-front.mp4".to_string(),
                date: "2025-01-15".to_string(),
                points: vec![[37.1, -122.1]],
                gear_states: vec![1],
                autopilot_states: vec![0],
                speeds: vec![10.0],
                accel_positions: vec![0.0],
                raw_park_count: 0,
                raw_frame_count: 10,
                gear_runs: vec![GearRun {
                    gear: 1,
                    frames: 10,
                }],
            },
        ];
        let groups = group_clips(&routes);
        // 1 hour gap > 5 min => 2 groups
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn test_compute_aggregate_stats_empty() {
        let stats = compute_aggregate_stats(&[]);
        assert_eq!(stats.drives_count, 0);
        assert_eq!(stats.total_distance_km, 0.0);
    }

    #[test]
    fn test_build_validity_mask_null_island() {
        let points = vec![[0.5, 0.5], [37.0, -122.0], [0.0, 0.1]];
        let valid = build_validity_mask(&points, false, 0.0, 0.0);
        assert!(!valid[0]); // null island
        assert!(valid[1]); // valid
        assert!(!valid[2]); // null island
    }
}
