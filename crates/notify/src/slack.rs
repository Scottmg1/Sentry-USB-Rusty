use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, webhook_url: &str, title: &str, message: &str) -> Result<()> {
    let payload = serde_json::json!({
        "text": format!("{}: {}", title, message),
        "username": "Tesla",
        "icon_emoji": ":tesla:",
    });

    let resp = client
        .post(webhook_url)
        .form(&[("payload", serde_json::to_string(&payload)?)])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("HTTP {} — {}", status, body);
    }
    Ok(())
}
