//! In-process Tesla BLE sampler.
//!
//! Same result types as the shell-out paths in `sample.rs` (DriveResult,
//! ClimateResult, etc.), but over a `PersistentSession` held by main.rs,
//! so a warm query lands in ~250-350 ms vs the ~1.5-2 s shell-out path.
//! The `body_controller` path stays unauthenticated and works against a
//! sleeping car.

use std::time::Instant;

use anyhow::Result;
use sentryusb_tesla_ble::{
    keys::KeyPair, manager::PersistentSession, proto::car_server,
};
use tracing::{info, warn};

use crate::sample::{
    BodyControllerSample, ChargeDetail, ChargeResult, ChargingState, ClimateDetail, ClimateResult,
    ClosuresDetail, ClosuresResult, DriveDetail, DriveResult, ResponseMeta, Sample, SentryMode,
    ShiftState, TiresResult, now_secs,
};

/// 1 bar = 14.5038 psi (NIST). Tesla reports TPMS in bar on the wire.
/// Rounded to 1 decimal so the DB doesn't carry FP noise we can't
/// observe at display time. Mirrors the helper in sample.rs.
fn bar_to_psi(bar: f64) -> f64 {
    ((bar * 14.5038) * 10.0).round() / 10.0
}

/// Build a ResponseMeta from a car_server `google.protobuf.Timestamp`
/// (every state response carries one in the relevant sub-message).
/// Mirrors `sample::build_meta` but for proto-typed inputs.
fn build_meta(ts: Option<&prost_types::Timestamp>, started: Instant) -> ResponseMeta {
    let vehicle_ts_ms = ts.map(|t| t.seconds * 1000 + (t.nanos / 1_000_000) as i64);
    ResponseMeta {
        vehicle_ts_ms,
        request_started_at: Some(started),
    }
}

/// Map car_server's nested ShiftState oneof to our flat enum. Tesla's
/// proto uses a oneof with Void leaves (P, R, N, D) which makes for
/// awkward pattern-matching; collapse to the enum the rest of the
/// crate already uses.
fn map_shift_state(ss: &car_server::ShiftState) -> ShiftState {
    use car_server::shift_state::Type;
    match &ss.r#type {
        Some(Type::P(_)) => ShiftState::Park,
        Some(Type::R(_)) => ShiftState::Reverse,
        Some(Type::N(_)) => ShiftState::Neutral,
        Some(Type::D(_)) => ShiftState::Drive,
        _ => ShiftState::Unknown,
    }
}

/// Map car_server's nested `ChargeState.ChargingState` oneof to our
/// flat `ChargingState`. Same pattern as `map_shift_state`. Returns
/// `Unknown` if the oneof is empty (Tesla didn't include the field
/// at all) — caller turns that into `None` so the phase machine's
/// conservative default kicks in.
fn map_charging_state(cs: &car_server::charge_state::ChargingState) -> ChargingState {
    use car_server::charge_state::charging_state::Type;
    match &cs.r#type {
        Some(Type::Unknown(_)) => ChargingState::Unknown,
        Some(Type::Disconnected(_)) => ChargingState::Disconnected,
        Some(Type::NoPower(_)) => ChargingState::NoPower,
        Some(Type::Starting(_)) => ChargingState::Starting,
        Some(Type::Charging(_)) => ChargingState::Charging,
        Some(Type::Complete(_)) => ChargingState::Complete,
        Some(Type::Stopped(_)) => ChargingState::Stopped,
        Some(Type::Calibrating(_)) => ChargingState::Calibrating,
        None => ChargingState::Unknown,
    }
}

