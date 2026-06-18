//! Response/request correlation for the shared persistent BLE session.
//!
//! All command, query, and keep-awake traffic to a parked car rides ONE
//! long-lived GATT connection, and a dozing car answers slowly. A query
//! that times out can have its real response arrive late and land in the
//! RX path of the *next* operation. `gatt::Connection::round_trip`'s
//! frame validator historically only checked that bytes decode as a
//! `RoutableMessage`, so such a straggler was accepted as "the
//! response" — producing a misleading `aead::Error` (a stale *signed*
//! reply decrypted with the new request's REQUEST_HASH) or a
//! "no sub_sig_data" error (an unsigned VCSEC reply — e.g. a
//! body-controller `VehicleStatus` — consumed by the signed path).
//!
//! `is_response_to` supplies the missing correlation. The strongest
//! signal — verified on the wire — is the **routing address**: the car
//! echoes our per-request `from_destination` routing address (field 7)
//! into the response's `to_destination` (field 6) on EVERY reply
//! addressed to us. It does NOT reliably echo `request_uuid` (field 50)
//! — so the routing address, not the uuid, is what catches a same-domain
//! straggler. We also use the response's `from_destination` domain and,
//! when present, `request_uuid`.

use prost::Message;

use crate::proto::universal_message::{Domain, RoutableMessage, destination};

/// Recover the per-request correlators from our own outgoing `envelope`:
/// the `from_destination` routing address (which the car echoes into the
/// response's `to_destination`) and the `uuid` (echoed into
/// `request_uuid`, when the car echoes it at all). Returns
/// `(request_uuid, routing_addr)`; either may be empty if absent.
pub fn our_correlators(envelope: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let Ok(rm) = RoutableMessage::decode(envelope) else {
        return (Vec::new(), Vec::new());
    };
    let addr = rm
        .from_destination
        .as_ref()
        .and_then(|d| d.sub_destination.as_ref())
        .and_then(|sd| match sd {
            destination::SubDestination::RoutingAddress(a) => Some(a.clone()),
            _ => None,
        })
        .unwrap_or_default();
    (rm.uuid, addr)
}

