//! Tiny adapter to expose the cloud-credentials-loaded view that the
//! local-API handlers and the uploader sweep need, without forcing them
//! to know about the on-disk JSON shape.

use anyhow::{anyhow, Result};

use sentryusb_cloud_crypto::credentials::CloudCredentialsV1;
use sentryusb_cloud_crypto::ids;

/// Decoded cloud credentials, including the unwrapped per-Pi key (used
/// at every encrypt) and the long-term X25519 keypair (used at rekey).
///
/// Built once at boot and stored alongside the on-disk struct in the
/// uploader's state. The unwrapped per-Pi key is held in process memory
/// for the lifetime of the credentials — this is the same trade the
/// browser makes (sessionStorage-backed DEK) and the root threat is
/// physical Pi compromise, not memory dump.
pub struct UnlockedCreds {
    pub on_disk: CloudCredentialsV1,
    pub pi_key: [u8; 32],
    pub long_term_priv: sentryusb_cloud_crypto::x25519::LongTermPrivate,
    pub pi_auth_token: [u8; 32],
}

impl UnlockedCreds {
    /// Derive the local wrap key from the SBC serial, unwrap the per-Pi
    /// key + long-term privkey, and decode the bearer token.
    pub fn unlock(creds: &CloudCredentialsV1) -> Result<Self> {
        // Local wrap key from the SBC serial. On non-Pi dev hosts the
        // serial-number file is missing and unlock returns an error;
        // callers can fall back to the SENTRYCLOUD_DEV_SERIAL env var
        // for tests (see `unlock_with_serial`).
        let serial = ids::read_serial_number(ids::SERIAL_PATH)
            .map_err(|e| anyhow!("read serial-number: {}", e))?;
        Self::unlock_with_serial(creds, &serial)
    }

    /// Same as `unlock`, but takes a caller-provided serial. Used by
    /// tests + by the dev fallback when the serial-number file is absent.
    pub fn unlock_with_serial(creds: &CloudCredentialsV1, serial: &[u8]) -> Result<Self> {
        let local_key = ids::derive_pi_local_wrap_key(serial)
            .map_err(|e| anyhow!("derive local wrap key: {}", e))?;

        let pi_key = sentryusb_cloud_crypto::credentials::unwrap_pi_key_local(
            &local_key,
            &creds.wrapped_pi_key_local,
            &creds.pi_id,
        )
        .map_err(|e| anyhow!("unwrap pi key: {}", e))?;

        let lt_seed = sentryusb_cloud_crypto::credentials::unwrap_long_term_privkey(
            &local_key,
            &creds.long_term_x25519.wrapped_private_key,
            &creds.pi_id,
        )
        .map_err(|e| anyhow!("unwrap long-term privkey: {}", e))?;
        let long_term_priv = sentryusb_cloud_crypto::x25519::LongTermPrivate::from_seed(lt_seed);

        let token = decode_b64_32(&creds.pi_auth_token).ok_or_else(|| anyhow!("bad piAuthToken"))?;

        Ok(UnlockedCreds {
            on_disk: creds.clone(),
            pi_key,
            long_term_priv,
            pi_auth_token: token,
        })
    }
}

fn decode_b64_32(s: &str) -> Option<[u8; 32]> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    let bytes = B64.decode(s).ok()?;
    bytes.try_into().ok()
}