/// Map `ClosuresState.SentryModeState` oneof to our flat `SentryMode`.
/// Returns `None` to the caller when the oneof is empty, so the phase
/// machine's conservative default (treat as "on" → stay Active) wins.
fn map_sentry_mode(sm: &car_server::closures_state::SentryModeState) -> SentryMode {
    use car_server::closures_state::sentry_mode_state::Type;
    match &sm.r#type {
        Some(Type::Off(_)) => SentryMode::Off,
        Some(Type::Idle(_)) => SentryMode::Idle,
        Some(Type::Armed(_)) => SentryMode::Armed,
        Some(Type::Aware(_)) => SentryMode::Aware,
        Some(Type::Panic(_)) => SentryMode::Panic,
        Some(Type::Quiet(_)) => SentryMode::Quiet,
        // Empty oneof — treat as Off; callers also check whether the
        // parent optional sub-message was present.
        None => SentryMode::Off,
    }
}

/// `state drive` over BLE: shift_state, odometer, and location_name.
/// Tesla returns location_name only in drive responses (bundled
/// LocationState), not in standalone `state location`, so this keeps
/// the displayed address fresh.
pub async fn sample_drive_ble(session: &PersistentSession) -> Result<DriveResult> {
    let started = Instant::now();
    let (drive, location) = session.get_drive_with_location().await?;
    let elapsed = started.elapsed().as_millis();

    let shift_state = drive.shift_state.as_ref().map(map_shift_state);
    // Log shift on change so a drive-time bundle shows what the car
    // reports for the gear and how fast it updates after a shift (older
    // Teslas report Invalid/SNA → Unknown when parked). Raw shift + speed
    // only — the full DriveState carries GPS/route (PII).
    {
        use std::sync::atomic::{AtomicI8, Ordering};
        static LAST: AtomicI8 = AtomicI8::new(i8::MIN);
        let code = match shift_state {
            None => -1,
            Some(ShiftState::Unknown) => 0,
            Some(ShiftState::Park) => 1,
            Some(ShiftState::Reverse) => 2,
            Some(ShiftState::Neutral) => 3,
            Some(ShiftState::Drive) => 4,
        };
        if LAST.swap(code, Ordering::Relaxed) != code {
            info!(
                "shift change: shift_state={:?}, speed={:?}",
                drive.shift_state, drive.optional_speed
            );
        }
    }
    let odometer_mi = drive
        .optional_odometer_in_hundredths_of_a_mile
        .as_ref()
        .map(|o| {
            let car_server::drive_state::OptionalOdometerInHundredthsOfAMile::OdometerInHundredthsOfAMile(h) = o;
            (*h as f64) / 100.0
        });
    // location_name + raw GPS from the bundled LocationState; all None
    // when parked-and-unchanged, which is expected. The coords feed the
    // keep-accessory home geofence (see keep_accessory.rs) at zero extra
    // round-trip cost — they ride along in the same `state drive` reply.
    let (location_name, lat, lon) = match location.as_ref() {
        Some(l) => {
            let name = l.optional_location_name.as_ref().map(|v| {
                let car_server::location_state::OptionalLocationName::LocationName(n) = v;
                n.clone()
            });
            let lat = l.optional_latitude.as_ref().map(|v| {
                let car_server::location_state::OptionalLatitude::Latitude(x) = v;
                *x as f64
            });
            let lon = l.optional_longitude.as_ref().map(|v| {
                let car_server::location_state::OptionalLongitude::Longitude(x) = v;
                *x as f64
            });
            (name, lat, lon)
        }
        None => (None, None, None),
    };
    let meta = build_meta(drive.timestamp.as_ref(), started);

    // Log address freshness so a bundle shows whether Tesla is
    // returning it on this poll vs leaving it out.
    match &location_name {
        Some(n) => info!(
            "state-poll: drive=ok({}ms) location=\"{}\" via in-process BLE",
            elapsed, n
        ),
        None => info!(
            "state-poll: drive=ok({}ms) location=<absent> via in-process BLE",
            elapsed
        ),
    }

    use car_server::drive_state as ds;
    let detail = DriveDetail {
        speed_mph: drive.optional_speed_float.as_ref().map(|v| {
            let ds::OptionalSpeedFloat::SpeedFloat(n) = v;
            *n
        }),
        power_kw: drive.optional_power.as_ref().map(|v| {
            let ds::OptionalPower::Power(n) = v;
            *n
        }),
        route_destination: drive.optional_active_route_destination.as_ref().map(|v| {
            let ds::OptionalActiveRouteDestination::ActiveRouteDestination(s) = v;
            s.clone()
        }),
        route_minutes_to_arrival: drive
            .optional_active_route_minutes_to_arrival
            .as_ref()
            .map(|v| {
                let ds::OptionalActiveRouteMinutesToArrival::ActiveRouteMinutesToArrival(n) = v;
                *n
            }),
        route_miles_to_arrival: drive.optional_active_route_miles_to_arrival.as_ref().map(|v| {
            let ds::OptionalActiveRouteMilesToArrival::ActiveRouteMilesToArrival(n) = v;
            *n
        }),
    };

    Ok(DriveResult {
        location_name,
        odometer_mi,
        shift_state,
        lat,
        lon,
        meta,
        detail,
    })
}

