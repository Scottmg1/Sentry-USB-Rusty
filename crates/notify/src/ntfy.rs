use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(client: &Client, url: &str, token: &str, priority: &str, title: &str, message: &str) -> Result<()> {
    let mut req = client
        .post(url)
        .header("Title", title)
        .header("Priority", if priority.is_empty() { "3" } else { priority })
        .body(message.to_string());

    if !token.is_empty() {
        req = req.header("Authorization", format!("Bearer {}", token));
    }

    let resp = req.send().await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("HTTP {} — {}", status, body);
    }
    Ok(())
}
