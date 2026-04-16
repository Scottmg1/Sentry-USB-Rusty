use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, bot_token: &str, chat_id: &str, title: &str, message: &str, silent: bool) -> Result<()> {
    let url = format!("https://api.telegram.org/{}/sendMessage", bot_token);
    let text = format!("{}: {}", title, message);

    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "chat_id": chat_id,
            "text": text,
            "disable_notification": silent,
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
