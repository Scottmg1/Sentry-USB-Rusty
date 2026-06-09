// aes-gcm 0.10 deprecates `Nonce::from_slice` during its generic-array
// 1.x migration; the replacements are verbose. Revisit on aes-gcm 0.11+.
#![allow(deprecated)]

//! AES-128-GCM signing for authenticated commands.
//!
//! Once `session::request_session_info` has returned a vehicle pubkey,
//! `crypto::derive_session_key` gives us the AES-128 session key for
//! that domain. This module wraps inner protobuf payloads in the
//! authenticated `RoutableMessage` envelope the car expects.
//!
//! **Wire format** (per Tesla's vehicle-command spec; see
//! `pkg/protocol/protocol/signatures` in their open-source repo):
//!
//! 1. Metadata = TLV stream of `tag(1) || len(1) || value`, in this
//!    fixed order:
//!      - TAG_SIGNATURE_TYPE (0x00) = 5 (AES_GCM_PERSONALIZED)
//!      - TAG_DOMAIN (0x01) = the target domain enum value
//!      - TAG_PERSONALIZATION (0x02) = VIN as 17-byte ASCII
//!      - TAG_EPOCH (0x03) = 16 bytes from SessionInfo
//!      - TAG_EXPIRES_AT (0x04) = 4 bytes BE, vehicle clock + window
//!      - TAG_COUNTER (0x05) = 4 bytes BE, our monotonic counter
//!      - TAG_FLAGS (0x07) = 4 bytes BE
//!      - TAG_END (0xFF) = no length byte, just the single 0xFF
//!
//! 2. AES-128-GCM:
//!      - key:   16-byte session key (from `crypto::derive_session_key`)
//!      - nonce: 12 random bytes (uniqueness via counter is in metadata)
//!      - plaintext: inner payload protobuf bytes
//!      - AAD:   the full metadata stream from step 1
//!      - output: ciphertext (same length as plaintext) + 16-byte tag
//!
//! 3. SignatureData proto wraps the (epoch, nonce, counter, expires_at,
//!    tag) so the car can recompute step 1 metadata + verify.
//!
//! 4. Outer RoutableMessage:
//!      - to_destination = target Domain
//!      - from_destination = our 16-byte routing UUID
//!      - payload = protobuf_message_as_bytes(ciphertext)
//!      - signature_data = the SignatureData from step 3
//!      - uuid = random 16 bytes

use anyhow::{Context, Result};
use prost::Message;
use rand::Rng;
use sha2::{Digest, Sha256};
use tracing::debug;

use crate::crypto::SessionKey;
use crate::proto::signatures::{
    AesGcmPersonalizedSignatureData, KeyIdentity, SignatureData, key_identity,
    signature_data,
};
use crate::proto::universal_message::{
    Destination, Domain, RoutableMessage, destination, routable_message,
};

/// TLV tag bytes (matches `enum Tag` in signatures.proto).
mod tag {
    pub const SIGNATURE_TYPE: u8 = 0;
    pub const DOMAIN: u8 = 1;
    pub const PERSONALIZATION: u8 = 2;
    pub const EPOCH: u8 = 3;
    pub const EXPIRES_AT: u8 = 4;
    pub const COUNTER: u8 = 5;
    pub const FLAGS: u8 = 7;
    pub const REQUEST_HASH: u8 = 8;
    pub const FAULT: u8 = 9;
    pub const END: u8 = 0xff;
}

/// SignatureType enum values used in metadata + dispatch.
const SIG_TYPE_AES_GCM_PERSONALIZED: u8 = 5;
const SIG_TYPE_AES_GCM_RESPONSE: u8 = 9;

/// AAD that AES-GCM signs over: SHA-256 of the metadata TLV stream, no
/// prefix or framing. Verified against captured tesla-control traffic —
/// raw metadata or SHA256(0x01 || metadata) don't verify.
fn metadata_aad(metadata: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(metadata);
    h.finalize().into()
}

