

use ring::digest;

use crate::errors::CredentialsError;
use crate::kdf;

pub const SERIAL_PATH: &str = "/sys/firmware/devicetree/base/serial-number";

pub const PI_LOCAL_WRAP_SALT: &[u8] = b"SENTRYCLOUD_DEK_WRAP_v1";

pub const PI_LOCAL_WRAP_INFO: &[u8] = b"pi-key-at-rest";

pub fn route_id_from_path(file_path: &str) -> String {
    let h = digest::digest(&digest::SHA256, file_path.as_bytes());
    hex::encode(h.as_ref())
}

pub fn read_serial_number(path: &str) -> Result<Vec<u8>, CredentialsError> {
    let raw = std::fs::read(path).map_err(|_| CredentialsError::SerialMissing {
        path: path.to_string(),
    })?;
    let trimmed: Vec<u8> = raw
        .iter()
        .copied()
        .filter(|b| !matches!(b, 0x00 | b'\n' | b'\r' | b' ' | b'\t'))
        .collect();
    if trimmed.len() < 8 {
        return Err(CredentialsError::SerialTooShort { len: trimmed.len() });
    }
    Ok(trimmed)
}

pub fn derive_pi_local_wrap_key(serial: &[u8]) -> Result<[u8; 32], CredentialsError> {
    Ok(kdf::derive_32(serial, PI_LOCAL_WRAP_SALT, PI_LOCAL_WRAP_INFO)?)
}

pub const PI_KEY_SALT: &[u8] = b"sentrycloud-pi-key-v1";

pub fn derive_pi_key(master_dek: &[u8; 32], pi_id: &str) -> Result<[u8; 32], CredentialsError> {
    Ok(kdf::derive_32(master_dek, PI_KEY_SALT, pi_id.as_bytes())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_id_empty_path() {
        let id = route_id_from_path("");
        assert_eq!(id, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(id.len(), 64);
    }

    #[test]
    fn route_id_stable_for_same_path() {
        let p = "2026-01-15_14-30-00/clip-front.mp4";
        assert_eq!(route_id_from_path(p), route_id_from_path(p));
    }

    #[test]
    fn route_id_distinct_for_distinct_paths() {
        let a = route_id_from_path("a.mp4");
        let b = route_id_from_path("b.mp4");
        assert_ne!(a, b);
    }

    #[test]
    fn route_id_is_lowercase_hex_64() {
        let id = route_id_from_path("anything");
        assert_eq!(id.len(), 64);
        assert!(id.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
    }

    #[test]
    fn pi_local_wrap_key_is_deterministic_per_serial() {
        let serial = b"100000001234abcd";
        let k1 = derive_pi_local_wrap_key(serial).unwrap();
        let k2 = derive_pi_local_wrap_key(serial).unwrap();
        assert_eq!(k1, k2);
    }

    #[test]
    fn pi_local_wrap_key_differs_across_serials() {
        let k1 = derive_pi_local_wrap_key(b"serial-aaaaaaaaa").unwrap();
        let k2 = derive_pi_local_wrap_key(b"serial-bbbbbbbbb").unwrap();
        assert_ne!(k1, k2);
    }
}
