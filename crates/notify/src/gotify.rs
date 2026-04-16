use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, domain: &str, app_token: &str, priority: &str, title: &str, message: &str) -> Result<()> {
    let url = format!("{}/message?token={}", domain, app_token);
    let priority_num: i32 = priority.parse().unwrap_or(5);

    let resp = client
        .post(&url)
        .form(&[
            ("title", title),
            ("message", message),
            ("priority", &priority_num.to_string()),
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
