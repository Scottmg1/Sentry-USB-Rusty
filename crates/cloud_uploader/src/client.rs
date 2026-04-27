

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;
use tracing::warn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum CloudError {
    #[error("transport: {0}")]
    Transport(#[from] reqwest::Error),

    #[error("cloud rejected with HTTP {status}: {body}")]
    Http { status: u16, body: String },

    #[error("auth rejected; pi credentials should be wiped")]
    AuthRejected,

    #[error("user suspended; uploads paused until reinstated")]
    UserSuspended,

    #[error("pi key stale; rekey required before upload retry")]
    PiKeyStale,

    #[error("response parse: {0}")]
    Parse(#[from] serde_json::Error),
}

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

    pub fn with_bearer(mut self, token_bytes: &[u8]) -> Self {
        self.bearer = Some(B64.encode(token_bytes));
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url.trim_end_matches('/'), path)
    }

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

    pub async fn post_json_bearer(
        &self,
        path: &str,
        body: &impl Serialize,
    ) -> Result<reqwest::Response, CloudError> {
        self.post_json_bearer_with_headers(path, body, &[]).await
    }

    pub async fn post_json_bearer_with_headers(
        &self,
        path: &str,
        body: &impl Serialize,
        extra_headers: &[(&str, String)],
    ) -> Result<reqwest::Response, CloudError> {
        let bearer = self
            .bearer
            .as_deref()
            .ok_or_else(|| CloudError::Http { status: 0, body: "no bearer".into() })?;
        let mut req = self
            .inner
            .post(self.url(path))
            .header("Authorization", format!("Bearer {}", bearer));
        for (k, v) in extra_headers {
            req = req.header(*k, v.clone());
        }
        let resp = req.json(body).send().await?;
        Ok(resp)
    }

    pub async fn classify(resp: reqwest::Response) -> Result<reqwest::Response, CloudError> {
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }

        let body_text = resp.text().await.unwrap_or_default();
        let body_json: Option<Value> = serde_json::from_str(&body_text).ok();

        if status.as_u16() == 401 {
            return Err(CloudError::AuthRejected);
        }

        if status.as_u16() == 403 {
            let err_field = body_json
                .as_ref()
                .and_then(|v| v.get("error"))
                .and_then(|e| e.as_str());
            match err_field {
                Some("user_suspended") => return Err(CloudError::UserSuspended),
                Some("revoked") | None => return Err(CloudError::AuthRejected),
                Some(_other) => {

                    return Err(CloudError::AuthRejected);
                }
            }
        }

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