/// Emit a one-line summary of live drive + navigation detail. Logged only
/// when the experimental flag is on. The route destination is treated as
/// PII (it is a place name), so the log reports only whether a route is
/// active plus the ETA / distance, never the destination string.
pub fn log_drive_detail(c: &DriveResult) {
    let d = &c.detail;
    info!(
        "drive-detail [experimental]: speed_mph={:?} power_kw={:?} navigating={} \
         eta_min={:?} miles_to_arrival={:?}",
        d.speed_mph,
        d.power_kw,
        d.route_destination.is_some(),
        d.route_minutes_to_arrival,
        d.route_miles_to_arrival,
    );
}

/// `state climate` over BLE. Interior/exterior temps + HVAC on/off.
pub async fn sample_climate_ble(session: &PersistentSession) -> Result<ClimateResult> {
    let started = Instant::now();
    let climate = session.get_climate().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: climate=ok({}ms) via in-process BLE", elapsed);

    let interior_temp_c = climate
        .optional_inside_temp_celsius
        .as_ref()
        .map(|v| {
            let car_server::climate_state::OptionalInsideTempCelsius::InsideTempCelsius(t) = v;
            *t as f64
        });
    let exterior_temp_c = climate
        .optional_outside_temp_celsius
        .as_ref()
        .map(|v| {
            let car_server::climate_state::OptionalOutsideTempCelsius::OutsideTempCelsius(t) = v;
            *t as f64
        });
    let hvac_on = climate
        .optional_is_climate_on
        .as_ref()
        .map(|v| {
            let car_server::climate_state::OptionalIsClimateOn::IsClimateOn(b) = v;
            *b
        });
    let meta = build_meta(climate.timestamp.as_ref(), started);

    use car_server::climate_state as cls;
    let detail = ClimateDetail {
        driver_setpoint_c: climate.optional_driver_temp_setting.as_ref().map(|v| {
            let cls::OptionalDriverTempSetting::DriverTempSetting(n) = v;
            *n
        }),
        passenger_setpoint_c: climate.optional_passenger_temp_setting.as_ref().map(|v| {
            let cls::OptionalPassengerTempSetting::PassengerTempSetting(n) = v;
            *n
        }),
        fan_status: climate.optional_fan_status.as_ref().map(|v| {
            let cls::OptionalFanStatus::FanStatus(n) = v;
            *n
        }),
        front_defroster_on: climate.optional_is_front_defroster_on.as_ref().map(|v| {
            let cls::OptionalIsFrontDefrosterOn::IsFrontDefrosterOn(b) = v;
            *b
        }),
        rear_defroster_on: climate.optional_is_rear_defroster_on.as_ref().map(|v| {
            let cls::OptionalIsRearDefrosterOn::IsRearDefrosterOn(b) = v;
            *b
        }),
        preconditioning: climate.optional_is_preconditioning.as_ref().map(|v| {
            let cls::OptionalIsPreconditioning::IsPreconditioning(b) = v;
            *b
        }),
        seat_heater_left: climate.optional_seat_heater_left.as_ref().map(|v| {
            let cls::OptionalSeatHeaterLeft::SeatHeaterLeft(n) = v;
            *n
        }),
        seat_heater_right: climate.optional_seat_heater_right.as_ref().map(|v| {
            let cls::OptionalSeatHeaterRight::SeatHeaterRight(n) = v;
            *n
        }),
    };

    Ok(ClimateResult {
        interior_temp_c,
        exterior_temp_c,
        hvac_on,
        meta,
        detail,
    })
}

