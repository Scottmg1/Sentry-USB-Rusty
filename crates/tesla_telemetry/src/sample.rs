//! Shared result/state types for the telemetry sampler.
//!
//! The actual sampling lives in `sample_ble.rs` (in-process BLE over a
//! `PersistentSession`). This module holds the result structs, the
//! flattened Tesla enums, and the `Sample` row shape the DB writer
//! consumes. The original shell-out path (`tesla-control` + permissive
//! JSON field probing) that these types were built for was removed once
//! every call site moved to the in-process path.

/// Tesla shift state from `state drive`'s `shiftState` (string
/// "P"/"R"/"N"/"D" or protobuf int P=1..D=4). The phase machine uses it
/// to pick parked-and-recording (sleep-safe polling) vs driving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftState {
    Park,
    Reverse,
    Neutral,
    Drive,
    /// Returned but value didn't match any known mapping. Treated
    /// as not-Park to avoid spuriously back-off-ing during real
    /// driving on a newer SDK.
    Unknown,
}

impl ShiftState {
    pub fn is_park(self) -> bool {
        matches!(self, ShiftState::Park)
    }
}

/// Tesla `ChargeState.charging_state` oneof, flattened so the phase
/// machine can match without proto types. Quiet-mode gate: Starting /
/// Charging / Calibrating keep the car awake (stay Active); everything
/// else (incl. Unknown) is a quiet-path candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChargingState {
    Unknown,
    Disconnected,
    NoPower,
    Starting,
    Charging,
    Complete,
    Stopped,
    Calibrating,
}

impl ChargingState {
    /// The three states that keep the car awake on its own; the sampler
    /// stays Active while true even if shift_state is Park (quieting
    /// would leave battery % stale mid-charge).
    pub fn is_active_charging(self) -> bool {
        matches!(
            self,
            ChargingState::Starting | ChargingState::Charging | ChargingState::Calibrating
        )
    }

    /// Stable lowercase string persisted to `telemetry_samples.charging_state`.
    /// The api crate (which can't depend on this binary crate) string-matches
    /// these to decide whether the dashboard banner stays in its charging
    /// state — so the spellings here are the wire contract; keep them in sync
    /// with `phase_is_active` in `crates/api/src/charging.rs`.
    pub fn as_db_str(self) -> &'static str {
        match self {
            ChargingState::Unknown => "unknown",
            ChargingState::Disconnected => "disconnected",
            ChargingState::NoPower => "nopower",
            ChargingState::Starting => "starting",
            ChargingState::Charging => "charging",
            ChargingState::Complete => "complete",
            ChargingState::Stopped => "stopped",
            ChargingState::Calibrating => "calibrating",
        }
    }
}

/// Tesla `ClosuresState.SentryModeState` oneof, flattened. Mirrors the
/// proto's six states; `Off` is the only one where the car will sleep
/// on its own (the other five all hold the car awake monitoring).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentryMode {
    Off,
    Idle,
    Armed,
    Aware,
    Panic,
    /// "Quiet Sentry" — alarm is suppressed but the system is still
    /// armed and recording. Not to be confused with the sampler's
    /// own quiet-poll mode (unfortunate naming collision in the
    /// Tesla proto).
    Quiet,
}

impl SentryMode {
    /// Anything that isn't `Off` keeps the car awake. Used by the
    /// quiet-mode gate alongside `ChargingState::is_active_charging`.
    pub fn is_on(self) -> bool {
        !matches!(self, SentryMode::Off)
    }
}

/// Common fields every state response carries that downstream code
/// might want regardless of which specific sub-sampler it was. The
/// `vehicle_ts_secs` field is what the clock-sync feature uses to
/// keep the Pi's wall clock honest — Tesla derives this from GPS
/// time so it's always accurate.
#[derive(Default, Clone, Copy)]
pub struct ResponseMeta {
    /// Tesla's wall-clock timestamp from the response body, parsed
    /// from the RFC 3339 `timestamp` field to ms-since-epoch
    /// precision (Tesla includes fractional seconds like `.794Z` —
    /// we preserve them so clock-sync doesn't introduce ~500ms of
    /// rounding error). None when not present or unparseable.
    /// Used by `clock_sync::maybe_set_clock_from_vehicle`.
    pub vehicle_ts_ms: Option<i64>,
    /// Monotonic Instant from just before we sent the BLE request.
    /// Used for diagnostic RTT logging when clock-sync fires.
    pub request_started_at: Option<std::time::Instant>,
}

/// Result of a successful `sample_drive` call. Drive is the
/// highest-priority poll because it carries the three signals that
/// must stay fresh during a drive:
///   * `shift_state` — phase-machine input (drive detection)
///   * `location_name` — Tesla's reverse-geocoded address
///   * `odometer_mi` — mile counter, ticks continuously while driving
pub struct DriveResult {
    pub location_name: Option<String>,
    pub odometer_mi: Option<f64>,
    pub shift_state: Option<ShiftState>,
    /// Raw GPS from the bundled LocationState (same round-trip). `None`
    /// on parked-and-unchanged polls. Feeds the keep-accessory geofence.
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub meta: ResponseMeta,
    /// Live speed / power / active-route detail. Decoded only when the
    /// experimental flag is on; inert otherwise.
    pub detail: DriveDetail,
}

