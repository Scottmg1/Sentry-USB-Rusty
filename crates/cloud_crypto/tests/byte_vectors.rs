//! Byte-for-byte parity test against the browser's `format.js`.
//!
//! `aad-vectors.json` is emitted from `SentryCloud/scripts/emit-aad-vectors.mjs`
//! (see that file's header for re-run instructions). If the vectors here ever
//! drift from the browser, the protocol is broken — every packet between
//! Pi and browser will fail AEAD verification.

use serde::Deserialize;
use sentryusb_cloud_crypto::aad;

#[derive(Debug, Deserialize)]
struct Vectors {
    inputs: Inputs,
    aad: AadHex,
    #[serde(rename = "collisionProbe")]
    collision_probe: CollisionProbe,
    constants: Constants,
}

#[derive(Debug, Deserialize)]
struct Inputs {
    #[serde(rename = "userId")]
    user_id: String,
    #[serde(rename = "piId")]
    pi_id: String,
    #[serde(rename = "routeId")]
    route_id: String,
    #[serde(rename = "credentialIdHex")]
    credential_id_hex: String,
    #[serde(rename = "synthPiId")]
    synth_pi_id: String,
}

#[derive(Debug, Deserialize)]
struct AadHex {
    #[serde(rename = "wrappedDekPassword")]
    wrapped_dek_password: String,
    #[serde(rename = "wrappedDekRecovery")]
    wrapped_dek_recovery: String,
    #[serde(rename = "wrappedDekPasskey")]
    wrapped_dek_passkey: String,
    #[serde(rename = "routeKey")]
    route_key: String,
    #[serde(rename = "routeBlob")]
    route_blob: String,
    #[serde(rename = "routeKeyPostRevoke")]
    route_key_post_revoke: String,
    #[serde(rename = "routeBlobPostRevoke")]
    route_blob_post_revoke: String,
    #[serde(rename = "rekeyGen0")]
    rekey_gen0: String,
    #[serde(rename = "rekeyGen1")]
    rekey_gen1: String,
    #[serde(rename = "rekeyGenLarge")]
    rekey_gen_large: String,
}

#[derive(Debug, Deserialize)]
struct CollisionProbe {
    #[serde(rename = "routeBlob_a_bc")]
    route_blob_a_bc: String,
    #[serde(rename = "routeBlob_ab_c")]
    route_blob_ab_c: String,
}

#[derive(Debug, Deserialize)]
struct Constants {
    #[serde(rename = "VERSION")]
    version: u8,
    #[serde(rename = "NONCE_LEN")]
    nonce_len: usize,
    #[serde(rename = "TAG_LEN")]
    tag_len: usize,
    #[serde(rename = "KEY_LEN")]
    key_len: usize,
    #[serde(rename = "SALT_LEN")]
    salt_len: usize,
    #[serde(rename = "WRAPPED_DEK_BLOB_LEN")]
    wrapped_dek_blob_len: usize,
}

fn load_vectors() -> Vectors {
    let json = include_str!("aad-vectors.json");
    serde_json::from_str(json).expect("aad-vectors.json must be valid JSON")
}

fn h(rust_bytes: Vec<u8>) -> String {
    hex::encode(rust_bytes)
}

#[test]
fn aad_constructors_match_browser() {
    let v = load_vectors();
    let cred_id = hex::decode(&v.inputs.credential_id_hex).unwrap();

    assert_eq!(
        h(aad::wrapped_dek_password(&v.inputs.user_id)),
        v.aad.wrapped_dek_password,
        "wrappedDekPassword AAD must match browser",
    );
    assert_eq!(
        h(aad::wrapped_dek_recovery(&v.inputs.user_id)),
        v.aad.wrapped_dek_recovery,
        "wrappedDekRecovery AAD must match browser",
    );
    assert_eq!(
        h(aad::wrapped_dek_passkey(&v.inputs.user_id, &cred_id)),
        v.aad.wrapped_dek_passkey,
        "wrappedDekPasskey AAD (v2 with lp) must match browser",
    );
    assert_eq!(
        h(aad::route_key(
            &v.inputs.user_id,
            &v.inputs.pi_id,
            &v.inputs.route_id,
        )),
        v.aad.route_key,
        "routeKey AAD (v2 with lp) must match browser",
    );
    assert_eq!(
        h(aad::route_blob(
            &v.inputs.user_id,
            &v.inputs.pi_id,
            &v.inputs.route_id,
        )),
        v.aad.route_blob,
        "routeBlob AAD (v2 with lp) must match browser",
    );
    assert_eq!(
        h(aad::route_key(
            &v.inputs.user_id,
            &v.inputs.synth_pi_id,
            &v.inputs.route_id,
        )),
        v.aad.route_key_post_revoke,
        "routeKey AAD with synthetic post-revoke piId must match browser",
    );
    assert_eq!(
        h(aad::route_blob(
            &v.inputs.user_id,
            &v.inputs.synth_pi_id,
            &v.inputs.route_id,
        )),
        v.aad.route_blob_post_revoke,
        "routeBlob AAD with synthetic post-revoke piId must match browser",
    );
    assert_eq!(
        h(aad::rekey(&v.inputs.user_id, &v.inputs.pi_id, 0)),
        v.aad.rekey_gen0,
        "rekey AAD (gen=0) must match browser",
    );
    assert_eq!(
        h(aad::rekey(&v.inputs.user_id, &v.inputs.pi_id, 1)),
        v.aad.rekey_gen1,
        "rekey AAD (gen=1) must match browser",
    );
    assert_eq!(
        h(aad::rekey(&v.inputs.user_id, &v.inputs.pi_id, 0x12345678)),
        v.aad.rekey_gen_large,
        "rekey AAD (gen=0x12345678 BE u32) must match browser",
    );
}

#[test]
fn collision_probe_matches_browser() {
    let v = load_vectors();
    assert_eq!(
        h(aad::route_blob(&v.inputs.user_id, "a", "bc")),
        v.collision_probe.route_blob_a_bc,
    );
    assert_eq!(
        h(aad::route_blob(&v.inputs.user_id, "ab", "c")),
        v.collision_probe.route_blob_ab_c,
    );
    assert_ne!(
        v.collision_probe.route_blob_a_bc, v.collision_probe.route_blob_ab_c,
        "v2 lp framing must produce distinct AADs for length-shifted inputs",
    );
}

#[test]
fn protocol_constants_match_browser() {
    use sentryusb_cloud_crypto::blob::{
        KEY_LEN, NONCE_LEN, SALT_LEN, TAG_LEN, VERSION, WRAPPED_KEY_BLOB_LEN,
    };
    let v = load_vectors();
    assert_eq!(VERSION, v.constants.version, "VERSION byte");
    assert_eq!(NONCE_LEN, v.constants.nonce_len, "NONCE_LEN");
    assert_eq!(TAG_LEN, v.constants.tag_len, "TAG_LEN");
    assert_eq!(KEY_LEN, v.constants.key_len, "KEY_LEN");
    assert_eq!(SALT_LEN, v.constants.salt_len, "SALT_LEN");
    assert_eq!(
        WRAPPED_KEY_BLOB_LEN, v.constants.wrapped_dek_blob_len,
        "wrapped DEK blob length (1+12+16+32 = 61)",
    );
}