/// Emit a one-line summary of extended climate detail. Logged only when
/// the experimental flag is on; lets a tester confirm the `ClimateState`
/// fields decode correctly off a real car before they reach the UI.
pub fn log_climate_detail(c: &ClimateResult) {
    let d = &c.detail;
    info!(
        "climate-detail [experimental]: setpoint[drv={:?} pass={:?}] fan={:?} \
         defrost[front={:?} rear={:?}] precond={:?} seat[l={:?} r={:?}]",
        d.driver_setpoint_c,
        d.passenger_setpoint_c,
        d.fan_status,
        d.front_defroster_on,
        d.rear_defroster_on,
        d.preconditioning,
        d.seat_heater_left,
        d.seat_heater_right,
    );
}

/// `state charge` over BLE. Battery percent (usable preferred,
/// falling back to display battery_level).
pub async fn sample_charge_ble(session: &PersistentSession) -> Result<ChargeResult> {
    let started = Instant::now();
    let charge = session.get_charge().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: charge=ok({}ms) via in-process BLE", elapsed);

    // Prefer usable_battery_level (matches Tesla app's headline %),
    // fall back to raw battery_level when usable isn't reported.
    let battery_pct = charge
        .optional_usable_battery_level
        .as_ref()
        .map(|v| {
            let car_server::charge_state::OptionalUsableBatteryLevel::UsableBatteryLevel(n) = v;
            *n as f64
        })
        .or_else(|| {
            charge.optional_battery_level.as_ref().map(|v| {
                let car_server::charge_state::OptionalBatteryLevel::BatteryLevel(n) = v;
                *n as f64
            })
        });
    // Hand the phase machine an Option so "populated with Unknown" stays
    // distinct from "never extracted", even though both currently behave
    // the same (stay Active).
    let charging_state = charge.charging_state.as_ref().map(map_charging_state);
    let meta = build_meta(charge.timestamp.as_ref(), started);

    // Expanded charging detail — fields the `ChargeState` message already
    // carries that the sampler didn't previously surface. Decode is cheap
    // and read-only; the caller only logs/consumes it when the
    // experimental flag is on, so this is inert on a normal install.
    let detail = ChargeDetail {
        charger_actual_current_a: charge.optional_charger_actual_current.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargerActualCurrent::ChargerActualCurrent(n) = v;
            *n
        }),
        charger_power_kw: charge.optional_charger_power.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargerPower::ChargerPower(n) = v;
            *n
        }),
        charger_voltage_v: charge.optional_charger_voltage.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargerVoltage::ChargerVoltage(n) = v;
            *n
        }),
        charging_amps_set: charge.optional_charging_amps.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargingAmps::ChargingAmps(n) = v;
            *n
        }),
        charge_current_request_max: charge.optional_charge_current_request_max.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargeCurrentRequestMax::ChargeCurrentRequestMax(n) = v;
            *n
        }),
        charge_rate_mph: charge.optional_charge_rate_mph_float.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargeRateMphFloat::ChargeRateMphFloat(n) = v;
            *n
        }),
        charge_energy_added_kwh: charge.optional_charge_energy_added.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargeEnergyAdded::ChargeEnergyAdded(n) = v;
            *n
        }),
        charge_limit_soc: charge.optional_charge_limit_soc.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargeLimitSoc::ChargeLimitSoc(n) = v;
            *n
        }),
        minutes_to_full_charge: charge.optional_minutes_to_full_charge.as_ref().map(|v| {
            let car_server::charge_state::OptionalMinutesToFullCharge::MinutesToFullCharge(n) = v;
            *n
        }),
        battery_range_mi: charge.optional_battery_range.as_ref().map(|v| {
            let car_server::charge_state::OptionalBatteryRange::BatteryRange(n) = v;
            *n
        }),
        charge_port_door_open: charge.optional_charge_port_door_open.as_ref().map(|v| {
            let car_server::charge_state::OptionalChargePortDoorOpen::ChargePortDoorOpen(n) = v;
            *n
        }),
    };

    Ok(ChargeResult {
        battery_pct,
        charging_state,
        meta,
        detail,
    })
}

