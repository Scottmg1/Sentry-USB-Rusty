//! Unauthenticated body-controller-state query (door/lock/presence/sleep).
//!
//! VCSEC accepts `GET_STATUS` without authentication, so this is the
//! perfect first end-to-end test of the bluez-based BLE port — proves
//! transport + framing + protobuf without needing any crypto.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use prost::Message;
use rand::Rng;
use tracing::{debug, info};

use crate::gatt::Connection;
use crate::proto::universal_message::{
    Destination, Domain, RoutableMessage, destination, routable_message,
};
use crate::proto::vcsec::{
    FromVcsecMessage, InformationRequest, InformationRequestType, UnsignedMessage,
    unsigned_message, VehicleStatus,
};

/// Run a `body-controller-state` query and return the parsed
/// VehicleStatus (closures, lock state, sleep status, user presence).
pub async fn query(conn: &mut Connection) -> Result<VehicleStatus> {
    let payload = build_request();
    info!("body-controller-state: TX {} bytes", payload.len());
    // Correlate the response to our request: it must originate from the
    // VEHICLE_SECURITY domain we addressed and echo our per-request
    // routing address. Without this, a late Infotainment straggler on the
    // shared session could be parsed as a VCSEC reply. See
    // `crate::correlate`.
    let (our_uuid, our_addr) = crate::correlate::our_correlators(&payload);
    let response_bytes = conn
        .round_trip(&payload, Duration::from_secs(10), |b| {
            crate::correlate::is_response_to(b, &our_uuid, &our_addr, Domain::VehicleSecurity)
        })
        .await
        .context("BLE round-trip")?;
    info!("body-controller-state: RX {} bytes", response_bytes.len());
    parse_response(&response_bytes)
}

/// Build the request byte payload (Routable wrapping an UnsignedMessage
/// wrapping an InformationRequest with type GET_STATUS).
fn build_request() -> Vec<u8> {
    // Inner: empty InformationRequest (default request type 0 = GET_STATUS).
    let unsigned = UnsignedMessage {
        sub_message: Some(unsigned_message::SubMessage::InformationRequest(
            InformationRequest {
                information_request_type:
                    InformationRequestType::GetStatus as i32,
                key: None,
            },
        )),
    };
    let inner_bytes = unsigned.encode_to_vec();

    // Outer Routable. We're talking to VEHICLE_SECURITY domain; our
    // "from_destination" is a random per-process routing UUID; per-
    // request UUID is also random.
    let from_uuid = random_uuid_bytes();
    let req_uuid = random_uuid_bytes();
    let msg = RoutableMessage {
        to_destination: Some(Destination {
            sub_destination: Some(destination::SubDestination::Domain(
                crate::proto::universal_message::Domain::VehicleSecurity as i32,
            )),
        }),
        from_destination: Some(Destination {
            sub_destination: Some(destination::SubDestination::RoutingAddress(
                from_uuid.to_vec(),
            )),
        }),
        payload: Some(routable_message::Payload::ProtobufMessageAsBytes(inner_bytes)),
        // Match the flag value tesla-control emits for this query.
        flags: 2,
        uuid: req_uuid.to_vec(),
        ..Default::default()
    };
    msg.encode_to_vec()
}

/// Decode the Routable wrapper, then the VCSEC FromVCSECMessage,
/// then extract the VehicleStatus.
fn parse_response(bytes: &[u8]) -> Result<VehicleStatus> {
    debug!("RX hex: {}", hex::encode(bytes));
    let routable =
        RoutableMessage::decode(bytes).context("decode outer Routable")?;
    debug!("RX decoded: {:#?}", routable);
    let inner = match routable.payload {
        Some(routable_message::Payload::ProtobufMessageAsBytes(b)) => b,
        Some(other) => bail!("unexpected payload variant: {:?}", other),
        None => bail!(
            "response has no payload (signedMessageStatus={:?})",
            routable.signed_message_status
        ),
    };
    let vcsec_msg =
        FromVcsecMessage::decode(inner.as_slice()).context("decode FromVCSECMessage")?;
    let Some(crate::proto::vcsec::from_vcsec_message::SubMessage::VehicleStatus(status)) =
        vcsec_msg.sub_message
    else {
        bail!("FromVCSECMessage did not contain VehicleStatus");
    };
    debug!("parsed VehicleStatus: {:?}", status);
    Ok(status)
}

fn random_uuid_bytes() -> [u8; 16] {
    let mut out = [0u8; 16];
    rand::rng().fill_bytes(&mut out);
    out
}