/// Returns `true` if `frame` is plausibly the response to a request we
/// sent to `target_domain` with request id `our_request_uuid` and
/// `from_destination` routing address `our_routing_addr`.
///
/// Correlation is deliberately *conservative*: it rejects a frame only
/// when the frame carries positive evidence of belonging to a different
/// request —
///   * a `from_destination` domain that differs from the one we
///     addressed (cross-domain VCSEC <-> Infotainment straggler — the
///     "no sub_sig_data" case), or
///   * a `to_destination` routing address that differs from ours — the
///     PRIMARY check: the car echoes our per-request routing address here
///     on every reply, so a mismatch is a straggler for a different
///     request. This is what catches the same-domain `aead` case that
///     `request_uuid` cannot, because the car doesn't echo `request_uuid`, or
///   * a non-empty `request_uuid` that differs from ours.
///
/// Frames that omit all three signals fall through to the caller's own
/// shape check, preserving the existing refresh/recovery paths. So the
/// change can only *remove* false acceptances — it never rejects a reply
/// the old validator would have accepted on a quiet link.
pub fn is_response_to(
    frame: &[u8],
    our_request_uuid: &[u8],
    our_routing_addr: &[u8],
    target_domain: Domain,
) -> bool {
    let Ok(rm) = RoutableMessage::decode(frame) else {
        return false;
    };

    // Cross-domain guard. A *response* stamps `from_destination` with the
    // domain that produced it; whenever it names a domain, it must be the
    // one we queried.
    if let Some(from_domain) = rm
        .from_destination
        .as_ref()
        .and_then(|d| d.sub_destination.as_ref())
        .and_then(|sd| match sd {
            destination::SubDestination::Domain(d) => Some(*d),
            _ => None,
        })
    {
        if from_domain != target_domain as i32 {
            return false;
        }
    }

    // Routing-address correlation (PRIMARY). The car echoes our
    // per-request `from_destination` routing address into the response's
    // `to_destination`. Reject a present routing address that isn't ours.
    if let Some(to_addr) = rm
        .to_destination
        .as_ref()
        .and_then(|d| d.sub_destination.as_ref())
        .and_then(|sd| match sd {
            destination::SubDestination::RoutingAddress(a) => Some(a.as_slice()),
            _ => None,
        })
    {
        if !our_routing_addr.is_empty() && to_addr != our_routing_addr {
            return false;
        }
    }

    // request_uuid correlation (when the car echoes it — currently it
    // doesn't, but reject a present mismatch in case that changes).
    if !rm.request_uuid.is_empty() && rm.request_uuid.as_slice() != our_request_uuid {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::universal_message::Destination;

    fn dest_domain(d: Domain) -> Destination {
        Destination {
            sub_destination: Some(destination::SubDestination::Domain(d as i32)),
        }
    }
    fn dest_addr(a: &[u8]) -> Destination {
        Destination {
            sub_destination: Some(destination::SubDestination::RoutingAddress(a.to_vec())),
        }
    }

    /// Build a response frame: `from` = a domain, `to` = a routing addr
    /// (omitted if empty), optional echoed `request_uuid`.
    fn response(from: Domain, to_addr: &[u8], request_uuid: &[u8]) -> Vec<u8> {
        RoutableMessage {
            to_destination: (!to_addr.is_empty()).then(|| dest_addr(to_addr)),
            from_destination: Some(dest_domain(from)),
            request_uuid: request_uuid.to_vec(),
            ..Default::default()
        }
        .encode_to_vec()
    }

    /// A reply from the queried domain echoing our routing address is accepted.
    #[test]
    fn accepts_matching_domain_and_addr() {
        let addr = [0xaa; 16];
        let frame = response(Domain::Infotainment, &addr, &[]);
        assert!(is_response_to(&frame, &[0x01; 16], &addr, Domain::Infotainment));
    }

    /// PRIMARY new capability: a same-domain straggler (Infotainment reply
    /// addressed to a DIFFERENT request's routing addr, no request_uuid —
    /// the real-world `aead` case) is rejected.
    #[test]
    fn rejects_same_domain_straggler_by_routing_addr() {
        let frame = response(Domain::Infotainment, &[0x99; 16], &[]);
        assert!(!is_response_to(&frame, &[0x01; 16], &[0x11; 16], Domain::Infotainment));
    }

    /// A reply from a different domain is rejected — the "no sub_sig_data"
    /// cross-talk.
    #[test]
    fn rejects_cross_domain_reply() {
        let frame = response(Domain::VehicleSecurity, &[0x11; 16], &[]);
        assert!(!is_response_to(&frame, &[0x01; 16], &[0x11; 16], Domain::Infotainment));
    }

    /// The exact bytes captured when the bug fired: a real body-controller
    /// `VehicleStatus` reply — from VEHICLE_SECURITY, to routing addr
    /// `696efe0b…`, no request_uuid. Rejected for the Infotainment path
    /// (wrong domain), accepted for the VCSEC path with the matching addr,
    /// and rejected for VCSEC with a wrong addr (straggler).
    #[test]
    fn captured_vcsec_reply_correlates() {
        let frame = hex::decode(
            "32121210696efe0b9d93a8284f908951281d56413a020802520c0a0a10011801200142020802",
        )
        .unwrap();
        let addr = hex::decode("696efe0b9d93a8284f908951281d5641").unwrap();
        assert!(!is_response_to(&frame, &[0x22; 16], &addr, Domain::Infotainment));
        assert!(is_response_to(&frame, &[0x22; 16], &addr, Domain::VehicleSecurity));
        assert!(!is_response_to(&frame, &[0x22; 16], &[0x00; 16], Domain::VehicleSecurity));
    }

    /// If the car ever DOES echo request_uuid, a mismatch is rejected.
    #[test]
    fn rejects_mismatched_request_uuid_when_present() {
        let frame = response(Domain::Infotainment, &[], &[0x11; 16]);
        assert!(!is_response_to(&frame, &[0x22; 16], &[], Domain::Infotainment));
    }

    /// A reply that omits all three signals still passes (left to the
    /// caller's shape check) — guarantees no regression on the refresh path.
    #[test]
    fn passes_reply_with_no_correlation_fields() {
        let frame = RoutableMessage::default().encode_to_vec();
        assert!(is_response_to(&frame, &[0x22; 16], &[0x11; 16], Domain::Infotainment));
    }

    /// `our_correlators` pulls our routing addr + uuid back out of an
    /// outgoing envelope.
    #[test]
    fn our_correlators_round_trips() {
        let env = RoutableMessage {
            from_destination: Some(dest_addr(&[0xab; 16])),
            uuid: vec![0xcd; 16],
            ..Default::default()
        }
        .encode_to_vec();
        let (uuid, addr) = our_correlators(&env);
        assert_eq!(uuid, vec![0xcd; 16]);
        assert_eq!(addr, vec![0xab; 16]);
    }
}
