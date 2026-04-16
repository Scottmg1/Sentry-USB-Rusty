use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, signal_url: &str, from_num: &str, to_num: &str, message: &str) -> Result<()> {
    let url = format!("{}/v2/send", signal_url);

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "message": message,
            "number": from_num,
            "recipients": [to_num],
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