/// Emit a one-line summary of the expanded charging detail. Called only
/// when the experimental flag is on, so a normal install stays quiet.
/// Lets a tester confirm the new `ChargeState` fields decode correctly
/// off a real car before we wire them into the DB + API + web UI.
pub fn log_charge_detail(c: &ChargeResult) {
    let d = &c.detail;
    info!(
        "charge-detail [experimental]: amps={:?} power_kw={:?} volts={:?} amps_set={:?} amps_max={:?} \
         rate_mph={:?} added_kwh={:?} limit_soc={:?} mins_to_full={:?} range_mi={:?} \
         port_open={:?}",
        d.charger_actual_current_a,
        d.charger_power_kw,
        d.charger_voltage_v,
        d.charging_amps_set,
        d.charge_current_request_max,
        d.charge_rate_mph,
        d.charge_energy_added_kwh,
        d.charge_limit_soc,
        d.minutes_to_full_charge,
        d.battery_range_mi,
        d.charge_port_door_open,
    );
}

/// `state location` over BLE — raw GPS `(lat, lon)`. Separate from the
/// drive poll because Tesla bundles only the reverse-geocoded
/// `location_name` in `state drive`; the raw coordinates come solely
/// from `state location`. Used by the keep-accessory home geofence,
/// which is why it's polled only when that feature is enabled.
pub async fn sample_location_ble(
    session: &PersistentSession,
) -> Result<(Option<f64>, Option<f64>)> {
    let loc = session.get_location().await?;
    let lat = loc.optional_latitude.as_ref().map(|v| {
        let car_server::location_state::OptionalLatitude::Latitude(x) = v;
        *x as f64
    });
    let lon = loc.optional_longitude.as_ref().map(|v| {
        let car_server::location_state::OptionalLongitude::Longitude(x) = v;
        *x as f64
    });
    Ok((lat, lon))
}

