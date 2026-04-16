use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, webhook_url: &str, title: &str, message: &str) -> Result<()> {
    let resp = client
        .post(webhook_url)
        .json(&serde_json::json!({
            "username": title,
            "content": message,
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
