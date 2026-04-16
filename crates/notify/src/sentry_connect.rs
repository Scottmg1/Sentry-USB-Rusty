//! Sentry Connect mobile app push notifications.

use anyhow::{bail, Result};
use reqwest::Client;

const PUSH_SERVER: &str = "https://notifications.sentry-six.com/send";

pub async fn send(client: &Client, device_id: &str, device_secret: &str, title: &str, message: &str) -> Result<()> {
    if device_id.is_empty() || device_secret.is_empty() {
        bail!("Mobile push credentials not found. Re-pair your device in Settings.");
    }

    let resp = client
        .post(PUSH_SERVER)
        .header("Content-Type", "application/json")
        .header("X-Device-Secret", device_secret)
        .json(&serde_json::json!({
            "title": title,
            "message": message,
            "device_id": device_id,
        }))
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("HTTP {} — {}", status, body);
    }
    Ok(())
}
