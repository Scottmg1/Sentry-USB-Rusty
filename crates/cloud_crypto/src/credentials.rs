

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::aead::{self, Key};
use crate::blob::WRAPPED_KEY_BLOB_LEN;
use crate::errors::CredentialsError;
use crate::{aad, x25519};

pub const DEFAULT_PATH: &str = "/root/.sentryusb/cloud-credentials.json";

#[cfg(unix)]
const FILE_MODE: u32 = 0o600;

#[cfg(unix)]
const DIR_MODE: u32 = 0o700;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LongTermX25519OnDisk {

    #[serde(rename = "publicKey")]
    pub public_key: String,

    #[serde(rename = "wrappedPrivateKey")]
    pub wrapped_private_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudCredentialsV1 {
    pub version: u32,
    #[serde(rename = "userId")]
    pub user_id: String,
    #[serde(rename = "piId")]
    pub pi_id: String,

    #[serde(rename = "piAuthToken")]
    pub pi_auth_token: String,

    #[serde(rename = "wrappedPiKeyLocal")]
    pub wrapped_pi_key_local: String,

    #[serde(rename = "longTermX25519")]
    pub long_term_x25519: LongTermX25519OnDisk,
    #[serde(rename = "cloudBaseUrl")]
    pub cloud_base_url: String,

    #[serde(rename = "pairedAt")]
    pub paired_at: DateTime<Utc>,

    #[serde(rename = "dekRotationGeneration", default)]
    pub dek_rotation_generation: u32,
}

pub fn load(path: &str) -> Result<CloudCredentialsV1, CredentialsError> {
    let contents = fs::read_to_string(path)?;
    let creds: CloudCredentialsV1 = serde_json::from_str(&contents)?;
    if creds.version != 1 {
        return Err(CredentialsError::UnsupportedVersion(creds.version));
    }
    Ok(creds)
}

pub fn save_atomic(path: &str, creds: &CloudCredentialsV1) -> Result<(), CredentialsError> {
    let final_path = PathBuf::from(path);
    if let Some(parent) = final_path.parent() {
        ensure_parent_dir(parent)?;
    }

    let json = serde_json::to_vec_pretty(creds)?;
    let tmp_path = with_tmp_suffix(&final_path);

    {
        let mut opts = OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(FILE_MODE);
        }
        let mut f = opts.open(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }

    fs::rename(&tmp_path, &final_path)?;
    Ok(())
}

pub fn secure_delete(path: &str) -> Result<(), CredentialsError> {
    let final_path = PathBuf::from(path);
    if !final_path.exists() {
        return Ok(());
    }

    let len = fs::metadata(&final_path)?.len() as usize;
    let mut overwrite = vec![0u8; len];
    use ring::rand::SecureRandom;
    ring::rand::SystemRandom::new()
        .fill(&mut overwrite)
        .map_err(|_| crate::errors::CryptoError::SealFailed)?;

    {
        let mut opts = OpenOptions::new();
        opts.write(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(FILE_MODE);
        }
        let mut f = opts.open(&final_path)?;
        f.write_all(&overwrite)?;
        f.sync_all()?;
    }
    fs::remove_file(&final_path)?;
    Ok(())
}

fn ensure_parent_dir(parent: &Path) -> Result<(), CredentialsError> {
    if parent.exists() {
        return Ok(());
    }
    fs::create_dir_all(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(DIR_MODE);
        fs::set_permissions(parent, perms)?;
    }
    Ok(())
}

fn with_tmp_suffix(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

pub fn wrap_pi_key_local(
    local_wrap_key: &[u8; 32],
    pi_key: &[u8; 32],
    pi_id: &str,
) -> Result<String, CredentialsError> {
    let key = Key::from_bytes(local_wrap_key)?;
    let aad = aad::pi_local(pi_id);
    let blob = aead::seal(&key, &aad, pi_key)?;
    debug_assert_eq!(blob.len(), WRAPPED_KEY_BLOB_LEN);
    Ok(B64.encode(&blob))
}

pub fn unwrap_pi_key_local(
    local_wrap_key: &[u8; 32],
    wrapped_b64: &str,
    pi_id: &str,
) -> Result<[u8; 32], CredentialsError> {
    let blob = B64
        .decode(wrapped_b64)
        .map_err(|_| CredentialsError::Base64Decode)?;
    let key = Key::from_bytes(local_wrap_key)?;
    let aad = aad::pi_local(pi_id);
    let out = aead::open(&key, &aad, &blob)?;
    out.as_slice()
        .try_into()
        .map_err(|_| CredentialsError::Crypto(crate::errors::CryptoError::InvalidKeyLength {
            expected: 32,
            actual: out.len(),
        }))
}

pub fn wrap_long_term_privkey(
    local_wrap_key: &[u8; 32],
    seed: &[u8; 32],
    pi_id: &str,
) -> Result<String, CredentialsError> {
    let key = Key::from_bytes(local_wrap_key)?;
    let aad = aad::pi_local_x25519(pi_id);
    let blob = aead::seal(&key, &aad, seed)?;
    debug_assert_eq!(blob.len(), WRAPPED_KEY_BLOB_LEN);
    Ok(B64.encode(&blob))
}

pub fn unwrap_long_term_privkey(
    local_wrap_key: &[u8; 32],
    wrapped_b64: &str,
    pi_id: &str,
) -> Result<[u8; 32], CredentialsError> {
    let blob = B64
        .decode(wrapped_b64)
        .map_err(|_| CredentialsError::Base64Decode)?;
    let key = Key::from_bytes(local_wrap_key)?;
    let aad = aad::pi_local_x25519(pi_id);
    let out = aead::open(&key, &aad, &blob)?;
    out.as_slice()
        .try_into()
        .map_err(|_| CredentialsError::Crypto(crate::errors::CryptoError::InvalidKeyLength {
            expected: 32,
            actual: out.len(),
        }))
}

#[allow(clippy::too_many_arguments)]
pub fn build_v1(
    user_id: String,
    pi_id: String,
    pi_auth_token: &[u8; 32],
    pi_key: &[u8; 32],
    long_term_priv: &x25519::LongTermPrivate,
    local_wrap_key: &[u8; 32],
    cloud_base_url: String,
    paired_at: DateTime<Utc>,
    dek_rotation_generation: u32,
) -> Result<CloudCredentialsV1, CredentialsError> {
    let wrapped_pi_key_local = wrap_pi_key_local(local_wrap_key, pi_key, &pi_id)?;
    let lt_seed = long_term_priv.to_seed();
    let wrapped_lt = wrap_long_term_privkey(local_wrap_key, &lt_seed, &pi_id)?;

    Ok(CloudCredentialsV1 {
        version: 1,
        user_id,
        pi_id,
        pi_auth_token: B64.encode(pi_auth_token),
        wrapped_pi_key_local,
        long_term_x25519: LongTermX25519OnDisk {
            public_key: B64.encode(long_term_priv.public_bytes()),
            wrapped_private_key: wrapped_lt,
        },
        cloud_base_url,
        paired_at,
        dek_rotation_generation,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn fixed_local_key() -> [u8; 32] {
        [42u8; 32]
    }

    #[test]
    fn pi_key_wrap_unwrap_roundtrip() {
        let lk = fixed_local_key();
        let pi_key = [9u8; 32];
        let wrapped = wrap_pi_key_local(&lk, &pi_key, "pi-abc").unwrap();
        let unwrapped = unwrap_pi_key_local(&lk, &wrapped, "pi-abc").unwrap();
        assert_eq!(unwrapped, pi_key);
    }

    #[test]
    fn pi_key_unwrap_rejects_wrong_pi_id() {
        let lk = fixed_local_key();
        let wrapped = wrap_pi_key_local(&lk, &[7u8; 32], "pi-abc").unwrap();
        let err = unwrap_pi_key_local(&lk, &wrapped, "pi-xyz");
        assert!(err.is_err());
    }

    #[test]
    fn pi_key_and_long_term_wraps_are_not_swappable() {
        let lk = fixed_local_key();
        let pi_key = [1u8; 32];
        let lt_seed = [2u8; 32];
        let pi_id = "samepi";

        let wrapped_pi_key = wrap_pi_key_local(&lk, &pi_key, pi_id).unwrap();
        let wrapped_lt = wrap_long_term_privkey(&lk, &lt_seed, pi_id).unwrap();

        assert!(
            unwrap_pi_key_local(&lk, &wrapped_lt, pi_id).is_err(),
            "long-term ciphertext must not unwrap as pi key"
        );
        assert!(
            unwrap_long_term_privkey(&lk, &wrapped_pi_key, pi_id).is_err(),
            "pi-key ciphertext must not unwrap as long-term privkey"
        );
    }

    #[test]
    fn build_v1_then_serialize_roundtrip() {
        let lk = fixed_local_key();
        let pi_key = [3u8; 32];
        let token = [4u8; 32];
        let lt = x25519::LongTermPrivate::generate().unwrap();
        let creds = build_v1(
            "user-cuid".to_string(),
            "pi-cuid".to_string(),
            &token,
            &pi_key,
            &lt,
            &lk,
            "https://sentryusb.com".to_string(),
            Utc::now(),
            0,
        )
        .unwrap();

        let json = serde_json::to_string(&creds).unwrap();
        let parsed: CloudCredentialsV1 = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user_id, "user-cuid");
        assert_eq!(parsed.pi_id, "pi-cuid");
        assert_eq!(parsed.version, 1);

        let recovered_pi_key =
            unwrap_pi_key_local(&lk, &parsed.wrapped_pi_key_local, &parsed.pi_id).unwrap();
        assert_eq!(recovered_pi_key, pi_key);

        let recovered_seed = unwrap_long_term_privkey(
            &lk,
            &parsed.long_term_x25519.wrapped_private_key,
            &parsed.pi_id,
        )
        .unwrap();
        assert_eq!(recovered_seed, lt.to_seed());
    }

    #[test]
    fn save_and_load_atomic_roundtrip() {
        let dir = std::env::temp_dir().join(format!("scc-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("cloud-credentials.json");

        let lk = fixed_local_key();
        let lt = x25519::LongTermPrivate::generate().unwrap();
        let creds = build_v1(
            "u".to_string(),
            "p".to_string(),
            &[5u8; 32],
            &[6u8; 32],
            &lt,
            &lk,
            "https://example.test".to_string(),
            Utc::now(),
            7,
        )
        .unwrap();

        let p_str = path.to_str().unwrap();
        save_atomic(p_str, &creds).unwrap();
        let loaded = load(p_str).unwrap();
        assert_eq!(loaded.user_id, "u");
        assert_eq!(loaded.pi_id, "p");
        assert_eq!(loaded.dek_rotation_generation, 7);

        let _ = std::fs::remove_file(p_str);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn load_rejects_unknown_version() {
        let dir = std::env::temp_dir().join(format!("scc-ver-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("v999.json");
        let p_str = path.to_str().unwrap();
        std::fs::write(p_str, br#"{"version":999,"userId":"","piId":"","piAuthToken":"","wrappedPiKeyLocal":"","longTermX25519":{"publicKey":"","wrappedPrivateKey":""},"cloudBaseUrl":"","pairedAt":"2026-04-27T00:00:00Z"}"#).unwrap();
        let err = load(p_str).unwrap_err();
        assert!(matches!(err, CredentialsError::UnsupportedVersion(999)));
        let _ = std::fs::remove_file(p_str);
        let _ = std::fs::remove_dir(&dir);
    }
}
