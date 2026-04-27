use serde::{Deserialize, Serialize};

/// A GPS point as [latitude, longitude].
pub type GpsPoint = [f64; 2];

/// Serde adapter that makes `Vec<u8>` roundtrip-compatible with Go's
/// `encoding/json` treatment of `[]uint8`.
///
/// Go marshals `[]byte` / `[]uint8` as a **standard-base64 string** rather
/// than a JSON array of numbers. So the existing `drive-data.json` files
/// that users already have on disk (produced by the Go server) look like:
///
/// ```json
/// { "gearStates": "AAABAA==" }
/// ```
///
/// A plain `Vec<u8>` field deserializes that as "invalid type: string,
/// expected a sequence". This helper accepts **either** the Go shape
/// (base64 string) **or** the natural JSON shape (array of u8) on read,
/// and always writes the Go shape on export so Sentry Studio and existing
/// archive tooling keep working bit-identically.
pub(crate) mod go_byte_slice {
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::de::{Error, SeqAccess, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S: Serializer>(v: &Vec<u8>, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&STANDARD.encode(v))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Vec<u8>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("base64 string or array of u8")
            }
            fn visit_str<E: Error>(self, s: &str) -> Result<Vec<u8>, E> {
                STANDARD.decode(s).map_err(E::custom)
            }
            fn visit_borrowed_str<E: Error>(self, s: &str) -> Result<Vec<u8>, E> {
                self.visit_str(s)
            }
            fn visit_string<E: Error>(self, s: String) -> Result<Vec<u8>, E> {
                self.visit_str(&s)
            }
            fn visit_unit<E: Error>(self) -> Result<Vec<u8>, E> {
                Ok(Vec::new())
            }
            fn visit_none<E: Error>(self) -> Result<Vec<u8>, E> {
                Ok(Vec::new())
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u8>, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(b) = seq.next_element::<u8>()? {
                    out.push(b);
                }
                Ok(out)
            }
        }
        d.deserialize_any(V)
    }
}

// -----------------------------------------------------------------------------
// Autopilot + Gear constants (match Tesla's Dashcam.proto and Go extract.go).
// Re-exported from extract.rs so consumers don't have to reach into a
// platform-gated module.
// -----------------------------------------------------------------------------

/// Gear state: parked.
pub const GEAR_PARK: u8 = 0;

/// Autopilot state: off / manual driving.
pub const AUTOPILOT_OFF: u8 = 0;
/// Autopilot state: Full Self-Driving (Supervised).
pub const AUTOPILOT_FSD: u8 = 1;
/// Autopilot state: Autopilot (Autosteer).
pub const AUTOPILOT_AUTOSTEER: u8 = 2;
/// Autopilot state: Traffic-Aware Cruise Control.
pub const AUTOPILOT_TACC: u8 = 3;

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
    #[serde(default, with = "go_byte_slice", skip_serializing_if = "Vec::is_empty")]
    pub gear_states: Vec<u8>,
    #[serde(default, with = "go_byte_slice", skip_serializing_if = "Vec::is_empty")]
    pub autopilot_states: Vec<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub speeds: Vec<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accel_positions: Vec<f32>,
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub raw_park_count: u32,
    #[serde(default, skip_serializing_if = "u32_is_zero")]
    pub raw_frame_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gear_runs: Vec<GearRun>,
    /// Provenance: "sei" (native dashcam) or "tessie" (imported from Tessie).
    /// Absent / null defaults to "sei" for backwards compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Stable identifier for a Tessie-imported drive — keeps drives from
    /// merging with each other when time/gear heuristics can't tell them apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_signature: Option<String>,
    /// Tessie-reported autopilot percentage for this drive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tessie_autopilot_percent: Option<f64>,
}

fn u32_is_zero(n: &u32) -> bool { *n == 0 }

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
    // Provenance
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tessie_autopilot_percent: Option<f64>,
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
    // Provenance
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tessie_autopilot_percent: Option<f64>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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

