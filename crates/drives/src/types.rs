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

// -----------------------------------------------------------------------------
// Per-frame SEI flag bits (SeiMetadata fields 7/8/9, plus the derived accel
// bit). Mirrors Sentry-Drive's drive-calc.cjs FLAG_* constants exactly.
// -----------------------------------------------------------------------------

/// blinker_on_left (steady switch state, not lamp flash).
pub const FLAG_BLINKER_LEFT: u8 = 1;
/// blinker_on_right.
pub const FLAG_BLINKER_RIGHT: u8 = 2;
/// brake_applied (pedal-only; summon's own braking never sets it).
pub const FLAG_BRAKE: u8 = 4;
/// accelerator_pedal_position > 0 (human-only input).
pub const FLAG_ACCEL: u8 = 8;

/// A contiguous run of a single per-frame SEI flag byte.
///
/// Carried per clip as `flagRuns`: an RLE over RAW SEI frame indices —
/// the same frame space as `gearRuns`, deliberately computed BEFORE GPS
/// point dedup. A stationary car with hazards on produces few unique GPS
/// points, and the summon detector needs those frames, not the survivors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagRun {
    pub flags: u8,
    pub frames: u32,
    /// Max |SEI speed| (m/s, 1 decimal) within this run's frames —
    /// frame-space speed evidence for the summon detector. The park
    /// splitter's fraction→point slice overshoots on deduped points, so
    /// a summon segment's *stats* can inherit the following drive's
    /// speed samples (observed live 2026-07-27 00:34: a 6 mph summon
    /// read 9 mph); per-run maxima confine the speed gate to the
    /// segment's own frames. `None` on runs written before this
    /// evidence existed — the detector then falls back to drive stats.
    /// Mirrors Sentry-Drive's `computeFlagRuns(flags, speeds)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_mps: Option<f64>,
}

/// A single clip's extracted route data (stored in SQLite).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Per-frame SEI flag RLE in RAW frame space (see [`FlagRun`]). Absent
    /// on routes written before flag extraction existed — the summon
    /// detector treats such drives as unverifiable, never as summon.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flag_runs: Vec<FlagRun>,
    /// IMU linear acceleration per deduped point, m/s² (see
    /// [`ExtractedGps::accel_x`]). Empty on routes written before v20 or
    /// on firmware without the SEI IMU fields — the Safety Score falls
    /// back to speed/GPS-derived G-forces for those.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accel_x: Vec<f32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accel_y: Vec<f32>,
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
    // ── Per-clip BLE telemetry rollup (writes to cloud blob) ───────────
    // Mirrors the `routes` table BLE columns populated by
    // `aggregate_telemetry::write_route_telemetry`. All optional — clips
    // whose 60s window had no telemetry samples, or Pis without the BLE
    // feature enabled, leave these NULL. Skip-serialize keeps the wire +
    // cloud blob compact when telemetry isn't present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_pct_start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_pct_end: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interior_temp_min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interior_temp_max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exterior_temp_avg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hvac_runtime_s: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_fl_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_fr_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_rl_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_rr_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odometer_mi_start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odometer_mi_end: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_name_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location_name_end: Option<String>,
    /// Per-clip temperature samples (see [`TempSample`]). Populated at
    /// cloud-upload time from `telemetry_samples`; empty on Pis without
    /// BLE telemetry and absent from their wire JSON.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub temp_samples: Vec<TempSample>,
}

fn u32_is_zero(n: &u32) -> bool { *n == 0 }

fn bool_is_false(b: &bool) -> bool { !*b }

