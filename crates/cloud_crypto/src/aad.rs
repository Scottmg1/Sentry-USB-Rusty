//! AAD constructors. AAD is reconstructed at decrypt time from row metadata
//! — never stored alongside the blob — so an attacker who can shift bytes in
//! storage cannot also shift the AAD. See ENCRYPTION.md §6.
//!
//! Every constructor here MUST agree byte-for-byte with the matching
//! function in `web/src/lib/encryption/format.js`. The `tests/byte_vectors.rs`
//! integration test compares against vectors emitted from that file.
//!
//! ## v1 vs v2 (AAD-format generation)
//!
//! ENCRYPTION.md §6.3 tracks per-constructor versions:
//! - **v1 (no length prefix):** `wrapped_dek_password`, `wrapped_dek_recovery`,
//!   `pair`, `pi_local`, `pi_local_x25519`. Each is either a single
//!   variable-length field after the domain string, or only fixed-length
//!   cuid IDs — no concatenation ambiguity.
//! - **v2 (length-prefixed multi-field):** `route_blob`, `route_key`,
//!   `wrapped_dek_passkey`, `rekey`. Every variable-length field after the
//!   first carries a 2-byte big-endian length prefix via [`lp`]. Defends
//!   against AAD collisions where two distinct `(a, b)` tuples with
//!   `len(a) + len(b)` constant could produce identical concatenations.

