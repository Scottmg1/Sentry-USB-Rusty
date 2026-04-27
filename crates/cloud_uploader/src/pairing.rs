//! Pi-side three-party pairing flow (ENCRYPTION.md §8).
//!
//! Drives the sequence:
//!   1. Pi generates `(piEphPriv, piEphPub)` ephemeral X25519.
//!   2. Pi generates `(piLongTermPriv, piLongTermPub)` long-term X25519
//!      for future rekey. The pubkey is sent to cloud at handshake.
//!   3. Pi POSTs `/api/pi/pair/handshake { code, piEphPub, piPubKey,
//!      piMetadata }`. Cloud relays to the user's browser via SSE.
//!   4. Browser computes `wrappedPiKeyForPi`, finalizes.
//!   5. Pi polls `/api/pi/pair/poll` (header `X-Pairing-Code`) until
//!      cloud returns the wrapped envelope + `piAuthToken` + `piId`.
//!   6. Pi decrypts `wrappedPiKeyForPi` with the ECDH-derived KEK.
//!   7. Pi wraps the per-Pi key + long-term privkey under the SBC-serial
//!      local wrap key and writes `cloud-credentials.json` atomically.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tracing::info;

use sentryusb_cloud_crypto::{aad, aead, credentials, ids, kdf, x25519};

use crate::client::CloudClient;
use crate::state::{CloudStateInner, PairingProgress, PairingState};

/// HKDF salt for the pairing transit KEK (matches ENCRYPTION.md §8 + the
/// browser's pairing worker — must agree byte-for-byte).
const HKDF_SALT_PAIR: &[u8] = b"sentrycloud-pair-v1";

/// HKDF info prefix; concatenated with `userId || piId` per ENCRYPTION.md §8.
const HKDF_INFO_PAIR_PREFIX: &[u8] = b"pair-kek";

/// Poll cadence and total budget. The browser is on the human side of the
/// flow — give it time. If the user dawdles past the budget, the pairing
/// code itself expires server-side at 10 min anyway.
const POLL_INTERVAL: Duration = Duration::from_millis(1500);
const POLL_TIMEOUT: Duration = Duration::from_secs(60 * 11);