/// Per-clip scalar summary computed once from a Route's BLOB-backed
/// parallel slices. Port of Go `drives.RouteAggregates`.
///
/// Cached as columns on the `routes` table so the Drives-page summary
/// endpoints never have to decode a Points/GearStates/AutopilotStates
/// BLOB to produce a list view. Semantics match Go's
/// `ComputeAggregateStatsFromRoutes` inner loop (null-island filter +
/// GPS-teleport guard, no group-level median); for clean data this is
/// bit-identical to the group-filtered path in `GroupSummaries`.
#[derive(Debug, Clone, Default)]
pub struct RouteAggregates {
    pub distance_m: f64,
    pub max_speed_mps: f64,
    pub avg_speed_mps: f64,
    pub speed_sample_count: i64,
    pub valid_point_count: i64,
    pub fsd_engaged_ms: i64,
    pub autosteer_engaged_ms: i64,
    pub tacc_engaged_ms: i64,
    pub fsd_distance_m: f64,
    pub autosteer_distance_m: f64,
    pub tacc_distance_m: f64,
    pub assisted_distance_m: f64,
    pub fsd_disengagements: i32,
    pub fsd_accel_pushes: i32,
    /// Start/End points are the first/last non-null-island Points on the
    /// clip. `None` when the clip has no valid points — explicit Option
    /// rather than overloading (0, 0) as a sentinel.
    pub start_lat: Option<f64>,
    pub start_lng: Option<f64>,
    pub end_lat: Option<f64>,
    pub end_lng: Option<f64>,
}

/// BLOB-free row shape used by the summary endpoints. Carries the
/// metadata that `groupClips` needs plus all pre-computed scalars from
/// `RouteAggregates`. Reading 5500 summary rows costs ~5 MB of heap
/// versus ~300 MB for the full Route slice.
#[derive(Debug, Clone)]
pub struct RouteSummary {
    pub file: String,
    pub date: String,
    pub raw_park_count: u32,
    pub raw_frame_count: u32,
    pub gear_runs: Vec<GearRun>,
    pub aggregates: RouteAggregates,
    /// Provenance carried through for grouping: "sei" or "tessie".
    pub source: Option<String>,
    /// Tessie external signature for `splitByExternalSignature` grouping.
    pub external_signature: Option<String>,
}

/// Archive-side JSON structure that Sentry Studio reads from the archive
/// server (rsync/CIFS/rclone). Also the payload for
/// `/api/drives/data/download` and `/api/drives/data/upload`.
///
/// Shape is locked by existing Sentry Studio clients; the SQLite store
/// translates to/from this on demand at the archive-sync boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoreData {
    #[serde(default)]
    pub processed_files: Vec<String>,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub drive_tags: std::collections::HashMap<String, Vec<String>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_format_base64_gear_states_deserialize() {
        // Produced by Go's encoding/json on []uint8{0, 1, 0, 0}.
        let json = r#"{"file":"a.mp4","date":"2026-04-20_14-30-00","points":[[40.7,-74.0]],"gearStates":"AAEAAA==","autopilotStates":"AAAAAQ=="}"#;
        let r: Route = serde_json::from_str(json).unwrap();
        assert_eq!(r.gear_states, vec![0, 1, 0, 0]);
        assert_eq!(r.autopilot_states, vec![0, 0, 0, 1]);
    }

    #[test]
    fn rust_array_shape_still_deserializes() {
        // A hand-authored or older Rust-exported file with arrays works too.
        let json = r#"{"file":"a.mp4","date":"2026-04-20_14-30-00","points":[[40.7,-74.0]],"gearStates":[0,1,0,0],"autopilotStates":[0,0,0,1]}"#;
        let r: Route = serde_json::from_str(json).unwrap();
        assert_eq!(r.gear_states, vec![0, 1, 0, 0]);
        assert_eq!(r.autopilot_states, vec![0, 0, 0, 1]);
    }

    #[test]
    fn missing_optional_fields_default_to_empty() {
        let json = r#"{"file":"a.mp4","date":"2026-04-20_14-30-00","points":[[40.7,-74.0]]}"#;
        let r: Route = serde_json::from_str(json).unwrap();
        assert!(r.gear_states.is_empty());
        assert!(r.autopilot_states.is_empty());
        assert!(r.speeds.is_empty());
    }

    #[test]
    fn serializes_back_to_go_base64_shape() {
        let r = Route {
            file: "a.mp4".into(),
            date: "2026-04-20_14-30-00".into(),
            points: vec![[40.7, -74.0]],
            gear_states: vec![0, 1, 0, 0],
            autopilot_states: vec![0, 0, 0, 1],
            speeds: vec![],
            accel_positions: vec![],
            raw_park_count: 0,
            raw_frame_count: 0,
            gear_runs: vec![],
            source: None,
            external_signature: None,
            tessie_autopilot_percent: None,
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""gearStates":"AAEAAA==""#), "serialized: {}", s);
        assert!(s.contains(r#""autopilotStates":"AAAAAQ==""#), "serialized: {}", s);
        // omitempty fields are dropped on export — matches Go output.
        assert!(!s.contains("speeds"), "empty speeds should be omitted: {}", s);
        assert!(!s.contains("rawParkCount"), "zero park count should be omitted: {}", s);
    }
}
