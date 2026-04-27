

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Deserialize;
use tracing::info;

use sentryusb_cloud_crypto::{aad, aead, credentials, ids, kdf};

use crate::client::CloudClient;
use crate::credentials_store::UnlockedCreds;
use crate::state::CloudStateInner;

const HKDF_INFO_REKEY_PREFIX: &[u8] = b"rekey-kek";
const HKDF_SALT_REKEY: &[u8] = b"sentrycloud-rekey-v1";

pub async fn poll_and_apply(state: Arc<CloudStateInner>) -> Result<bool> {
    let creds_snapshot = {
        let g = state.creds.lock().await;
        match g.as_ref() {
            Some(c) => c.clone(),
            None => return Err(anyhow!("not paired")),
        }
    };

    let unlocked = unlock_or_err(&creds_snapshot)?;

    let client =
        CloudClient::new(&creds_snapshot.cloud_base_url).with_bearer(&unlocked.pi_auth_token);
    let resp = client
        .get_bearer("/api/pi/rekey/poll")
        .await
        .map_err(|e| anyhow!("rekey poll: {}", e))?;
    let status = resp.status();

    match status.as_u16() {
        204 => {

            return Ok(false);
        }
        202 => {

            return Ok(false);
        }
        409 => {

            return Ok(false);
        }
        401 => {

            state.handle_remote_revoke().await;
            return Err(anyhow!("auth rejected during rekey poll"));
        }
        403 => {

            let body_text = resp.text().await.unwrap_or_default();
            let err_field = serde_json::from_str::<serde_json::Value>(&body_text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()));
            if err_field.as_deref() == Some("user_suspended") {
                *state.last_upload_error.lock().await =
                    Some("user_suspended".to_string());
                return Err(anyhow!("user_suspended during rekey poll"));
            }
            state.handle_remote_revoke().await;
            return Err(anyhow!("auth rejected during rekey poll"));
        }
        200 => {}
        s => return Err(anyhow!("rekey poll: unexpected HTTP {}", s)),
    }

    let parsed: PollOk = resp.json().await.context("rekey poll: parse 200 body")?;

    let browser_eph_pub = decode_b64_32(&parsed.browser_eph_pub)
        .ok_or_else(|| anyhow!("bad browserEphPub"))?;
    let wrapped_new = B64
        .decode(&parsed.wrapped_new_pi_key)
        .map_err(|_| anyhow!("bad wrappedNewPiKey base64"))?;

    let shared = unlocked.long_term_priv.compute_shared(&browser_eph_pub);

    let info = rekey_kek_info(
        &creds_snapshot.user_id,
        &creds_snapshot.pi_id,
        parsed.new_generation,
    );
    let kek_bytes = kdf::derive_32(&shared, HKDF_SALT_REKEY, &info)
        .map_err(|e| anyhow!("rekey HKDF: {}", e))?;
    let kek = aead::Key::from_bytes(&kek_bytes).map_err(|e| anyhow!("rekey kek: {}", e))?;
    let aad_bytes = aad::rekey(
        &creds_snapshot.user_id,
        &creds_snapshot.pi_id,
        parsed.new_generation,
    );
    let new_pi_key_bytes = aead::open(&kek, &aad_bytes, &wrapped_new)
        .map_err(|e| anyhow!("decrypt wrappedNewPiKey: {}", e))?;
    let new_pi_key: [u8; 32] = new_pi_key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("new pi key wrong length"))?;

    let serial = ids::read_serial_number(ids::SERIAL_PATH)
        .or_else(|_| {
            std::env::var("SENTRYCLOUD_DEV_SERIAL")
                .map(|s| s.into_bytes())
                .map_err(|_| anyhow!("SBC serial-number missing and SENTRYCLOUD_DEV_SERIAL unset"))
        })?;
    let local_wrap_key =
        ids::derive_pi_local_wrap_key(&serial).map_err(|e| anyhow!("local wrap key: {}", e))?;
    let new_wrapped_b64 = credentials::wrap_pi_key_local(
        &local_wrap_key,
        &new_pi_key,
        &creds_snapshot.pi_id,
    )
    .map_err(|e| anyhow!("rewrap pi key: {}", e))?;

    let mut updated = creds_snapshot.clone();
    updated.wrapped_pi_key_local = new_wrapped_b64;
    updated.dek_rotation_generation = parsed.new_generation;
    state.set_credentials(updated).await?;

    info!(
        "cloud rekey applied: piId={} newGeneration={}",
        creds_snapshot.pi_id, parsed.new_generation
    );
    Ok(true)
}

fn unlock_or_err(c: &credentials::CloudCredentialsV1) -> Result<UnlockedCreds> {
    UnlockedCreds::unlock(c).or_else(|_| {

        let serial = std::env::var("SENTRYCLOUD_DEV_SERIAL")
            .map(|s| s.into_bytes())
            .map_err(|_| anyhow!("unlock failed and SENTRYCLOUD_DEV_SERIAL unset"))?;
        UnlockedCreds::unlock_with_serial(c, &serial)
    })
}

fn decode_b64_32(s: &str) -> Option<[u8; 32]> {
    let bytes = B64.decode(s).ok()?;
    bytes.try_into().ok()
}

fn rekey_kek_info(user_id: &str, pi_id: &str, new_generation: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        HKDF_INFO_REKEY_PREFIX.len() + user_id.len() + pi_id.len() + 4,
    );
    out.extend_from_slice(HKDF_INFO_REKEY_PREFIX);
    out.extend_from_slice(user_id.as_bytes());
    out.extend_from_slice(pi_id.as_bytes());
    out.extend_from_slice(&new_generation.to_be_bytes());
    out
}

#[derive(Deserialize)]
struct PollOk {
    #[serde(rename = "browserEphPub")]
    browser_eph_pub: String,
    #[serde(rename = "wrappedNewPiKey")]
    wrapped_new_pi_key: String,
    #[serde(rename = "newGeneration")]
    new_generation: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rekey_kek_info_layout() {
        let info = rekey_kek_info("u", "p", 0x12345678);
        let mut expected = Vec::new();
        expected.extend_from_slice(b"rekey-kek");
        expected.extend_from_slice(b"u");
        expected.extend_from_slice(b"p");
        expected.extend_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(info, expected);
    }
}
