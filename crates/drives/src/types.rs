use serde::{Deserialize, Serialize};

/// A GPS point as [latitude, longitude].
pub type GpsPoint = [f64; 2];

/// A contiguous run of a single gear state across frames.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GearRun {
    pub gear: u8,
    pub frames: u32,
}

/// A single clip's extracted route data (stored in SQLite).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Route {
    pub file: String,
    pub date: String,
    pub points: Vec<GpsPoint>,
    pub gear_states: Vec<u8>,
    pub autopilot_states: Vec<u8>,
    pub speeds: Vec<f32>,
    pub accel_positions: Vec<f32>,
    pub raw_park_count: u32,
    pub raw_frame_count: u32,
    pub gear_runs: Vec<GearRun>,
}

/// FSD event location (disengagement or accel push).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FsdEvent {
    pub lat: f64,
    pub lng: f64,
    #[serde(rename = "type")]
    pub event_type: String,
}

/// A grouped drive (multiple clips forming a single trip) — full point data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: i32,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub duration_ms: i64,
    pub distance_mi: f64,
    pub distance_km: f64,
    pub avg_speed_mph: f64,
    pub max_speed_mph: f64,
    pub avg_speed_kmh: f64,
    pub max_speed_kmh: f64,
    pub clip_count: usize,
    pub point_count: usize,
    pub points: Vec<[f64; 4]>,  // [lat, lng, timeMs, speedMps]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gear_states: Vec<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fsd_states: Vec<i32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fsd_events: Vec<FsdEvent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    // FSD analytics (state=1 — Full Self-Driving)
    pub fsd_engaged_ms: i64,
    pub fsd_disengagements: i32,
    pub fsd_accel_pushes: i32,
    pub fsd_percent: f64,
    pub fsd_distance_km: f64,
    pub fsd_distance_mi: f64,
    // Autosteer (state=2)
    pub autosteer_engaged_ms: i64,
    pub autosteer_percent: f64,
    pub autosteer_distance_km: f64,
    pub autosteer_distance_mi: f64,
    // TACC (state=3)
    pub tacc_engaged_ms: i64,
    pub tacc_percent: f64,
    pub tacc_distance_km: f64,
    pub tacc_distance_mi: f64,
    // Assisted aggregate
    pub assisted_percent: f64,
}

/// Lightweight drive summary (no full point arrays) for list views.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveSummary {
    pub id: i32,
    pub date: String,
    pub start_time: String,
    pub end_time: String,
    pub duration_ms: i64,
    pub distance_mi: f64,
    pub distance_km: f64,
    pub avg_speed_mph: f64,
    pub max_speed_mph: f64,
    pub avg_speed_kmh: f64,
    pub max_speed_kmh: f64,
    pub clip_count: usize,
    pub point_count: usize,
    pub start_point: Option<GpsPoint>,
    pub end_point: Option<GpsPoint>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    // FSD analytics (state=1)
    pub fsd_engaged_ms: i64,
    pub fsd_disengagements: i32,
    pub fsd_accel_pushes: i32,
    pub fsd_percent: f64,
    pub fsd_distance_km: f64,
    pub fsd_distance_mi: f64,
    // Autosteer (state=2)
    pub autosteer_engaged_ms: i64,
    pub autosteer_percent: f64,
    pub autosteer_distance_km: f64,
    pub autosteer_distance_mi: f64,
    // TACC (state=3)
    pub tacc_engaged_ms: i64,
    pub tacc_percent: f64,
    pub tacc_distance_km: f64,
    pub tacc_distance_mi: f64,
    // Assisted aggregate
    pub assisted_percent: f64,
}

/// Aggregate statistics across all drives.
/// Note: uses snake_case JSON to match Go API output expected by the frontend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AggregateStats {
    pub drives_count: usize,
    pub routes_count: usize,
    pub processed_count: usize,
    pub total_distance_km: f64,
    pub total_distance_mi: f64,
    pub total_duration_ms: i64,
    pub fsd_engaged_ms: i64,
    pub fsd_distance_km: f64,
    pub fsd_distance_mi: f64,
    pub fsd_percent: f64,
    pub fsd_disengagements: i32,
    pub fsd_accel_pushes: i32,
    pub autosteer_engaged_ms: i64,
    pub autosteer_distance_km: f64,
    pub autosteer_distance_mi: f64,
    pub tacc_engaged_ms: i64,
    pub tacc_distance_km: f64,
    pub tacc_distance_mi: f64,
    pub assisted_percent: f64,
}

/// Daily FSD statistics for analytics breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsdDayStats {
    pub date: String,
    pub day_name: String,
    pub disengagements: i32,
    pub accel_pushes: i32,
    pub fsd_percent: f64,
    pub drives: i32,
    pub fsd_distance_km: f64,
    pub fsd_distance_mi: f64,
    pub total_duration_ms: i64,
    pub fsd_engaged_ms: i64,
}

/// FSD analytics with daily/weekly breakdowns.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FsdAnalytics {
    pub period: String,
    pub period_start: String,
    pub total_drives: i32,
    pub fsd_sessions: i32,
    pub fsd_percent: f64,
    pub today_percent: f64,
    pub best_day: String,
    pub best_day_percent: f64,
    pub fsd_engaged_ms: i64,
    pub fsd_distance_km: f64,
    pub fsd_distance_mi: f64,
    pub total_distance_km: f64,
    pub total_distance_mi: f64,
    pub disengagements: i32,
    pub accel_pushes: i32,
    pub daily: Vec<FsdDayStats>,
    pub fsd_grade: String,
    pub streak_days: i32,
    pub fsd_time_formatted: String,
    pub avg_disengagements_per_drive: f64,
    pub avg_accel_pushes_per_drive: f64,
    pub autosteer_engaged_ms: i64,
    pub autosteer_distance_km: f64,
    pub autosteer_distance_mi: f64,
    pub tacc_engaged_ms: i64,
    pub tacc_distance_km: f64,
    pub tacc_distance_mi: f64,
    pub assisted_percent: f64,
}

/// Overview route for map display (downsampled points).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteOverview {
    pub id: i32,
    pub points: Vec<GpsPoint>,
}

/// Extracted GPS data from a single MP4 file.
#[derive(Debug, Clone)]
pub struct ExtractedGps {
    pub points: Vec<GpsPoint>,
    pub gear_states: Vec<u8>,
    pub autopilot_states: Vec<u8>,
    pub speeds: Vec<f32>,
    pub accel_positions: Vec<f32>,
    pub raw_park_count: u32,
    pub raw_frame_count: u32,
    pub gear_runs: Vec<GearRun>,
}

/// Processing progress status.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessingStatus {
    pub running: bool,
    pub total_files: usize,
    pub processed_files: usize,
    pub current_file: Option<String>,
}

/// Internal timed route used during grouping.
#[derive(Debug, Clone)]
pub struct TimedRoute {
    pub route: Route,
    pub timestamp: chrono::NaiveDateTime,
}
