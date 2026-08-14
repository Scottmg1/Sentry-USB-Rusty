//! Inner payload bytes for keep-awake actions, used by `awake_start`'s
//! nudge loop during archive cycles. All are signed messages (same
//! AES-GCM pipeline as state queries):
//!
//! * `wake_vehicle`          → VEHICLE_SECURITY, RKE_ACTION_WAKE_VEHICLE (30).
//!                             Works even when the main computer is asleep.
//! * `set_sentry_mode`       → INFOTAINMENT. Used by SENTRY_CASE=1
//!                             (auto-arm away) and =3 (periodic nudge).
//! * `charge_port_open/close` → INFOTAINMENT, ChargePortDoor{Open,Close}.

use anyhow::{Result, bail};
use prost::Message;

use crate::proto::car_server::{
    Action, ChargePortDoorClose, ChargePortDoorOpen, ChargingSetLimitAction,
    ChargingStartStopAction, SetChargingAmpsAction, SetKeepAccessoryPowerModeAction,
    VehicleAction, VehicleControlSetSentryModeAction, Void, action, charging_start_stop_action,
    vehicle_action,
};
use crate::proto::vcsec::{UnsignedMessage, unsigned_message};
use crate::proto::universal_message::Domain;

/// One keep-awake command with the domain it ships to and the
/// pre-encoded inner payload bytes.
pub struct ActionPayload {
    pub domain: Domain,
    pub inner: Vec<u8>,
}

/// Wake the car. Routes through VEHICLE_SECURITY domain because the
/// VCSEC controller is awake even when the main computer is asleep —
/// this is the same path `tesla-control wake` uses.
pub fn wake_vehicle() -> ActionPayload {
    const RKE_ACTION_WAKE_VEHICLE: i32 = 30;
    let inner = UnsignedMessage {
        sub_message: Some(unsigned_message::SubMessage::RkeAction(
            RKE_ACTION_WAKE_VEHICLE,
        )),
    }
    .encode_to_vec();
    ActionPayload {
        domain: Domain::VehicleSecurity,
        inner,
    }
}