/// One telemetry temperature sample inside a clip's 60s window —
/// uploaded with the route blob so the cloud can chart the drive's
/// interior/exterior temperature series (the cloud has no access to
/// the local telemetry DB). Keys are single letters to keep the wire
/// compact at one to two samples per clip.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TempSample {
    /// Unix seconds.
    pub t: i64,
    /// Interior temperature, °C.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub i: Option<f32>,
    /// Exterior temperature, °C.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub e: Option<f32>,
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
    /// Summon / Smart Summon drive (see `grouper::detect_summon`).
    /// Emitted only when true, matching Sentry-Drive's
    /// `...(summon ? { summon: true } : {})`.
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub summon: bool,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
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
    /// Summon / Smart Summon drive (see `grouper::detect_summon`).
    /// Emitted only when true, matching Sentry-Drive's
    /// `...(summon ? { summon: true } : {})`.
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub summon: bool,
    // ── v6 BLE telemetry rollup ────────────────────────────────────────
    // Aggregated across the drive's clips from `telemetry_samples`.
    // All optional — pre-telemetry drives, drives that never crossed a
    // sample, and routes whose clip window had no samples in it all
    // render as omitted (`skip_serializing_if`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_pct_start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_pct_end: Option<f64>,
    /// Convenience scalar: `battery_pct_start - battery_pct_end`,
    /// rounded to two decimals. Computed in `build_summary_from_aggregates`
    /// so the UI doesn't have to derive it (and to avoid floating-point
    /// surprises across language boundaries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery_pct_used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interior_temp_min_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interior_temp_max_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exterior_temp_avg_c: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hvac_runtime_s: Option<i64>,
    // v7 TPMS rollup — latest non-null PSI per tire across the
    // drive's clips. Omitted when null so the UI naturally hides
    // the TPMS section for cars without it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_fl_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_fr_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_rl_psi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tire_rr_psi: Option<f64>,
    // v9 odometer + software/FSD version. Per-drive odometer delta
    // is the more reliable "distance" number than GPS (no curve
    // overestimation, no drift). FSD version is only emitted when
    // the drive actually had FSD engaged AND we know what release
    // the car was running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odometer_mi_start: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odometer_mi_end: Option<f64>,
    /// `end - start`, rounded to one decimal — UI convenience scalar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub odometer_mi_driven: Option<f64>,
    /// v10 drive start/end labels. Either Tesla's reverse-geocoded
    /// address string OR "Home" / "Work" when the drive's start/end
    /// GPS matched the user's saved home/work coords (within ~150m).
    /// The matcher prefers "Home"/"Work" over raw addresses for
    /// drives that begin or end at known places.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_location: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_location: Option<String>,
    // Provenance
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_signature: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tessie_autopilot_percent: Option<f64>,
    /// v18 Safety Score totals for this drive (see `safety.rs`). All
    /// `#[serde(default)]` so pre-v18 `drive_list_cache` JSON still
    /// deserializes on the append fast path.
    #[serde(default)]
    pub safety_hard_brake_ms: i64,
    #[serde(default)]
    pub safety_hard_brake_events: i32,
    #[serde(default)]
    pub safety_aggr_turn_ms: i64,
    #[serde(default)]
    pub safety_aggr_turn_events: i32,
    #[serde(default)]
    pub safety_speeding_ms: i64,
    #[serde(default)]
    pub safety_moving_ms: i64,
    #[serde(default)]
    pub safety_manual_moving_ms: i64,
    #[serde(default)]
    pub safety_night_ms: i64,
    #[serde(default)]
    pub safety_night_mi: f64,
    /// Night miles scaled by the per-hour risk curve (`night_weight`).
    #[serde(default)]
    pub safety_night_weighted_mi: f64,
    #[serde(default)]
    pub safety_brake_any_ms: i64,
    #[serde(default)]
    pub safety_turn_any_ms: i64,
    /// This drive's own 0–100 score, `None` when the drive is too short,
    /// imported, a summon session, or carries no SEI safety data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub safety_score: Option<f64>,
}

// AggregateStats was removed with the Route-walking stats path — the
// drive-stats JSON contract (snake_case keys, matches the old Go API)
// is built directly in db.rs::compute_drive_caches.

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

/// Daily Safety Score row for the analytics breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetyDayStats {
    pub date: String,
    pub day_name: String,
    /// Day's own score, `None` when the day's mileage is below the
    /// scoring floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    pub drives: i32,
    pub distance_mi: f64,
    pub distance_km: f64,
    pub hard_brake_events: i32,
    pub aggr_turn_events: i32,
    pub speeding_ms: i64,
    pub night_mi: f64,
    /// Day-level time denominators/numerators so the UI can chart each
    /// factor as a daily PROPORTION (mirroring the Tesla app's per-factor
    /// daily graphs) instead of raw counts.
    pub moving_ms: i64,
    pub manual_moving_ms: i64,
    pub hard_brake_ms: i64,
    pub aggr_turn_ms: i64,
    /// v19 conditional denominators (braking >0.1g / turning >0.2g time).
    pub brake_any_ms: i64,
    pub turn_any_ms: i64,
}

