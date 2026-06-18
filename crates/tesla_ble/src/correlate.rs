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
//! `is_response_to` supplies the missing correlation, using two signals
//! the car already provides: it echoes our request `uuid` (field 51)
//! into the response `request_uuid` (field 50), and a response carries
//! `from_destination` = the domain that produced it.

use prost::Message;

use crate::proto::universal_message::{Domain, RoutableMessage, destination};

/// Returns `true` if `frame` is plausibly the response to a request we
/// sent to `target_domain` carrying request id `our_uuid`.
///
/// Correlation is deliberately *conservative*: it rejects a frame only
/// when the frame carries positive evidence of belonging to a different
/// request —
///   * a `from_destination` domain that differs from the one we
///     addressed (kills cross-domain VCSEC <-> Infotainment straggler
///     pickup — the "no sub_sig_data" failure), or
///   * a non-empty `request_uuid` that differs from ours (kills
///     same-domain stale-response pickup — the `aead::Error` failure).
///
/// Frames that omit both signals (some SessionInfo refreshes, unsigned
/// replies that don't echo a uuid) are *not* rejected here; they fall
/// through to the caller's own shape check, preserving the existing
/// refresh/recovery paths. This guarantees the change can only *remove*
/// false acceptances — it never rejects a reply the old validator would
/// have accepted on a quiet link.
pub fn is_response_to(frame: &[u8], our_uuid: &[u8], target_domain: Domain) -> bool {
    let Ok(rm) = RoutableMessage::decode(frame) else {
        return false;
    };

    // Cross-domain guard. A *response* stamps `from_destination` with
    // the domain that produced it (a *request* stamps it with the
    // sender's routing address). So whenever `from_destination` names a
    // domain, it must be the domain we queried.
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

    // request_uuid correlation. The car echoes our request `uuid` here.
    // Reject only on a *present* mismatch — an absent request_uuid
    // cannot be correlated and is left to the caller's shape check.
    if !rm.request_uuid.is_empty() && rm.request_uuid.as_slice() != our_uuid {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::universal_message::Destination;

    fn response(from_domain: Domain, request_uuid: &[u8]) -> Vec<u8> {
        RoutableMessage {
            from_destination: Some(Destination {
                sub_destination: Some(destination::SubDestination::Domain(
                    from_domain as i32,
                )),
            }),
            request_uuid: request_uuid.to_vec(),
            ..Default::default()
        }
        .encode_to_vec()
    }

    /// A reply from the queried domain echoing our uuid is accepted.
    #[test]
    fn accepts_matching_domain_and_uuid() {
        let uuid = [0xaa; 16];
        let frame = response(Domain::Infotainment, &uuid);
        assert!(is_response_to(&frame, &uuid, Domain::Infotainment));
    }

    /// A reply whose request_uuid is a *different* request's id (a stale
    /// same-domain straggler) is rejected — this is the `aead::Error`
    /// case.
    #[test]
    fn rejects_mismatched_request_uuid() {
        let frame = response(Domain::Infotainment, &[0x11; 16]);
        assert!(!is_response_to(&frame, &[0x22; 16], Domain::Infotainment));
    }

    /// A reply from a *different domain* is rejected even with no uuid —
    /// this is the cross-talk that produced "no sub_sig_data".
    #[test]
    fn rejects_cross_domain_reply() {
        // A VCSEC reply must not satisfy an Infotainment query.
        let frame = response(Domain::VehicleSecurity, &[]);
        assert!(!is_response_to(&frame, &[0x22; 16], Domain::Infotainment));
    }

    /// The exact bytes captured on the wire when the bug fired: a real
    /// body-controller `VehicleStatus` reply (from VEHICLE_SECURITY, no
    /// request_uuid). It must be REJECTED for an Infotainment query (the
    /// source of the "no sub_sig_data" log line) yet ACCEPTED for the
    /// VEHICLE_SECURITY query it actually answers.
    #[test]
    fn captured_vcsec_reply_is_domain_separated() {
        let frame = hex::decode(
            "32121210696efe0b9d93a8284f908951281d56413a020802520c0a0a10011801200142020802",
        )
        .unwrap();
        // Wrong path (signed Infotainment query) -> reject.
        assert!(!is_response_to(&frame, &[0x22; 16], Domain::Infotainment));
        // Right path (body-controller VCSEC query) -> accept.
        assert!(is_response_to(&frame, &[0x22; 16], Domain::VehicleSecurity));
    }

    /// A reply that omits both signals still passes (left to the caller's
    /// shape validator) — guarantees no regression on the refresh path.
    #[test]
    fn passes_reply_with_no_correlation_fields() {
        let frame = RoutableMessage::default().encode_to_vec();
        assert!(is_response_to(&frame, &[0x22; 16], Domain::Infotainment));
    }
}
