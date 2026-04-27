//! HTTP client wrapper for cloud-bound requests.
//!
//! Owns a `reqwest::Client` with rustls-tls + sane defaults. Adds the
//! `Authorization: Bearer <piAuthToken>` header to every authenticated
//! request and converts cloud responses into typed Rust enums so the
//! caller doesn't repeat status-code switching.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// HTTP errors surfaced to the caller.
#[derive(Debug, Error)]
pub enum CloudError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("cloud rejected with HTTP {status}: {body}")]
    Http { status: u16, body: String },

    /// 401, or 403 with an `error: "revoked"` body. The Pi has been
    /// removed from `/settings → Devices` and its bearer token is dead.
    /// Caller wipes credentials and transitions to unpaired.
    #[error("auth rejected; pi credentials should be wiped")]
    AuthRejected,

    /// 403 with an `error: "user_suspended"` body. The account is
    /// suspended but the Pi pairing is still valid. Caller surfaces
    /// this as an error state without unpairing — uploads resume
    /// automatically once the suspension is lifted.
    #[error("user suspended; uploads paused until reinstated")]
    UserSuspended,

    #[error("pi key stale; rekey required before upload retry")]
    PiKeyStale,

    #[error("response parse: {0}")]
    Parse(#[from] serde_json::Error),
}

/// A wrapped `reqwest::Client` configured for cloud calls.
pub struct CloudClient {
    inner: reqwest::Client,
    base_url: String,
    bearer: Option<String>,
}

impl CloudClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let inner = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .expect("reqwest client");
        CloudClient {
            inner,
            base_url: base_url.into(),
            bearer: None,
        }
    }

    /// Set the bearer token (raw 32 random bytes, base64-encoded).
    pub fn with_bearer(mut self, token_bytes: &[u8]) -> Self {
        self.bearer = Some(B64.encode(token_bytes));
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

    /// POST a JSON body, no auth (pairing handshake).
    pub async fn post_json_anon(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response, CloudError> {
        let resp = self
            .inner
            .post(self.url(path))
            .json(body)
            .send()
            .await?;
        Ok(resp)
    }

    /// GET, no auth, with a custom header (used by `/pair/poll` which
    /// authenticates via `X-Pairing-Code`).
    pub async fn get_with_header(
        &self,
        path: &str,
        header: (&str, &str),
    ) -> Result<reqwest::Response, CloudError> {
        let resp = self
            .inner
            .get(self.url(path))
            .header(header.0, header.1)
            .send()
            .await?;
        Ok(resp)
    }

    /// GET with bearer auth.
    pub async fn get_bearer(&self, path: &str) -> Result<reqwest::Response, CloudError> {
        let bearer = self
            .bearer
            .as_deref()
            .ok_or_else(|| CloudError::Http { status: 0, body: "no bearer".into() })?;
        let resp = self
            .inner
            .get(self.url(path))
            .header("Authorization", format!("Bearer {}", bearer))
            .send()
            .await?;
        Ok(resp)
    }

    /// POST JSON with bearer auth.
    pub async fn post_json_bearer(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response, CloudError> {
        let bearer = self
            .bearer
            .as_deref()
            .ok_or_else(|| CloudError::Http { status: 0, body: "no bearer".into() })?;
        let resp = self
            .inner
            .post(self.url(path))
            .header("Authorization", format!("Bearer {}", bearer))
            .json(body)
            .send()
            .await?;
        Ok(resp)
    }

    /// Helper: collapse common error statuses into typed errors so call
    /// sites can branch on them without re-parsing.
    pub async fn classify(resp: reqwest::Response) -> Result<reqwest::Response, CloudError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        // Read the body for diagnostics. Best-effort JSON parse so we can
        // catch typed error codes.
        let body_text = resp.text().await.unwrap_or_default();
        let body_json: Option<Value> = serde_json::from_str(&body_text).ok();

        // 401: token is invalid or unknown. Always wipe — there's no
        // valid bearer state to recover.
        if status.as_u16() == 401 {
            return Err(CloudError::AuthRejected);
        }
        // 403: split on the typed body. `revoked` is true revocation
        // (wipe). `user_suspended` is recoverable (don't wipe). Any
        // other 403 falls through to the generic Http variant so the
        // caller can decide.
        if status.as_u16() == 403 {
            let err_field = body_json
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|e| e.as_str());
            match err_field {
                Some("user_suspended") => return Err(CloudError::UserSuspended),
                Some("revoked") | None => return Err(CloudError::AuthRejected),
                Some(_other) => {
                    // Unknown 403 reason — treat as revocation rather
                    // than risk a tight loop.
                    return Err(CloudError::AuthRejected);
                }
            }
        }
        // 409 with `pi_key_stale` is the rekey trigger.
        if status.as_u16() == 409
            && body_json
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|e| e.as_str())
                == Some("pi_key_stale")
        {
            return Err(CloudError::PiKeyStale);
        }

        warn!("cloud rejected HTTP {} body={}", status, body_text);
        Err(CloudError::Http {
            status: status.as_u16(),
            body: body_text,
        })
    }
}