/// Safety Score analytics over a period. Totals are summed across the
/// period's SEI drives and scored ONCE (mileage/time-weighted by
/// construction) — never an average of per-drive scores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SafetyAnalytics {
    pub period: String,
    pub period_start: String,
    pub total_drives: i32,
    /// Drives that carried safety data (SEI, non-summon, non-imported).
    pub scored_drives: i32,
    /// The window score + factor breakdown, `None` when the window has
    /// too little scored driving.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<crate::safety::SafetyScore>,
    pub total_distance_mi: f64,
    pub total_distance_km: f64,
    pub moving_ms: i64,
    pub manual_moving_ms: i64,
    pub hard_brake_events: i32,
    pub hard_brake_ms: i64,
    pub aggr_turn_events: i32,
    pub aggr_turn_ms: i64,
    pub speeding_ms: i64,
    pub brake_any_ms: i64,
    pub turn_any_ms: i64,
    pub night_mi: f64,
    pub night_km: f64,
    pub assisted_percent: f64,
    /// ALL FSD/Autosteer disengagements in the period. Informational
    /// only (weight 0 in the score): the SEI stream cannot distinguish
    /// Tesla's "forced" disengagements (strikeouts) from normal driver
    /// takeovers.
    pub fsd_disengagements: i32,
    pub daily: Vec<SafetyDayStats>,
    pub best_day: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub best_day_score: Option<f64>,
}

/// Overview route for map display (downsampled points).
///
/// `start_time` is included so the Drives list page can reliably
/// associate a route with its DriveSummary entry: the list cache
/// uses sub-clip-aware grouping (`group_summary_clips`) while this
/// endpoint uses `group_clips` (full-clip), so the integer `id`
/// indices CAN drift apart when a clip is split mid-way. Joining on
/// the formatted start_time string is robust to that drift.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteOverview {
    pub id: i32,
    pub points: Vec<GpsPoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub start_time: String,
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
    pub flag_runs: Vec<FlagRun>,
    /// IMU linear acceleration per deduped point, m/s² (SEI proto fields
    /// 14/15): X lateral (positive = rightward force), Y longitudinal
    /// (negative = deceleration). Peak-|value| per collapsed GPS run.
    /// Empty when the clip's firmware doesn't emit the IMU fields.
    pub accel_x: Vec<f32>,
    pub accel_y: Vec<f32>,
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

/// Result of the targeted summon evidence re-read (see
/// `Processor::check_summon`). Field names mirror Sentry-Drive's
/// `check-summon` IPC result payload.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummonCheckOutcome {
    pub candidate_drives: usize,
    pub clips_scanned: usize,
    pub updated_routes: usize,
    pub missing_clips: usize,
}

/// Internal timed route used during grouping.
#[derive(Debug, Clone)]
pub struct TimedRoute {
    pub route: Route,
    pub timestamp: chrono::NaiveDateTime,
}

/// Per-clip scalar summary computed once from a Route's BLOB-backed
/// parallel slices.
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
    /// v15 clip-boundary state. A per-clip aggregate can't see across the
    /// 60s seam, so edge transitions export STATE for the drive grouper to
    /// resolve — mirroring the cloud summary v4 boundary fields
    /// (`dPnd`/`pk1`) so local and cloud counts agree.
    ///
    /// Pending FSD disengagement still open at clip end: elapsed ms since
    /// the FSD→off transition (0..=2000). The grouper checks the next
    /// clip's `park_ms_start` against the remaining grace window — Park
    /// inside it means "FSD parked the car" (not a disengagement),
    /// anything else counts.
    pub fsd_pend_ms_end: Option<f64>,
    /// First Park gear sample within 2000 ms of clip start, in clip-ms.
    pub park_ms_start: Option<f64>,
    /// FSD engaged on the clip's final sample — the next clip is an
    /// engaged continuation, so its `fsd_accel_pushes_early` are real.
    pub fsd_at_end: bool,
    /// Accel pushes that began <3000 ms into a clip that started already
    /// FSD-engaged. In-clip the 3s engage grace can't be proven satisfied
    /// (the engagement began in an earlier clip), so these export
    /// separately and the grouper counts them only when the previous clip
    /// ended FSD-engaged.
    pub fsd_accel_pushes_early: i32,
    /// Autopilot mode on the clip's FIRST sample (AUTOPILOT_* constant),
    /// `None` when the clip has no AP data. The grouper attributes the
    /// inter-clip bridge segment and seam wall-time to this mode —
    /// Sentry-Drive's merged point walk does the same implicitly (the
    /// crossing segment's dt/distance lands on the next point's state).
    pub ap_at_start: Option<i32>,
    /// Start/End points are the first/last non-null-island Points on the
    /// clip. `None` when the clip has no valid points — explicit Option
    /// rather than overloading (0, 0) as a sentinel.
    pub start_lat: Option<f64>,
    pub start_lng: Option<f64>,
    pub end_lat: Option<f64>,
    pub end_lng: Option<f64>,
    /// v16: max **absolute** SEI speed (m/s) across the clip's samples,
    /// `None` when the clip has no SEI speed channel. The locked
    /// `max_speed_mps` drops negative (Reverse) samples; summon detection
    /// needs the magnitude — a reverse-only summon crawl would otherwise
    /// read 0 m/s and fail the `> 0` gate. Also distinguishes SEI from
    /// GPS-derived speed, which jitters past walking pace on a car that
    /// barely moved (mirrors Sentry-Drive's `hasSeiSpeeds` gate).
    pub sei_speed_abs_max: Option<f64>,
    /// v18 Safety Score scalars (see `safety.rs`). `None` = row not yet
    /// backfilled; a clip without a usable SEI speed channel backfills to
    /// `Some(0)` everywhere (no safety data, honestly recorded).
    pub safety_hard_brake_ms: Option<i64>,
    pub safety_hard_brake_events: Option<i64>,
    pub safety_aggr_turn_ms: Option<i64>,
    pub safety_aggr_turn_events: Option<i64>,
    pub safety_speeding_ms: Option<i64>,
    pub safety_moving_ms: Option<i64>,
    pub safety_manual_moving_ms: Option<i64>,
    /// v19 conditional-ratio denominators (see `safety.rs`): manual time
    /// decelerating above 0.1g / turning above 0.2g.
    pub safety_brake_any_ms: Option<i64>,
    pub safety_turn_any_ms: Option<i64>,
}

