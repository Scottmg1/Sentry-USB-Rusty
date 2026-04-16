use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, event_name: &str, key: &str, title: &str, message: &str) -> Result<()> {
    let url = format!("https://maker.ifttt.com/trigger/{}/with/key/{}", event_name, key);

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "value1": title,
            "value2": message,
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
