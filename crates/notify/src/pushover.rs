use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, app_key: &str, user_key: &str, title: &str, message: &str) -> Result<()> {
    let resp = client
        .post("https://api.pushover.net/1/messages.json")
        .form(&[
            ("token", app_key),
            ("user", user_key),
            ("title", title),
            ("message", message),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("HTTP {} — {}", status, body);
    }
    Ok(())
}