/// `state closures` over BLE. Used only for `sentry_mode_state` (the
/// quiet-mode gate); door/window/port state is in the same response if
/// the UI ever needs it.
pub async fn sample_closures_ble(session: &PersistentSession) -> Result<ClosuresResult> {
    let started = Instant::now();
    let closures = session.get_closures().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: closures=ok({}ms) via in-process BLE", elapsed);

    // Absent on cars that don't support sentry mode; None (not Off) lets
    // the conservative default handle "unsupported" and "no poll yet"
    // alike.
    let sentry_mode = closures.sentry_mode_state.as_ref().map(map_sentry_mode);
    let meta = build_meta(closures.timestamp.as_ref(), started);

    use car_server::closures_state as cs;
    let detail = ClosuresDetail {
        locked: closures.optional_locked.as_ref().map(|v| {
            let cs::OptionalLocked::Locked(b) = v;
            *b
        }),
        door_driver_front_open: closures.optional_door_open_driver_front.as_ref().map(|v| {
            let cs::OptionalDoorOpenDriverFront::DoorOpenDriverFront(b) = v;
            *b
        }),
        door_driver_rear_open: closures.optional_door_open_driver_rear.as_ref().map(|v| {
            let cs::OptionalDoorOpenDriverRear::DoorOpenDriverRear(b) = v;
            *b
        }),
        door_passenger_front_open: closures.optional_door_open_passenger_front.as_ref().map(|v| {
            let cs::OptionalDoorOpenPassengerFront::DoorOpenPassengerFront(b) = v;
            *b
        }),
        door_passenger_rear_open: closures.optional_door_open_passenger_rear.as_ref().map(|v| {
            let cs::OptionalDoorOpenPassengerRear::DoorOpenPassengerRear(b) = v;
            *b
        }),
        frunk_open: closures.optional_door_open_trunk_front.as_ref().map(|v| {
            let cs::OptionalDoorOpenTrunkFront::DoorOpenTrunkFront(b) = v;
            *b
        }),
        trunk_open: closures.optional_door_open_trunk_rear.as_ref().map(|v| {
            let cs::OptionalDoorOpenTrunkRear::DoorOpenTrunkRear(b) = v;
            *b
        }),
        window_driver_front_open: closures.optional_window_open_driver_front.as_ref().map(|v| {
            let cs::OptionalWindowOpenDriverFront::WindowOpenDriverFront(b) = v;
            *b
        }),
        window_passenger_front_open: closures
            .optional_window_open_passenger_front
            .as_ref()
            .map(|v| {
                let cs::OptionalWindowOpenPassengerFront::WindowOpenPassengerFront(b) = v;
                *b
            }),
        window_driver_rear_open: closures.optional_window_open_driver_rear.as_ref().map(|v| {
            let cs::OptionalWindowOpenDriverRear::WindowOpenDriverRear(b) = v;
            *b
        }),
        window_passenger_rear_open: closures
            .optional_window_open_passenger_rear
            .as_ref()
            .map(|v| {
                let cs::OptionalWindowOpenPassengerRear::WindowOpenPassengerRear(b) = v;
                *b
            }),
        sunroof_percent_open: closures.optional_sun_roof_percent_open.as_ref().map(|v| {
            let cs::OptionalSunRoofPercentOpen::SunRoofPercentOpen(n) = v;
            *n
        }),
    };

    Ok(ClosuresResult {
        sentry_mode,
        meta,
        detail,
    })
}

/// Emit a one-line summary of door / window / lock state. Logged only
/// when the experimental flag is on so a normal install stays quiet;
/// lets a tester confirm the `ClosuresState` fields decode correctly off
/// a real car before they are surfaced in the API and web UI.
pub fn log_closures_detail(c: &ClosuresResult) {
    let d = &c.detail;
    info!(
        "closures-detail [experimental]: locked={:?} doors[df={:?} dr={:?} pf={:?} pr={:?}] \
         frunk={:?} trunk={:?} windows[df={:?} pf={:?} dr={:?} pr={:?}] sunroof%={:?}",
        d.locked,
        d.door_driver_front_open,
        d.door_driver_rear_open,
        d.door_passenger_front_open,
        d.door_passenger_rear_open,
        d.frunk_open,
        d.trunk_open,
        d.window_driver_front_open,
        d.window_passenger_front_open,
        d.window_driver_rear_open,
        d.window_passenger_rear_open,
        d.sunroof_percent_open,
    );
}

/// `state tire-pressure` over BLE. Converts Tesla's native bar →
/// PSI to match what's displayed in the UI (US convention).
pub async fn sample_tires_ble(session: &PersistentSession) -> Result<TiresResult> {
    let started = Instant::now();
    let tires = session.get_tire_pressure().await?;
    let elapsed = started.elapsed().as_millis();
    info!("state-poll: tires=ok({}ms) via in-process BLE", elapsed);

    let fl = tires.optional_tpms_pressure_fl.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureFl::TpmsPressureFl(b) = v;
        bar_to_psi(*b as f64)
    });
    let fr = tires.optional_tpms_pressure_fr.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureFr::TpmsPressureFr(b) = v;
        bar_to_psi(*b as f64)
    });
    let rl = tires.optional_tpms_pressure_rl.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureRl::TpmsPressureRl(b) = v;
        bar_to_psi(*b as f64)
    });
    let rr = tires.optional_tpms_pressure_rr.as_ref().map(|v| {
        let car_server::tire_pressure_state::OptionalTpmsPressureRr::TpmsPressureRr(b) = v;
        bar_to_psi(*b as f64)
    });
    let meta = build_meta(tires.timestamp.as_ref(), started);

    Ok(TiresResult {
        tire_fl_psi: fl,
        tire_fr_psi: fr,
        tire_rl_psi: rl,
        tire_rr_psi: rr,
        meta,
    })
}