/// Run the entire pairing flow, owning the state mutations along the way.
pub async fn run(state: Arc<CloudStateInner>, code: String) -> Result<()> {
    // Refuse if already paired.
    {
        let creds = state.creds.lock().await;
        if creds.is_some() {
            return Err(anyhow!("already paired; unpair first"));
        }
    }

    // Cancellation channel for /api/cloud/pair/cancel.
    let cancel = Arc::new(Notify::new());
    {
        let mut g = state.pairing_cancel.lock().await;
        *g = Some(cancel.clone());
    }

    set_state(&state, PairingState::Handshaking, None).await;

    // Step 1: ephemeral keypair. Used for the one-time ECDH at the end.
    let pi_eph = x25519::EphemeralPrivate::generate()
        .map_err(|e| anyhow!("ephemeral keypair: {}", e))?;
    let pi_eph_pub = pi_eph
        .public_bytes()
        .map_err(|e| anyhow!("ephemeral pubkey: {}", e))?;

    // Step 2: long-term keypair. Sent to cloud at handshake; private half
    // gets wrapped under the local key after pairing succeeds.
    let pi_long_term = x25519::LongTermPrivate::generate()
        .map_err(|e| anyhow!("long-term keypair: {}", e))?;
    let pi_long_term_pub = pi_long_term.public_bytes();

    // Step 3: handshake POST.
    let metadata = pi_metadata();
    let body = HandshakeBody {
        code: code.clone(),
        pi_eph_pub: B64.encode(pi_eph_pub),
        pi_pub_key: B64.encode(pi_long_term_pub),
        pi_metadata: metadata,
    };
    let client = CloudClient::new(&state.cloud_base_url);
    let resp = client
        .post_json_anon("/api/pi/pair/handshake", &body)
        .await
        .map_err(|e| anyhow!("handshake POST: {}", e))?;
    let resp = CloudClient::classify(resp).await.map_err(|e| {
        anyhow!("handshake rejected: {}", e)
    })?;
    drop(resp);

    // Step 4-5: poll for the browser's finalize.
    set_state(&state, PairingState::Polling, None).await;
    let started = std::time::Instant::now();
    let mut poll_resp: Option<PollResponse> = None;
    while started.elapsed() < POLL_TIMEOUT {
        // Honor cancel.
        if was_cancelled(&state).await {
            return Err(anyhow!("pairing cancelled"));
        }
        let r = client
            .get_with_header("/api/pi/pair/poll", ("X-Pairing-Code", &code))
            .await
            .map_err(|e| anyhow!("poll GET: {}", e))?;
        match r.status().as_u16() {
            200 => {
                let parsed: PollResponse =
                    r.json().await.map_err(|e| anyhow!("poll parse: {}", e))?;
                poll_resp = Some(parsed);
                break;
            }
            202 => {
                // Browser hasn't finalized yet. Sleep and retry.
                tokio::select! {
                    _ = tokio::time::sleep(POLL_INTERVAL) => {},
                    _ = cancel.notified() => return Err(anyhow!("pairing cancelled")),
                }
            }
            404 => return Err(anyhow!("pairing code invalid or expired")),
            409 => return Err(anyhow!("server lost pairing state; try again")),
            s => return Err(anyhow!("poll: unexpected HTTP {}", s)),
        }
    }
    let poll_resp = poll_resp.ok_or_else(|| anyhow!("pairing timed out waiting for browser"))?;

    // Step 6: decrypt the wrapped per-Pi key.
    let browser_eph_pub = decode_b64_32(&poll_resp.browser_eph_pub)
        .ok_or_else(|| anyhow!("bad browserEphPub"))?;
    let wrapped_pi_key = B64
        .decode(&poll_resp.wrapped_pi_key_for_pi)
        .map_err(|_| anyhow!("bad wrappedPiKeyForPi base64"))?;
    let pi_auth_token = decode_b64_32(&poll_resp.pi_auth_token)
        .ok_or_else(|| anyhow!("bad piAuthToken"))?;

    let shared = pi_eph
        .compute_shared(&browser_eph_pub)
        .map_err(|e| anyhow!("ECDH: {}", e))?;
    let info = pair_kek_info(&poll_resp.user_id, &poll_resp.pi_id);
    let kek_bytes = kdf::derive_32(&shared, HKDF_SALT_PAIR, &info)
        .map_err(|e| anyhow!("HKDF kek: {}", e))?;
    let kek = aead::Key::from_bytes(&kek_bytes).map_err(|e| anyhow!("kek key: {}", e))?;
    let pair_aad = aad::pair(&poll_resp.user_id, &poll_resp.pi_id);
    let pi_key_bytes = aead::open(&kek, &pair_aad, &wrapped_pi_key)
        .map_err(|e| anyhow!("decrypt wrappedPiKeyForPi: {}", e))?;
    let pi_key: [u8; 32] = pi_key_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("pi_key wrong length"))?;

    // Step 7: derive local wrap key + persist credentials.
    let serial = ids::read_serial_number(ids::SERIAL_PATH)
        .or_else(|_| {
            // Dev fallback: tests + local development on non-Pi hardware.
            std::env::var("SENTRYCLOUD_DEV_SERIAL")
                .map(|s| s.into_bytes())
                .map_err(|_| anyhow!("SBC serial-number missing and SENTRYCLOUD_DEV_SERIAL unset"))
        })?;
    let local_wrap_key = ids::derive_pi_local_wrap_key(&serial)
        .map_err(|e| anyhow!("local wrap key: {}", e))?;

    let creds = credentials::build_v1(
        poll_resp.user_id.clone(),
        poll_resp.pi_id.clone(),
        &pi_auth_token,
        &pi_key,
        &pi_long_term,
        &local_wrap_key,
        poll_resp.cloud_base_url.clone(),
        Utc::now(),
        0,
    )
    .map_err(|e| anyhow!("build credentials: {}", e))?;

    state
        .set_credentials(creds)
        .await
        .context("set credentials")?;

    // Boot a one-shot upload sweep (kick the Notify) so any routes that
    // already exist on disk start landing in the cloud immediately.
    state.notify.notify_one();

    set_state(&state, PairingState::Complete, None).await;
    info!(
        "cloud pairing complete: piId={} userId={}",
        poll_resp.pi_id, poll_resp.user_id
    );
    Ok(())
}

