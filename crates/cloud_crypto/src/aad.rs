

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    let total: usize = parts.iter().map(|p| p.len()).sum();
    let mut out = Vec::with_capacity(total);
    for p in parts {
        out.extend_from_slice(p);
    }
    out
}

pub fn lp(bytes: &[u8]) -> Vec<u8> {
    assert!(bytes.len() <= u16::MAX as usize, "AAD field length exceeds 16-bit prefix");
    let mut out = Vec::with_capacity(2 + bytes.len());
    out.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    out.extend_from_slice(bytes);
    out
}

const STR_WRAP_PWD: &[u8] = b"sentrycloud-wrap-pwd-v1";
const STR_WRAP_REC: &[u8] = b"sentrycloud-wrap-rec-v1";
const STR_WRAP_PASSKEY: &[u8] = b"sentrycloud-wrap-passkey-v2";
const STR_ROUTE: &[u8] = b"sentrycloud-route-v2";
const STR_ROUTEKEY: &[u8] = b"sentrycloud-routekey-v2";
const STR_PAIR: &[u8] = b"sentrycloud-pair-v1";
const STR_PI_LOCAL: &[u8] = b"sentrycloud-pi-local-v1";

const STR_PI_LOCAL_X25519: &[u8] = b"sentrycloud-pi-local-x25519-v1";

const STR_REKEY: &[u8] = b"sentrycloud-rekey-v2";

pub fn wrapped_dek_password(user_id: &str) -> Vec<u8> {
    concat(&[STR_WRAP_PWD, user_id.as_bytes()])
}

pub fn wrapped_dek_recovery(user_id: &str) -> Vec<u8> {
    concat(&[STR_WRAP_REC, user_id.as_bytes()])
}

pub fn wrapped_dek_passkey(user_id: &str, credential_id: &[u8]) -> Vec<u8> {
    let cred_lp = lp(credential_id);
    concat(&[STR_WRAP_PASSKEY, user_id.as_bytes(), &cred_lp])
}

pub fn route_blob(user_id: &str, uploaded_from_pi: &str, route_id: &str) -> Vec<u8> {
    let pi_lp = lp(uploaded_from_pi.as_bytes());
    let route_lp = lp(route_id.as_bytes());
    concat(&[STR_ROUTE, user_id.as_bytes(), &pi_lp, &route_lp])
}

pub fn route_key(user_id: &str, uploaded_from_pi: &str, route_id: &str) -> Vec<u8> {
    let pi_lp = lp(uploaded_from_pi.as_bytes());
    let route_lp = lp(route_id.as_bytes());
    concat(&[STR_ROUTEKEY, user_id.as_bytes(), &pi_lp, &route_lp])
}

pub fn pair(user_id: &str, pi_id: &str) -> Vec<u8> {
    concat(&[STR_PAIR, user_id.as_bytes(), pi_id.as_bytes()])
}

pub fn pi_local(pi_id: &str) -> Vec<u8> {
    concat(&[STR_PI_LOCAL, pi_id.as_bytes()])
}

pub fn pi_local_x25519(pi_id: &str) -> Vec<u8> {
    concat(&[STR_PI_LOCAL_X25519, pi_id.as_bytes()])
}

pub fn rekey(user_id: &str, pi_id: &str, new_generation: u32) -> Vec<u8> {
    let pi_lp = lp(pi_id.as_bytes());
    let gen_be = new_generation.to_be_bytes();
    concat(&[STR_REKEY, user_id.as_bytes(), &pi_lp, &gen_be])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lp_format() {

        assert_eq!(lp(b"hello"), [0x00, 0x05, b'h', b'e', b'l', b'l', b'o']);

        assert_eq!(lp(b""), [0x00, 0x00]);
    }

    #[test]
    fn route_blob_layout_v2() {
        let aad = route_blob("u123", "pi456", "r789");

        assert!(aad.starts_with(b"sentrycloud-route-v2"));

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

        let last_four: [u8; 4] = aad[aad.len() - 4..].try_into().unwrap();
        assert_eq!(last_four, [0x12, 0x34, 0x56, 0x78]);

        assert_eq!(aad.len(), b"sentrycloud-rekey-v2".len() + 1 + (2 + 2) + 4);
    }

    #[test]
    fn passkey_aad_length_prefixes_credential_id() {

        let a = wrapped_dek_passkey("u", &[0u8; 16]);
        let b = wrapped_dek_passkey("u", &[0u8; 32]);
        assert_ne!(a, b);

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

        let aad1 = route_blob("u", "a", "bc");
        let aad2 = route_blob("u", "ab", "c");
        assert_ne!(aad1, aad2, "v2 lp framing must prevent length-shift collisions");
    }
}