/// Build the canonical metadata TLV stream used as the AES-GCM AAD and
/// the basis for the car's signature reconstruction. Tag order is fixed
/// (a reorder breaks the signature). FLAGS is always written, even when
/// zero — Tesla's `AddUint32` always emits the 4-byte value, and the
/// signature won't verify otherwise.
fn build_metadata(
    domain: Domain,
    vin: &[u8],
    epoch: &[u8],
    expires_at: u32,
    counter: u32,
    flags: u32,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    write_tlv(&mut buf, tag::SIGNATURE_TYPE, &[SIG_TYPE_AES_GCM_PERSONALIZED]);
    write_tlv(&mut buf, tag::DOMAIN, &[(domain as i32) as u8]);
    write_tlv(&mut buf, tag::PERSONALIZATION, vin);
    write_tlv(&mut buf, tag::EPOCH, epoch);
    write_tlv(&mut buf, tag::EXPIRES_AT, &expires_at.to_be_bytes());
    write_tlv(&mut buf, tag::COUNTER, &counter.to_be_bytes());
    write_tlv(&mut buf, tag::FLAGS, &flags.to_be_bytes());
    buf.push(tag::END);
    buf
}

/// Build the response-side metadata TLV stream. Receiver reconstructs
/// this from response.from_destination.domain, response.flags, the
/// SignatureData fields, the originating request's tag bytes, and the
/// response's signed_message_fault. Tag order matches Tesla's
/// `peer.go::responseMetadata`.
///
/// Note: this is what gets fed to `SHA-256` for the AAD that AES-GCM
/// uses to verify the response. Computed locally; never transmitted.
fn build_response_metadata(
    from_domain: Domain,
    vin: &[u8],
    counter: u32,
    flags: u32,
    request_sig_type: u8,
    request_tag: &[u8],
    fault: u32,
) -> Vec<u8> {
    // REQUEST_HASH = [request_sig_type byte] || request tag bytes.
    // For AES_GCM_PERSONALIZED requests, sig_type = 5 and tag is 16
    // bytes, so REQUEST_HASH is 17 bytes.
    let mut request_hash = Vec::with_capacity(1 + request_tag.len());
    request_hash.push(request_sig_type);
    request_hash.extend_from_slice(request_tag);

    let mut buf = Vec::with_capacity(96);
    write_tlv(&mut buf, tag::SIGNATURE_TYPE, &[SIG_TYPE_AES_GCM_RESPONSE]);
    write_tlv(&mut buf, tag::DOMAIN, &[(from_domain as i32) as u8]);
    write_tlv(&mut buf, tag::PERSONALIZATION, vin);
    write_tlv(&mut buf, tag::COUNTER, &counter.to_be_bytes());
    write_tlv(&mut buf, tag::FLAGS, &flags.to_be_bytes());
    write_tlv(&mut buf, tag::REQUEST_HASH, &request_hash);
    write_tlv(&mut buf, tag::FAULT, &fault.to_be_bytes());
    buf.push(tag::END);
    buf
}

fn write_tlv(buf: &mut Vec<u8>, tag: u8, value: &[u8]) {
    // Tesla uses single-byte length. All known tag values fit (epoch
    // is 16 bytes max, personalization is 17 bytes for VINs, etc.).
    debug_assert!(value.len() <= u8::MAX as usize, "TLV value too long");
    buf.push(tag);
    buf.push(value.len() as u8);
    buf.extend_from_slice(value);
}

/// Encrypt + sign an inner payload, returning the SignedMessage parts
/// that go into the outer RoutableMessage.
pub struct SignedParts {
    /// Ciphertext to put in `RoutableMessage.protobuf_message_as_bytes`.
    pub ciphertext: Vec<u8>,
    /// SignatureData proto for `RoutableMessage.signature_data`.
    pub signature_data: SignatureData,
    /// Counter actually used (passed back so caller can persist for
    /// the next request's counter > this one).
    pub counter: u32,
}