/// Turn Sentry Mode on or off via INFOTAINMENT. Lives under the
/// `vehicleControlSetSentryModeAction` oneof variant in
/// `car_server.VehicleAction`.
pub fn set_sentry_mode(on: bool) -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::VehicleControlSetSentryModeAction(
                    VehicleControlSetSentryModeAction { on },
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Open the charge port. INFOTAINMENT domain.
pub fn charge_port_open() -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::ChargePortDoorOpen(
                    ChargePortDoorOpen {},
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Close the charge port. INFOTAINMENT domain.
pub fn charge_port_close() -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::ChargePortDoorClose(
                    ChargePortDoorClose {},
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Turn "Keep Accessory Power" on or off via INFOTAINMENT — the same toggle the
/// Tesla app exposes on the Charging screen. Mirrors [`set_sentry_mode`]; lives
/// under the `setKeepAccessoryPowerModeAction` oneof variant in
/// `car_server.VehicleAction` (field 138).
pub fn set_keep_accessory_power(on: bool) -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::SetKeepAccessoryPowerModeAction(
                    SetKeepAccessoryPowerModeAction {
                        keep_accessory_power_mode: on,
                    },
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::car_server::{charging_start_stop_action, vehicle_action};

    fn decode_vehicle_action(payload: ActionPayload) -> vehicle_action::VehicleActionMsg {
        assert_eq!(payload.domain, Domain::Infotainment);
        let action = Action::decode(payload.inner.as_slice()).unwrap();
        let action::ActionMsg::VehicleAction(vehicle) = action.action_msg.unwrap();
        vehicle.vehicle_action_msg.unwrap()
    }

    #[test]
    fn wake_payload_is_vcsec_rke_30() {
        let p = wake_vehicle();
        assert_eq!(p.domain, Domain::VehicleSecurity);
        // VCSEC UnsignedMessage { RKEAction = 30 } encodes as:
        //   tag for field 2 varint (0x10), value 30 (0x1e) = "10 1e"
        assert_eq!(hex::encode(&p.inner), "101e");
    }

    #[test]
    fn sentry_mode_on_routes_to_infotainment() {
        let p = set_sentry_mode(true);
        assert_eq!(p.domain, Domain::Infotainment);
        // Action { VehicleAction { vehicleControlSetSentryModeAction { on=true } } }
        //   Action.vehicleAction (field 2, length-delimited)
        //     VehicleAction.vehicleControlSetSentryModeAction (field 30)
        //       SentryModeAction.on (field 1, varint, true)
        // We don't pin exact bytes (proto encoding can vary by lib
        // version on field-tag widths); just sanity-check non-empty.
        assert!(!p.inner.is_empty());
    }

    #[test]
    fn sentry_mode_off_differs_from_on() {
        // The two states must produce distinct payloads or the car
        // would arm/disarm the wrong way.
        assert_ne!(set_sentry_mode(true).inner, set_sentry_mode(false).inner);
    }

    #[test]
    fn charge_port_open_close_are_distinct() {
        assert_ne!(charge_port_open().inner, charge_port_close().inner);
    }

    #[test]
    fn charging_start_and_stop_encode_exact_proto_variants() {
        let vehicle_action::VehicleActionMsg::ChargingStartStopAction(start) =
            decode_vehicle_action(start_charging())
        else {
            panic!("expected ChargingStartStopAction");
        };
        assert!(matches!(
            start.charging_action,
            Some(charging_start_stop_action::ChargingAction::Start(_))
        ));

        let vehicle_action::VehicleActionMsg::ChargingStartStopAction(stop) =
            decode_vehicle_action(stop_charging())
        else {
            panic!("expected ChargingStartStopAction");
        };
        assert!(matches!(
            stop.charging_action,
            Some(charging_start_stop_action::ChargingAction::Stop(_))
        ));
    }

    #[test]
    fn charging_settings_encode_requested_values() {
        let vehicle_action::VehicleActionMsg::SetChargingAmpsAction(amps) =
            decode_vehicle_action(set_charging_amps(32))
        else {
            panic!("expected SetChargingAmpsAction");
        };
        assert_eq!(amps.charging_amps, 32);

        let vehicle_action::VehicleActionMsg::ChargingSetLimitAction(limit) =
            decode_vehicle_action(set_charge_limit(80))
        else {
            panic!("expected ChargingSetLimitAction");
        };
        assert_eq!(limit.percent, 80);
    }

    #[test]
    fn parameterized_action_verbs_validate_bounds() {
        assert!(parse_verb("charge-start").is_ok());
        assert!(parse_verb("charge-stop").is_ok());
        assert!(parse_verb("set-charging-amps:32").is_ok());
        assert!(parse_verb("set-charge-limit:80").is_ok());
        assert!(parse_verb("set-charging-amps:0").is_err());
        assert!(parse_verb("set-charging-amps:81").is_err());
        assert!(parse_verb("set-charge-limit:49").is_err());
        assert!(parse_verb("set-charge-limit:101").is_err());
        assert!(parse_verb("set-charging-amps:not-a-number").is_err());
    }
}

fn charging_start_stop(
    charging_action: charging_start_stop_action::ChargingAction,
) -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(
                vehicle_action::VehicleActionMsg::ChargingStartStopAction(
                    ChargingStartStopAction {
                        charging_action: Some(charging_action),
                    },
                ),
            ),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Start charging through the infotainment controller.
pub fn start_charging() -> ActionPayload {
    charging_start_stop(charging_start_stop_action::ChargingAction::Start(Void {}))
}

/// Stop charging through the infotainment controller.
pub fn stop_charging() -> ActionPayload {
    charging_start_stop(charging_start_stop_action::ChargingAction::Stop(Void {}))
}

/// Set the requested charging current in amps.
pub fn set_charging_amps(charging_amps: i32) -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(vehicle_action::VehicleActionMsg::SetChargingAmpsAction(
                SetChargingAmpsAction { charging_amps },
            )),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Set the target battery charge limit in percent.
pub fn set_charge_limit(percent: i32) -> ActionPayload {
    let action = Action {
        action_msg: Some(action::ActionMsg::VehicleAction(VehicleAction {
            vehicle_action_msg: Some(vehicle_action::VehicleActionMsg::ChargingSetLimitAction(
                ChargingSetLimitAction { percent },
            )),
        })),
    };
    ActionPayload {
        domain: Domain::Infotainment,
        inner: action.encode_to_vec(),
    }
}

/// Translate the CLI/IPC wire verb into a typed action. Parameterized
/// values use a single colon-delimited argument so the daemon's one-line
/// socket protocol remains unchanged.
pub fn parse_verb(verb: &str) -> Result<ActionPayload> {
    match verb {
        "wake" => Ok(wake_vehicle()),
        "sentry-on" => Ok(set_sentry_mode(true)),
        "sentry-off" => Ok(set_sentry_mode(false)),
        "charge-port-open" => Ok(charge_port_open()),
        "charge-port-close" => Ok(charge_port_close()),
        "keep-accessory-on" => Ok(set_keep_accessory_power(true)),
        "keep-accessory-off" => Ok(set_keep_accessory_power(false)),
        "charge-start" => Ok(start_charging()),
        "charge-stop" => Ok(stop_charging()),
        other => {
            if let Some(raw) = other.strip_prefix("set-charging-amps:") {
                let amps: i32 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("charging amps must be a whole number"))?;
                if !(1..=80).contains(&amps) {
                    bail!("charging amps must be between 1 and 80");
                }
                return Ok(set_charging_amps(amps));
            }
            if let Some(raw) = other.strip_prefix("set-charge-limit:") {
                let percent: i32 = raw
                    .parse()
                    .map_err(|_| anyhow::anyhow!("charge limit must be a whole number"))?;
                if !(50..=100).contains(&percent) {
                    bail!("charge limit must be between 50 and 100");
                }
                return Ok(set_charge_limit(percent));
            }
            bail!("unknown BLE action verb '{other}'")
        }
    }
}