/// Live driving + navigation detail from the BLE `DriveState` message.
#[derive(Debug, Clone, Default)]
pub struct DriveDetail {
    /// Instantaneous speed, mph.
    pub speed_mph: Option<f32>,
    /// Instantaneous drive power, kW (negative = regen).
    pub power_kw: Option<i32>,
    /// Active-route destination label, if navigating.
    pub route_destination: Option<String>,
    /// Estimated minutes to arrival on the active route.
    pub route_minutes_to_arrival: Option<f32>,
    /// Miles remaining to arrival on the active route.
    pub route_miles_to_arrival: Option<f32>,
}

/// Result of a successful `sample_climate` call. Slow-changing —
/// polled at a coarser cadence than `sample_drive`.
pub struct ClimateResult {
    pub interior_temp_c: Option<f64>,
    pub exterior_temp_c: Option<f64>,
    pub hvac_on: Option<bool>,
    pub meta: ResponseMeta,
    /// Setpoints / fan / defroster / seat-heater / preconditioning detail.
    /// Decoded only when the experimental flag is on; inert otherwise.
    pub detail: ClimateDetail,
}

/// Extended climate detail from the BLE `ClimateState` message.
/// Temperatures are Celsius; seat-heater / fan levels are Tesla's raw
/// integer steps (0 = off).
#[derive(Debug, Clone, Default)]
pub struct ClimateDetail {
    pub driver_setpoint_c: Option<f32>,
    pub passenger_setpoint_c: Option<f32>,
    pub fan_status: Option<i32>,
    pub front_defroster_on: Option<bool>,
    pub rear_defroster_on: Option<bool>,
    pub preconditioning: Option<bool>,
    pub seat_heater_left: Option<i32>,
    pub seat_heater_right: Option<i32>,
}

// LocationResult was removed: standalone `state location` queries
// don't return location_name (only GPS coords). Tesla returns the
// address in the LocationState bundled into `state drive`
// responses, so DriveResult now carries it directly. This struct
// would only resurface if we added a UI surface for GPS coords.

/// Result of a successful `sample_charge` call. Slow-changing.
///
/// `charging_state` is in-memory only (not persisted to the DB) — the
/// phase machine in `main.rs` reads it on each tick to decide whether
/// it's safe to drop into quiet polling. `None` means we couldn't
/// extract it from the response; the gate treats unknown as
/// "stay Active" to avoid wrongly quieting down during a real charge
/// session.
pub struct ChargeResult {
    pub battery_pct: Option<f64>,
    pub charging_state: Option<ChargingState>,
    pub meta: ResponseMeta,
    /// Expanded charging detail. Always decoded (cheap, read-only), but
    /// only consumed/logged when the experimental flag is on, so a
    /// normal install is unaffected. Persistence + API/web surfacing is
    /// a follow-up once the decode is validated on real hardware.
    pub detail: ChargeDetail,
}

/// Extra fields from the BLE `ChargeState` message that the car already
/// sends but the sampler didn't previously surface. All optional — a
/// field is `None` when the car didn't report it this poll.
#[derive(Debug, Clone, Default)]
pub struct ChargeDetail {
    /// Actual current the charger is delivering, amps.
    pub charger_actual_current_a: Option<i32>,
    /// Charging power, kW.
    pub charger_power_kw: Option<i32>,
    /// Charger input voltage, volts.
    pub charger_voltage_v: Option<i32>,
    /// Set/requested charging amps.
    pub charging_amps_set: Option<i32>,
    /// Vehicle-reported maximum charging current, amps.
    pub charge_current_request_max: Option<i32>,
    /// Charge rate, mi/hr added.
    pub charge_rate_mph: Option<f32>,
    /// Energy added this session, kWh.
    pub charge_energy_added_kwh: Option<f32>,
    /// Charge limit (target SoC), percent.
    pub charge_limit_soc: Option<i32>,
    /// Estimated minutes to full charge.
    pub minutes_to_full_charge: Option<i32>,
    /// Rated battery range, miles.
    pub battery_range_mi: Option<f32>,
    /// Charge port door open.
    pub charge_port_door_open: Option<bool>,
}

/// Result of a successful `sample_closures` call. In-memory only —
/// the only field we read from closures right now is sentry mode (for
/// the quiet-mode gate), no DB persistence. If we later want to
/// surface door / window / charge-port state in the UI, this is the
/// right place to add fields.
pub struct ClosuresResult {
    pub sentry_mode: Option<SentryMode>,
    pub meta: ResponseMeta,
    /// Door / window / lock / trunk state. Decoded only when the
    /// experimental flag is on; inert otherwise. Each field is None when
    /// the car didn't report it this poll.
    pub detail: ClosuresDetail,
}