/// Encrypt + sign one message.
///
/// `our_pubkey_sec1` is our 65-byte uncompressed public key — included
/// in the SignatureData so the car can pick the right whitelist entry.
/// `counter` MUST be strictly greater than any counter previously used
/// with this (key, domain, epoch) tuple, or the car will reject as a
/// replay. Caller manages counter persistence across runs.
#[allow(clippy::too_many_arguments)]
pub fn sign(
    session_key: &SessionKey,
    our_pubkey_sec1: &[u8],
    inner: &[u8],
    domain: Domain,
    vin: &[u8],
    epoch: &[u8],
    expires_at: u32,
    counter: u32,
    flags: u32,
) -> Result<SignedParts> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes128Gcm, Nonce};

    let metadata = build_metadata(domain, vin, epoch, expires_at, counter, flags);
    let aad = metadata_aad(&metadata);
    debug!(
        "AES-GCM metadata ({} bytes): {}",
        metadata.len(),
        hex::encode(&metadata)
    );
    debug!(
        "AES-GCM AAD = SHA-256(metadata) = {}",
        hex::encode(aad)
    );

    // Random 12-byte nonce. AES-GCM needs only per-(key,message)
    // uniqueness, and replay protection comes from the metadata counter.
    let mut nonce_bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut nonce_bytes);

    let cipher = Aes128Gcm::new_from_slice(session_key.as_bytes())
        .context("Aes128Gcm new_from_slice")?;
    let nonce = Nonce::from_slice(&nonce_bytes);
    // aes-gcm's `encrypt` with Payload returns ciphertext with the
    // 16-byte tag appended at the end.
    let mut combined = cipher
        .encrypt(
            nonce,
            Payload {
                msg: inner,
                aad: &aad,
            },
        )
        .map_err(|e| anyhow::anyhow!("AES-GCM encrypt: {e}"))?;

    // Split off the 16-byte tag from the end. Both ciphertext and
    // tag travel separately in Tesla's wire format.
    if combined.len() < 16 {
        anyhow::bail!("aes-gcm output too short to contain a tag");
    }
    let tag = combined.split_off(combined.len() - 16);
    let ciphertext = combined;

    let signature_data = SignatureData {
        signer_identity: Some(KeyIdentity {
            identity_type: Some(key_identity::IdentityType::PublicKey(
                our_pubkey_sec1.to_vec(),
            )),
        }),
        sig_type: Some(signature_data::SigType::AesGcmPersonalizedData(
            AesGcmPersonalizedSignatureData {
                epoch: epoch.to_vec(),
                nonce: nonce_bytes.to_vec(),
                counter,
                expires_at,
                tag,
            },
        )),
    };

    Ok(SignedParts {
        ciphertext,
        signature_data,
        counter,
    })
}

/// Decrypt an AES_GCM_RESPONSE the car sent back. Needs the original
/// request's tag bytes (for REQUEST_HASH binding), the response's
/// own metadata fields (from `RoutableMessage` + its `SignatureData`),
/// and the same session key used for the request.
///
/// Caller flow:
///   1. Decode the inbound bytes as `RoutableMessage`.
///   2. Pull out the response's `signature_data.aes_gcm_response_data`
///      (nonce, counter, tag) and the encrypted `protobuf_message_as_bytes`.
///   3. Pass everything here along with the originating request's
///      tag (the 16-byte AES-GCM tag we sent on the way out).
///   4. On success, returns the inner plaintext bytes — a
///      car_server response payload for INFOTAINMENT, etc.
pub fn decrypt_response(
    session_key: &SessionKey,
    request_tag: &[u8],
    from_domain: Domain,
    vin: &[u8],
    response_flags: u32,
    response_counter: u32,
    response_fault: u32,
    response_nonce: &[u8],
    response_tag: &[u8],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes128Gcm, Nonce};

    let metadata = build_response_metadata(
        from_domain,
        vin,
        response_counter,
        response_flags,
        SIG_TYPE_AES_GCM_PERSONALIZED,
        request_tag,
        response_fault,
    );
    let aad = metadata_aad(&metadata);
    debug!(
        "response metadata ({} bytes): {}",
        metadata.len(),
        hex::encode(&metadata)
    );
    debug!("response AAD: {}", hex::encode(aad));

    let cipher = Aes128Gcm::new_from_slice(session_key.as_bytes())
        .context("Aes128Gcm new_from_slice")?;
    if response_nonce.len() != 12 {
        anyhow::bail!("response nonce must be 12 bytes, got {}", response_nonce.len());
    }
    let nonce = Nonce::from_slice(response_nonce);

    // aes-gcm's `decrypt` API expects ciphertext + tag concatenated.
    let mut combined = ciphertext.to_vec();
    combined.extend_from_slice(response_tag);

    cipher
        .decrypt(nonce, Payload { msg: &combined, aad: &aad })
        .map_err(|e| anyhow::anyhow!("AES-GCM response decrypt: {e}"))
}