async fn set_state(state: &CloudStateInner, st: PairingState, err: Option<String>) {
    let mut p = state.pairing.lock().await;
    *p = PairingProgress { state: st, error: err };
}

async fn was_cancelled(state: &CloudStateInner) -> bool {
    let p = state.pairing.lock().await;
    matches!(p.state, PairingState::Idle if p.error.as_deref() == Some("cancelled"))
}

#[derive(Serialize)]
struct HandshakeBody {
    code: String,
    #[serde(rename = "piEphPub")]
    pi_eph_pub: String,
    #[serde(rename = "piPubKey")]
    pi_pub_key: String,
    #[serde(rename = "piMetadata")]
    pi_metadata: serde_json::Value,
}

#[derive(Deserialize)]
struct PollResponse {
    #[serde(rename = "userId")]
    user_id: String,
    #[serde(rename = "piId")]
    pi_id: String,
    #[serde(rename = "browserEphPub")]
    browser_eph_pub: String,
    #[serde(rename = "wrappedPiKeyForPi")]
    wrapped_pi_key_for_pi: String,
    #[serde(rename = "piAuthToken")]
    pi_auth_token: String,
    #[serde(rename = "cloudBaseUrl")]
    cloud_base_url: String,
}

fn decode_b64_32(s: &str) -> Option<[u8; 32]> {
    let bytes = B64.decode(s).ok()?;
    bytes.try_into().ok()
}

/// HKDF info for the pairing KEK. Matches the browser's
/// `info="pair-kek" || userId || piId` byte construction.
fn pair_kek_info(user_id: &str, pi_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(HKDF_INFO_PAIR_PREFIX.len() + user_id.len() + pi_id.len());
    out.extend_from_slice(HKDF_INFO_PAIR_PREFIX);
    out.extend_from_slice(user_id.as_bytes());
    out.extend_from_slice(pi_id.as_bytes());
    out
}

/// Pi metadata sent at pairing. Bounded < 4 KiB per server validation.
/// Truncated MAC (last 4 hex chars) so we don't fingerprint the device.
fn pi_metadata() -> serde_json::Value {
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| "sentryusb".to_string());

    let kernel = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    let sentryusb_version = env!("CARGO_PKG_VERSION").to_string();

    let model = std::fs::read_to_string("/sys/firmware/devicetree/base/model")
        .ok()
        .map(|s| s.trim_end_matches('\0').trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    let mac_tail = read_first_mac_tail().unwrap_or_default();

    serde_json::json!({
        "hostname": hostname,
        "sbcModel": model,
        "kernelVersion": kernel,
        "sentryusbVersion": sentryusb_version,
        "macTail": mac_tail,
    })
}

/// Read the last 4 hex chars (lowercase) of the first wired MAC. Best-
/// effort; returns None on dev hosts. Trims the colon separators.
fn read_first_mac_tail() -> Option<String> {
    let entries = std::fs::read_dir("/sys/class/net").ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_s = name.to_string_lossy().to_string();
        if name_s == "lo" || name_s.starts_with("docker") {
            continue;
        }
        let addr_path = entry.path().join("address");
        if let Ok(addr) = std::fs::read_to_string(&addr_path) {
            let trimmed: String = addr.trim().replace(':', "").to_lowercase();
            if trimmed.len() == 12 {
                return Some(trimmed[8..12].to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_kek_info_layout() {
        let info = pair_kek_info("u123", "pi456");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"pair-kek");
        expected.extend_from_slice(b"u123");
        expected.extend_from_slice(b"pi456");
        assert_eq!(info, expected);
    }
}