/// `body-controller-state` over BLE. Unauthenticated — works against a
/// sleeping car without waking it. Routed through the PersistentSession's
/// held connection so it doesn't fight for bluez (which caused
/// framing-desync errors and multi-second outliers).
pub async fn sample_body_controller_ble(
    session: &PersistentSession,
) -> Result<BodyControllerSample> {
    let start = Instant::now();
    let result = session.body_controller_state().await;
    let elapsed_ms = start.elapsed().as_millis() as u64;
    match &result {
        Ok(_) => info!("body-controller poll: ok({}ms) via in-process BLE", elapsed_ms),
        Err(e) => warn!("body-controller poll: err({}ms): {:#}", elapsed_ms, e),
    }
    let resp = result?;

    // UserPresence_E (vcsec.proto): 0=UNKNOWN, 1=NOT_PRESENT, 2=PRESENT.
    // Treat unknown as None so the phase machine doesn't take action
    // on a non-signal; only collapse the present/not-present cases to
    // a bool.
    let user_presence = match resp.user_presence {
        2 => Some(true),
        1 => Some(false),
        _ => None,
    };

    // VehicleSleepStatus_E (vcsec.proto): 0=UNKNOWN, 1=AWAKE, 2=ASLEEP.
    // Same unknown-is-None treatment as user_presence above.
    let awake = match resp.vehicle_sleep_status {
        1 => Some(true),
        2 => Some(false),
        _ => None,
    };

    Ok(BodyControllerSample {
        sample: Sample {
            ts: now_secs(),
            source: "body_controller".into(),
            ..Sample::default()
        },
        user_presence,
        awake,
    })
}

/// Bundles a `PersistentSession` with the VIN + adapter it was
/// opened for, so the sampler can detect a config change between
/// ticks and recreate the session cleanly. Stored as
/// `Option<SessionHandle>` in main.
pub struct SessionHandle {
    pub session: PersistentSession,
    pub vin: String,
    pub adapter: Option<String>,
}

/// Ensure `handle` is a `PersistentSession` for the given VIN +
/// adapter. Lazily spawns the session on first call, recreates it
/// if EITHER the VIN or the configured adapter changed. The
/// keypair is loaded from the standard /root/.ble path each time
/// the session is created.
pub fn ensure_session_for(
    handle: &mut Option<SessionHandle>,
    vin: &str,
    adapter: Option<&str>,
) -> Result<()> {
    let want_adapter = adapter
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(existing) = handle {
        if existing.vin == vin && existing.adapter == want_adapter {
            return Ok(());
        }
        info!(
            "PersistentSession config changed (vin {}->{}, adapter {:?}->{:?}); recreating",
            short_vin(&existing.vin),
            short_vin(vin),
            existing.adapter,
            want_adapter
        );
        // Dropping the existing handle closes the mpsc Sender; the
        // background task notices and shuts down naturally.
    }
    let keypair = KeyPair::load(std::path::Path::new("/root/.ble/key_private.pem"))?;
    *handle = Some(SessionHandle {
        session: PersistentSession::start(keypair, vin.to_string(), want_adapter.clone()),
        vin: vin.to_string(),
        adapter: want_adapter,
    });
    Ok(())
}

fn short_vin(vin: &str) -> String {
    if vin.len() >= 7 {
        format!("{}...{}", &vin[..3], &vin[vin.len() - 4..])
    } else {
        vin.to_string()
    }
}