/// Per-clip telemetry rollup populated from `telemetry_samples` rows
/// whose `ts` falls inside the clip's 60s window. Defined here (rather
/// than in `aggregate_telemetry.rs`) so `RouteSummary` can embed it
/// without a circular module dependency.
///
/// Note: the `battery_temp_avg` column on the routes table is left
/// nullable in the schema for forward compatibility but is no longer
/// populated — Tesla's state API doesn't expose battery cell temp.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RouteTelemetryAggregates {
    pub battery_pct_start: Option<f64>,
    pub battery_pct_end: Option<f64>,
    pub interior_temp_min: Option<f64>,
    pub interior_temp_max: Option<f64>,
    pub exterior_temp_avg: Option<f64>,
    pub hvac_runtime_s: Option<i64>,
    /// v7 TPMS — latest non-null pressure per tire (PSI) within the
    /// clip's 60s window. All four None on cars without TPMS.
    pub tire_fl_psi: Option<f64>,
    pub tire_fr_psi: Option<f64>,
    pub tire_rl_psi: Option<f64>,
    pub tire_rr_psi: Option<f64>,
    /// v9 odometer (miles, Tesla native unit). First / last non-null
    /// reading within the clip's window.
    pub odometer_mi_start: Option<f64>,
    pub odometer_mi_end: Option<f64>,
    /// v10 Tesla-supplied address strings — first / last non-null
    /// `location_name` sample in the clip's window. The per-drive
    /// rollup chains these across all the clips of a trip to
    /// produce the start/end labels surfaced in the UI.
    pub location_name_start: Option<String>,
    pub location_name_end: Option<String>,
}
// `software_version` removed from the rollup — Tesla doesn't expose
// `car_version` over BLE, so we never populated it. The DB column on
// `routes` stays nullable as forward-compat.

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
    /// Raw-frame-space SEI flag RLE (see [`FlagRun`]). Empty on rows
    /// written before flag extraction — those drives are summon-
    /// unverifiable by definition.
    pub flag_runs: Vec<FlagRun>,
    pub aggregates: RouteAggregates,
    /// Provenance carried through for grouping: "sei" or "tessie".
    pub source: Option<String>,
    /// Tessie external signature for `splitByExternalSignature` grouping.
    pub external_signature: Option<String>,
    /// v6 BLE telemetry rollup. Defaults to all-None for routes that
    /// predate the sampler or whose window never overlapped a sample.
    pub telemetry: RouteTelemetryAggregates,
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
            ..Default::default()
        };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains(r#""gearStates":"AAEAAA==""#), "serialized: {}", s);
        assert!(s.contains(r#""autopilotStates":"AAAAAQ==""#), "serialized: {}", s);
        // omitempty fields are dropped on export.
        assert!(!s.contains("speeds"), "empty speeds should be omitted: {}", s);
        assert!(!s.contains("rawParkCount"), "zero park count should be omitted: {}", s);
    }
}