/// Build the full outer RoutableMessage bytes ready to send over GATT.
///
/// `flags` MUST match the flags value used when computing the metadata
/// for `sign()` — the car reconstructs metadata from
/// SignatureData fields + RoutableMessage.flags, so a mismatch produces
/// INVALID_SIGNATURE. tesla-control sets flags=2 (FLAG_ENCRYPT_RESPONSE)
/// for state queries; matching it is required for the signature to
/// verify on those paths.
pub fn build_signed_routable_message(
    parts: &SignedParts,
    domain: Domain,
    flags: u32,
) -> Vec<u8> {
    let mut from_uuid = [0u8; 16];
    let mut req_uuid = [0u8; 16];
    rand::rng().fill_bytes(&mut from_uuid);
    rand::rng().fill_bytes(&mut req_uuid);

    let msg = RoutableMessage {
        to_destination: Some(Destination {
            sub_destination: Some(destination::SubDestination::Domain(domain as i32)),
        }),
        from_destination: Some(Destination {
            sub_destination: Some(destination::SubDestination::RoutingAddress(
                from_uuid.to_vec(),
            )),
        }),
        payload: Some(routable_message::Payload::ProtobufMessageAsBytes(
            parts.ciphertext.clone(),
        )),
        sub_sig_data: Some(routable_message::SubSigData::SignatureData(
            parts.signature_data.clone(),
        )),
        uuid: req_uuid.to_vec(),
        flags,
        ..Default::default()
    };
    msg.encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// build_metadata is deterministic for fixed inputs — guards against
    /// hashing random bytes into the AAD.
    #[test]
    fn metadata_is_deterministic() {
        let vin = b"1FAKEVIN000000001";
        let epoch = [0xab; 16];
        let m1 = build_metadata(Domain::VehicleSecurity, vin, &epoch, 100, 7, 0);
        let m2 = build_metadata(Domain::VehicleSecurity, vin, &epoch, 100, 7, 0);
        assert_eq!(m1, m2);
    }

    /// Metadata layout sanity: starts with SIGNATURE_TYPE TLV, ends with
    /// the bare 0xFF terminator — catches a tag-order/terminator break
    /// here instead of as an opaque car-side INVALID_SIGNATURE.
    #[test]
    fn metadata_starts_with_sig_type_ends_with_end_marker() {
        let vin = b"1FAKEVIN000000001";
        let epoch = [0xab; 16];
        let m = build_metadata(Domain::VehicleSecurity, vin, &epoch, 100, 7, 0);
        assert_eq!(m[0], tag::SIGNATURE_TYPE);
        assert_eq!(m[1], 1, "SIGNATURE_TYPE value is 1 byte long");
        assert_eq!(m[2], SIG_TYPE_AES_GCM_PERSONALIZED);
        assert_eq!(*m.last().unwrap(), tag::END);
    }

    /// AES-GCM round-trip with a known key: encrypt, decrypt locally,
    /// confirm we get the plaintext back. Doesn't prove the car will
    /// accept our format, but does prove the AES-GCM bookkeeping
    /// (key length, nonce length, tag handling) is right.
    #[test]
    fn aes_gcm_round_trip_locally() {
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{Aes128Gcm, Nonce};

        let key = SessionKey([0x42; 16]);
        let plaintext = b"hello tesla";
        let vin = b"1FAKEVIN000000001";
        let epoch = [0xcd; 16];
        let parts = sign(
            &key,
            &[0x04; 65],
            plaintext,
            Domain::VehicleSecurity,
            vin,
            &epoch,
            999,
            5,
            0,
        )
        .unwrap();

        // Reconstruct metadata + recompute AAD = SHA-256(metadata) +
        // decrypt to confirm the tag is good.
        let metadata = build_metadata(
            Domain::VehicleSecurity,
            vin,
            &epoch,
            999,
            5,
            0,
        );
        let aad = metadata_aad(&metadata);
        let cipher = Aes128Gcm::new_from_slice(key.as_bytes()).unwrap();
        let sig = match parts.signature_data.sig_type {
            Some(signature_data::SigType::AesGcmPersonalizedData(s)) => s,
            _ => panic!("expected AES_GCM_Personalized"),
        };
        let mut combined = parts.ciphertext.clone();
        combined.extend_from_slice(&sig.tag);
        let decrypted = cipher
            .decrypt(
                Nonce::from_slice(&sig.nonce),
                Payload {
                    msg: &combined,
                    aad: &aad,
                },
            )
            .expect("decrypt round-trip");
        assert_eq!(decrypted, plaintext);
    }

    /// Known-answer regression test for the full sign chain (pinned hex
    /// from deterministic inputs). Fires if the metadata TLV layout, AAD
    /// hashing, or AES-GCM bookkeeping changes.
    #[test]
    fn signed_known_answer() {
        use aes_gcm::aead::{Aead, KeyInit, Payload};
        use aes_gcm::{Aes128Gcm, Nonce};

        let key = SessionKey([0x42; 16]);
        let nonce_bytes: [u8; 12] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06,
            0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c,
        ];
        let vin = b"1FAKEVIN000000001";
        let epoch = [0xcd; 16];
        let expires_at: u32 = 1000;
        let counter: u32 = 5;
        let flags: u32 = 2;
        let plaintext = b"hello tesla";

        let metadata = build_metadata(
            Domain::Infotainment, vin, &epoch, expires_at, counter, flags,
        );
        let aad = metadata_aad(&metadata);
        let cipher = Aes128Gcm::new_from_slice(key.as_bytes()).unwrap();
        let combined = cipher
            .encrypt(
                Nonce::from_slice(&nonce_bytes),
                Payload { msg: plaintext, aad: &aad },
            )
            .unwrap();
        let (ct, tag) = combined.split_at(combined.len() - 16);

        // Pinned output of build_metadata + metadata_aad + AES-128-GCM
        // for the inputs above.
        assert_eq!(hex::encode(ct), "d057120dadce8156be5ac3");
        assert_eq!(hex::encode(tag), "10a6a4ec5eddfbe1a9dc0eb829df2e64");
    }

    /// Response-path mirror of `signed_known_answer`: pins the response
    /// metadata + AAD + AES-GCM chain.
    #[test]
    fn response_known_answer() {
        let key = SessionKey([0x42; 16]);
        let nonce: [u8; 12] = [
            0x30, 0x31, 0x32, 0x33, 0x34, 0x35,
            0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b,
        ];
        let request_tag = [0xab; 16];
        let ciphertext = hex::decode("e8b92542c9d2fd61cd25").unwrap();
        let tag        = hex::decode("15ef215d26014dd52e7ae1097fac9a8b").unwrap();

        let plaintext = decrypt_response(
            &key,
            &request_tag,
            Domain::Infotainment,
            b"1FAKEVIN000000001",
            2,    // response_flags
            7,    // response_counter
            0,    // response_fault
            &nonce,
            &tag,
            &ciphertext,
        )
        .expect("response decrypt should succeed for matching inputs");
        assert_eq!(plaintext, b"climate ok");
    }
}
