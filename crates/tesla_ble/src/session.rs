//! Handshake: SessionInfoRequest → SessionInfo, per Tesla domain.

use std::time::Duration;

use anyhow::Context;
use prost::Message;
use rand::Rng;
use thiserror::Error;
use tracing::{debug, info, warn};

use crate::gatt::Connection;
use crate::keys::KeyPair;
use crate::proto::signatures::{SessionInfo, SessionInfoStatus};
use crate::proto::universal_message::{
    Destination, Domain, RoutableMessage, SessionInfoRequest, destination,
    routable_message,
};

/// Distinct session-handshake error so callers (notably the API
/// crate's pair handler and the UI status broadcasts) can render a
/// specific message for "your key isn't paired on the car" instead of
/// the generic "decode failed" path. Mirrors tesla-control's
/// `protocol.ErrKeyNotPaired` (returned by `protocol.GetError` when the
/// car's SessionInfo response status is
/// `SESSION_INFO_STATUS_KEY_NOT_ON_WHITELIST`).
#[derive(Debug, Error)]
pub enum SessionError {
    /// Tesla returned a SessionInfo whose `status` field explicitly
    /// says our public key isn't on the car's whitelist. The user
    /// needs to re-run the pair flow and tap the card on the
    /// center-console NFC reader — this is NOT a transport bug or a
    /// retryable error.
    #[error(
        "key not paired with car (SESSION_INFO_STATUS_KEY_NOT_ON_WHITELIST) — \
         re-pair from the SentryUSB UI and tap your card on the center console"
    )]
    KeyNotPaired,
    /// Any other handshake failure (decode error, transport error,
    /// unexpected payload shape). Carries the underlying anyhow chain
    /// so existing call sites that just bail/propagate don't lose
    /// diagnostic detail.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Result of one SessionInfoRequest exchange. The raw bytes match
/// what tesla-control writes into its session-cache JSON, so we
/// can mirror its on-disk format byte-for-byte.
#[derive(Debug, Clone)]
pub struct SessionInfoResponse {
    pub domain: Domain,
    /// Raw SessionInfo proto bytes — keep for cache-file compat.
    pub raw: Vec<u8>,
    pub parsed: SessionInfo,
}

/// Send SessionInfoRequest to `domain` and decode the response.
///
/// Returns `SessionError::KeyNotPaired` if the car responds with
/// `SESSION_INFO_STATUS_KEY_NOT_ON_WHITELIST` — distinct from generic
/// transport/decode failures so the caller can render a clear "re-pair
/// your card" error in the UI instead of "AES-GCM decrypt failed three
/// minutes later because we tried to use an unauthorized session."
pub async fn request_session_info(
    conn: &mut Connection,
    keypair: &KeyPair,
    domain: Domain,
) -> std::result::Result<SessionInfoResponse, SessionError> {
    let payload = build_request(keypair, domain);
    debug!(
        "session-info: TX {} bytes to {:?}",
        payload.len(),
        domain,
    );
    // Correlate the response to our request (echoed routing address +
    // originating domain) so a stale straggler on the shared session
    // can't be mistaken for the handshake reply. See `crate::correlate`.
    let (our_uuid, our_addr) = crate::correlate::our_correlators(&payload);
    let response = conn
        .round_trip(&payload, Duration::from_secs(10), |b| {
            crate::correlate::is_response_to(b, &our_uuid, &our_addr, domain)
        })
        .await
        .context("session-info round-trip")?;
    debug!("session-info: RX {} bytes", response.len());
    parse_response(&response, domain)
}

fn build_request(keypair: &KeyPair, domain: Domain) -> Vec<u8> {
    let from_uuid = random_uuid_bytes();
    let req_uuid = random_uuid_bytes();
    let msg = RoutableMessage {
        to_destination: Some(Destination {
            sub_destination: Some(destination::SubDestination::Domain(domain as i32)),
        }),
        from_destination: Some(Destination {
            sub_destination: Some(destination::SubDestination::RoutingAddress(
                from_uuid.to_vec(),
            )),
        }),
        payload: Some(routable_message::Payload::SessionInfoRequest(
            SessionInfoRequest {
                public_key: keypair.pub_uncompressed.clone(),
                challenge: Vec::new(),
            },
        )),
        uuid: req_uuid.to_vec(),
        ..Default::default()
    };
    msg.encode_to_vec()
}

fn parse_response(
    bytes: &[u8],
    domain: Domain,
) -> std::result::Result<SessionInfoResponse, SessionError> {
    debug!("session-info RX hex: {}", hex::encode(bytes));
    let routable = RoutableMessage::decode(bytes)
        .context("decode outer Routable")?;
    debug!("session-info RX decoded: {:#?}", routable);

    // Newer car firmware answers a SessionInfoRequest from an unenrolled
    // key with a bare MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID status instead of
    // a SessionInfo carrying KEY_NOT_ON_WHITELIST. Both mean the same
    // thing — the key isn't paired — so map both to KeyNotPaired, or the
    // UI's pairing probe reads this as "couldn't reach the car" and tells
    // the user their Bluetooth adapter is bad instead of "re-pair and tap
    // your card" (seen live on a Pi 4B against 2026 firmware).
    const MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID: i32 = 3;
    if routable
        .signed_message_status
        .as_ref()
        .is_some_and(|s| s.signed_message_fault == MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID)
    {
        warn!(
            "session-info from {:?}: car returned MESSAGEFAULT_ERROR_UNKNOWN_KEY_ID — \
             our public key is not paired with this VIN. \
             User must re-pair from the SentryUSB UI and tap the card on the console.",
            domain,
        );
        return Err(SessionError::KeyNotPaired);
    }

    let raw = match routable.payload {
        Some(routable_message::Payload::SessionInfo(b)) => b,
        Some(other) => {
            return Err(SessionError::Other(anyhow::anyhow!(
                "expected session_info payload, got {:?}",
                other
            )));
        }
        None => {
            return Err(SessionError::Other(anyhow::anyhow!(
                "response has no payload (signed_message_status={:?}, to={:?}, from={:?})",
                routable.signed_message_status,
                routable.to_destination,
                routable.from_destination
            )));
        }
    };
    let parsed = SessionInfo::decode(raw.as_slice())
        .context("decode SessionInfo proto")?;

    // Promote KEY_NOT_ON_WHITELIST to a distinct error so the UI can
    // tell the user to re-pair their card. Matches tesla-control's
    // `protocol.GetError` behavior. Without this check the bytes
    // would parse fine, we'd derive a useless session key, and the
    // next encrypted query would fail with AES-GCM decrypt error —
    // a confusing failure mode that hides the actual problem.
    if parsed.status == SessionInfoStatus::KeyNotOnWhitelist as i32 {
        warn!(
            "session-info from {:?}: car returned KEY_NOT_ON_WHITELIST — \
             our public key is not paired with this VIN. \
             User must re-pair from the SentryUSB UI and tap the card on the console.",
            domain,
        );
        return Err(SessionError::KeyNotPaired);
    }

    info!(
        "session-info from {:?}: counter={}, clock_time={}, pubkey={} bytes, status={}",
        domain,
        parsed.counter,
        parsed.clock_time,
        parsed.public_key.len(),
        parsed.status,
    );
    Ok(SessionInfoResponse {
        domain,
        raw,
        parsed,
    })
}

fn random_uuid_bytes() -> [u8; 16] {
    let mut out = [0u8; 16];
    rand::rng().fill_bytes(&mut out);
    out
}