/// Security-relevant closure state from the BLE `ClosuresState` message.
/// `true` = open / unlocked. `frunk` is the front trunk, `trunk` the rear.
#[derive(Debug, Clone, Default)]
pub struct ClosuresDetail {
    pub locked: Option<bool>,
    pub door_driver_front_open: Option<bool>,
    pub door_driver_rear_open: Option<bool>,
    pub door_passenger_front_open: Option<bool>,
    pub door_passenger_rear_open: Option<bool>,
    pub frunk_open: Option<bool>,
    pub trunk_open: Option<bool>,
    pub window_driver_front_open: Option<bool>,
    pub window_passenger_front_open: Option<bool>,
    pub window_driver_rear_open: Option<bool>,
    pub window_passenger_rear_open: Option<bool>,
    /// Sunroof opening as a percentage (0 = closed).
    pub sunroof_percent_open: Option<i32>,
}

/// Result of a successful `sample_tires` call. Very slow-changing —
/// polled at the coarsest cadence (every few minutes).
pub struct TiresResult {
    pub tire_fl_psi: Option<f64>,
    pub tire_fr_psi: Option<f64>,
    pub tire_rl_psi: Option<f64>,
    pub tire_rr_psi: Option<f64>,
    pub meta: ResponseMeta,
}

/// Result of a body-controller-state probe. The signal fields are
/// in-memory inputs for the phase machine — they ride the sample
/// row so it gets persisted with a body_controller source marker,
/// but the flags themselves aren't stored.
pub struct BodyControllerSample {
    pub sample: Sample,
    /// Driver-seat occupancy. Used to detect "user got back in"
    /// while in body-controller-only mode so the sampler can
    /// promote to full state polling without waiting for the
    /// 15-min asleep cycle.
    pub user_presence: Option<bool>,
    /// VCSEC vehicleSleepStatus collapsed to a bool (AWAKE → true,
    /// ASLEEP → false, UNKNOWN → None). Sleep-safe awake signal for
    /// Quiet mode: cam-disk mtime goes stale whenever the car isn't
    /// recording (e.g. charging with Sentry off), so this is the only
    /// awake indicator that survives a sampler restart mid-charge.
    pub awake: Option<bool>,
}

/// A single point-in-time observation, in the shape the DB writer
/// wants. All fields except `ts` and `source` are nullable because
/// different sample paths populate different subsets.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    pub ts: i64,
    pub battery_pct: Option<f64>,
    pub battery_temp_c: Option<f64>,
    pub interior_temp_c: Option<f64>,
    pub exterior_temp_c: Option<f64>,
    pub hvac_on: Option<bool>,
    // TPMS pressures in PSI. All four optional — cars without TPMS
    // (or runs where the `state tire-pressure` call fails / times
    // out) just leave these as None and the UI hides the row.
    pub tire_fl_psi: Option<f64>,
    pub tire_fr_psi: Option<f64>,
    pub tire_rl_psi: Option<f64>,
    pub tire_rr_psi: Option<f64>,
    /// Odometer in miles (Tesla native unit). Sampled every awake
    /// cycle — ticks continuously while driving.
    pub odometer_mi: Option<f64>,
    /// Tesla's reverse-geocoded address string for the car's
    /// current location. Pulled from `state drive`. Used as
    /// drive start/end labels in the UI.
    pub location_name: Option<String>,
    // Charging detail (v11). Populated from ChargeResult.detail only
    // when the experimental flag is on; None otherwise, so a normal
    // install writes the same rows it always has. Powers the charging
    // view under "Driving".
    pub charger_power_kw: Option<i32>,
    pub charger_actual_current_a: Option<i32>,
    pub charger_voltage_v: Option<i32>,
    pub charge_rate_mph: Option<f32>,
    pub charge_energy_added_kwh: Option<f32>,
    pub charge_limit_soc: Option<i32>,
    pub battery_range_mi: Option<f32>,
    // Charging-control telemetry (v17). These values are used only from
    // fresh state samples; stale persisted values never enable controls.
    pub charging_amps_set: Option<i32>,
    pub charge_current_request_max: Option<i32>,
    pub charge_port_door_open: Option<bool>,
    // Estimated minutes to full charge (v13). Drives the dashboard
    // "charging" banner's time-to-full readout.
    pub charge_minutes_to_full: Option<i32>,
    // Persisted charge phase (v14), lowercase via ChargingState::as_db_str.
    // Was in-memory only before; persisting it lets /api/charging/current
    // keep the banner up the entire charge across multi-minute BLE sampler
    // dropouts (the phase only leaves "charging" when a poll actually
    // reports a stopped/complete phase, not when a sample goes stale).
    pub charging_state: Option<String>,
    // Raw GPS (v12). Populated from the bundled LocationState only when
    // the experimental flag is on; None otherwise. Lets a parked-and-
    // charging sample carry the charger's location for the map pin.
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
    pub source: String,
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