/// Concatenate the byte slices into a single owned `Vec<u8>`.
fn concat(parts: &[&[u8]]) -> Vec<u8> {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(total);
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

/// Length-prefix framing per ENCRYPTION.md §6.1: `[length:u16 BE][bytes:length]`.
/// Used inside v2 AAD constructors below. Panics on inputs > u16::MAX bytes,
/// matching the browser's explicit 16-bit cap.
pub fn lp(bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len() <= u16::MAX as usize, "AAD field length exceeds 16-bit prefix");
    let mut out = Vec::with_capacity(2 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

// ----- Domain separators. Must match format.js verbatim. ------------------

const STR_WRAP_PWD: &[u8] = b"sentrycloud-wrap-pwd-v1";
const STR_WRAP_REC: &[u8] = b"sentrycloud-wrap-rec-v1";
const STR_WRAP_PASSKEY: &[u8] = b"sentrycloud-wrap-passkey-v2";
const STR_ROUTE: &[u8] = b"sentrycloud-route-v2";
const STR_ROUTEKEY: &[u8] = b"sentrycloud-routekey-v2";
const STR_PAIR: &[u8] = b"sentrycloud-pair-v1";
const STR_PI_LOCAL: &[u8] = b"sentrycloud-pi-local-v1";

// Project-plan Amendment 1: AAD for the wrapped long-term X25519 privkey
// stored alongside `wrappedPiKeyLocal` in cloud-credentials.json. Without
// this distinct domain separator, a corrupted-state attacker could swap
// the two ciphertexts both wrapped under `localKey`. v1 because it has
// only the fixed-length cuid `piId` — no concatenation ambiguity.
const STR_PI_LOCAL_X25519: &[u8] = b"sentrycloud-pi-local-x25519-v1";

const STR_REKEY: &[u8] = b"sentrycloud-rekey-v2";

// ----- Constructors. Each returns owned bytes ready to feed to AEAD. -----

pub fn wrapped_dek_password(user_id: &str) -> Vec<u8> {
    concat(&[STR_WRAP_PWD, user_id.as_bytes()])
}

pub fn wrapped_dek_recovery(user_id: &str) -> Vec<u8> {
    concat(&[STR_WRAP_REC, user_id.as_bytes()])
}

/// AAD for `users.wrappedDekPasskey` (v2). credentialId is variable-length
/// per the WebAuthn spec (typically 16–256 B, up to 1023). Length-prefix
/// the credential bytes so userId|credentialId is unambiguous.
pub fn wrapped_dek_passkey(user_id: &str, credential_id: &[u8]) -> Vec<u8> {
    let cred_lp = lp(credential_id);
    concat(&[STR_WRAP_PASSKEY, user_id.as_bytes(), &cred_lp])
}

/// AAD for `drive_routes.routeBlob` (v2). `route_id` is the 64-char
/// lowercase-hex SHA-256 of the source clip path; ASCII bytes go into AAD.
/// `uploaded_from_pi` is variable-length (cuid in normal case, synthetic
/// `post-revoke:<piId>:<nonce>` after revoke) — length-prefixed.
pub fn route_blob(user_id: &str, uploaded_from_pi: &str, route_id: &str) -> Vec<u8> {
    let pi_lp = lp(uploaded_from_pi.as_bytes());
    let route_lp = lp(route_id.as_bytes());
    concat(&[STR_ROUTE, user_id.as_bytes(), &pi_lp, &route_lp])
}

/// AAD for `drive_routes.wrappedRouteKey` (v2). Same framing as `route_blob`.
pub fn route_key(user_id: &str, uploaded_from_pi: &str, route_id: &str) -> Vec<u8> {
    let pi_lp = lp(uploaded_from_pi.as_bytes());
    let route_lp = lp(route_id.as_bytes());
    concat(&[STR_ROUTEKEY, user_id.as_bytes(), &pi_lp, &route_lp])
}

/// AAD for the pairing transit envelope (carrying piKey from browser to Pi).
/// v1: only fixed-length cuid IDs.
pub fn pair(user_id: &str, pi_id: &str) -> Vec<u8> {
    concat(&[STR_PAIR, user_id.as_bytes(), pi_id.as_bytes()])
}

/// AAD for the Pi's locally-cached `wrappedPiKeyLocal`. v1.
pub fn pi_local(pi_id: &str) -> Vec<u8> {
    concat(&[STR_PI_LOCAL, pi_id.as_bytes()])
}

/// AAD for the wrapped long-term X25519 privkey. Project-plan Amendment 1.
/// v1 (single variable-length field over fixed-cuid piId).
pub fn pi_local_x25519(pi_id: &str) -> Vec<u8> {
    concat(&[STR_PI_LOCAL_X25519, pi_id.as_bytes()])
}

/// AAD for the post-rotation per-Pi rekey envelope (ENCRYPTION.md §16
/// step 5b, v2). `new_generation` is encoded as a fixed-length big-endian
/// u32 — unambiguous trailer, no length prefix needed. `pi_id` is
/// variable-length, so it gets `lp()`.
pub fn rekey(user_id: &str, pi_id: &str, new_generation: u32) -> Vec<u8> {
    let pi_lp = lp(pi_id.as_bytes());
    let gen_be = new_generation.to_be_bytes();
    concat(&[STR_REKEY, user_id.as_bytes(), &pi_lp, &gen_be])
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real byte-vector parity with the browser is enforced by
    // tests/byte_vectors.rs against output emitted from format.js.
    // These tests exercise local invariants only.

    #[test]
    fn lp_format() {
        // 5-byte field: prefix [0x00, 0x05] then bytes.
        assert_eq!(lp(b"hello"), [0x00, 0x05, b'h', b'e', b'l', b'l', b'o']);
        // Empty field: prefix [0x00, 0x00], no bytes.
        assert_eq!(lp(b""), [0x00, 0x00]);
    }

    #[test]
    fn route_blob_layout_v2() {
        let aad = route_blob("u123", "pi456", "r789");
        // Domain prefix
        assert!(aad.starts_with(b"sentrycloud-route-v2"));
        // Length: prefix(20) + "u123"(4) + lp("pi456")(2+5) + lp("r789")(2+4) = 37
        assert_eq!(aad.len(), b"sentrycloud-route-v2".len() + 4 + (2 + 5) + (2 + 4));
    }

    #[test]
    fn route_key_layout_v2() {
        let aad = route_key("u", "pi", "r");
        assert!(aad.starts_with(b"sentrycloud-routekey-v2"));
        assert_eq!(aad.len(), b"sentrycloud-routekey-v2".len() + 1 + (2 + 2) + (2 + 1));
    }

    #[test]
    fn rekey_uses_lp_pi_and_be_u32_gen() {
        let aad = rekey("u", "pi", 0x12345678);
        // Trailer is the 4-byte BE u32.
        let last_four: [u8; 4] = aad[aad.len() - 4..].try_into().unwrap();
        assert_eq!(last_four, [0x12, 0x34, 0x56, 0x78]);
        // Length: prefix(20) + "u"(1) + lp("pi")(2+2) + 4 = 29
        assert_eq!(aad.len(), b"sentrycloud-rekey-v2".len() + 1 + (2 + 2) + 4);
    }

    #[test]
    fn passkey_aad_length_prefixes_credential_id() {
        // Credential lengths differ; AAD lengths must differ.
        let a = wrapped_dek_passkey("u", &[0u8; 16]);
        let b = wrapped_dek_passkey("u", &[0u8; 32]);
        assert_ne!(a, b);
        // The first 2 bytes of credential framing are 0x00 0x10 (16).
        let domain_plus_user = b"sentrycloud-wrap-passkey-v2".len() + 1;
        assert_eq!(a[domain_plus_user], 0x00);
        assert_eq!(a[domain_plus_user + 1], 0x10);
    }

    #[test]
    fn pi_local_and_pi_local_x25519_are_distinct() {
        let a = pi_local("samepi");
        let b = pi_local_x25519("samepi");
        assert_ne!(a, b, "the two Pi-local AADs MUST differ to prevent ciphertext swap");
    }

    #[test]
    fn collision_resistance_via_lp() {
        // Pre-v2 collision target: ("a", "bc") and ("ab", "c") both produce
        // "...abc" without framing. With lp() they don't.
        let aad1 = route_blob("u", "a", "bc");
        let aad2 = route_blob("u", "ab", "c");
        assert_ne!(aad1, aad2, "v2 lp framing must prevent length-shift collisions");
    }
}
