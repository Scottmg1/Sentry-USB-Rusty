

use anyhow::{anyhow, Result};

use sentryusb_cloud_crypto::credentials::CloudCredentialsV1;
use sentryusb_cloud_crypto::ids;

pub struct UnlockedCreds {
    pub on_disk: CloudCredentialsV1,
    pub pi_key: [u8; 32],
    pub long_term_priv: sentryusb_cloud_crypto::x25519::LongTermPrivate,
    pub pi_auth_token: [u8; 32],
}

impl UnlockedCreds {

    pub fn unlock(creds: &CloudCredentialsV1) -> Result<Self> {

        let serial = ids::read_serial_number(ids::SERIAL_PATH)
            .map_err(|e| anyhow!("read serial-number: {}", e))?;
        Self::unlock_with_serial(creds, &serial)
    }

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
