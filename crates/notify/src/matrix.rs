//! Matrix notification provider — login, send message, logout.

use anyhow::{bail, Result};
use reqwest::Client;

pub async fn send(
    client: &Client,
    server_url: &str,
    username: &str,
    password: &str,
    room: &str,
    title: &str,
    message: &str,
) -> Result<()> {
    // 1. Login
    let login_url = format!("{}/_matrix/client/v3/login", server_url);
    let login_resp = client
        .post(&login_url)
        .json(&serde_json::json!({
            "type": "m.login.password",
            "user": username,
            "password": password,
        }))
        .send()
        .await?;

    if !login_resp.status().is_success() {
        bail!("Matrix login failed: HTTP {}", login_resp.status());
    }

    let login_data: serde_json::Value = login_resp.json().await?;
    let access_token = login_data["access_token"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no access_token in login response"))?;

    // 2. Send message
    let txn_id = format!("sentryusb_{}", chrono::Utc::now().timestamp_millis());
    let send_url = format!(
        "{}/_matrix/client/v3/rooms/{}/send/m.room.message/{}",
        server_url, room, txn_id
    );

    let text = format!("{}: {}", title, message);
    let send_resp = client
        .put(&send_url)
        .header("Authorization", format!("Bearer {}", access_token))
        .json(&serde_json::json!({
            "msgtype": "m.text",
            "body": text,
        }))
        .send()
        .await?;

    // 3. Logout (best-effort)
    let logout_url = format!("{}/_matrix/client/v3/logout", server_url);
    let _ = client
        .post(&logout_url)
        .header("Authorization", format!("Bearer {}", access_token))
        .send()
        .await;

    if !send_resp.status().is_success() {
        bail!("Matrix send failed: HTTP {}", send_resp.status());
    }
    Ok(())
}
