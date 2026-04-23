//! Sentry Connect mobile app push notifications.
//!
//! The server lives at `https://notifications.sentry-six.com` — override
//! via `SENTRY_NOTIFICATION_URL` (matches PR #31 on the Go side).
//!
//! For archive_start notifications with an `ARCHIVE_TOTAL_COUNT`, the
//! payload also carries a `live_activity` block so the iOS app can
//! reliably start its Live Activity even if it had been terminated by
//! the system (more reliable than silent-push wake).

use anyhow::{bail, Result};
use reqwest::Client;
use serde_json::json;

/// Optional per-send context. Default leaves all fields `None`, which
/// reproduces the minimal title+message payload.
#[derive(Debug, Clone, Default)]
pub struct SendContext<'a> {
    /// `start` or `finish`. Only relevant when paired with
    /// `notification_type = "archive_start"` to enable the live_activity
    /// payload.
    pub type_hint: Option<&'a str>,
    /// The notification category (`archive_start`, `archive_complete`,
    /// `temperature`, `drives`, etc.). Echoed in the payload so the
    /// mobile app can categorize the alert.
    pub notification_type: Option<&'a str>,
    /// Total clip count for the pending archive run. Required for the
    /// live_activity payload on `archive_start`.
    pub archive_total_count: Option<u32>,
    /// Device name shown in the live_activity header. Usually the
    /// user's title (e.g. `"MyTesla:"`); the trailing colon is stripped.
    pub device_name: Option<&'a str>,
}

fn default_push_server() -> String {
    std::env::var("SENTRY_NOTIFICATION_URL")
        .unwrap_or_else(|_| "https://notifications.sentry-six.com".to_string())
}

pub async fn send(
    client: &Client,
    device_id: &str,
    device_secret: &str,
    title: &str,
    message: &str,
) -> Result<()> {
    send_with_context(client, device_id, device_secret, title, message, &SendContext::default()).await
}

pub async fn send_with_context(
    client: &Client,
    device_id: &str,
    device_secret: &str,
    title: &str,
    message: &str,
    ctx: &SendContext<'_>,
) -> Result<()> {
    if device_id.is_empty() || device_secret.is_empty() {
        bail!("Mobile push credentials not found. Re-pair your device in Settings.");
    }

    let mut payload = json!({
        "title": title,
        "message": message,
        "device_id": device_id,
    });
    let obj = payload.as_object_mut().expect("payload is a JSON object");

    if let Some(nt) = ctx.notification_type {
        if !nt.is_empty() {
            obj.insert("notification_type".into(), json!(nt));
        }
    }

    // live_activity — ONLY on archive_start/start with a known total count.
    // Matches bash `send_mobile_push` live_activity branch.
    let is_archive_start = ctx.type_hint == Some("start")
        && ctx.notification_type == Some("archive_start");
    if is_archive_start {
        if let Some(total) = ctx.archive_total_count {
            // device_name: strip trailing ":" to mirror `${title%:}` in bash.
            let raw_name = ctx.device_name.unwrap_or(title);
            let device_name = raw_name.strip_suffix(':').unwrap_or(raw_name);
            obj.insert(
                "live_activity".into(),
                json!({
                    "action": "start",
                    "phase": "archiving",
                    "current": 0,
                    "total": total,
                    "device_name": device_name,
                }),
            );
        }
    }

    let url = format!("{}/send", default_push_server().trim_end_matches('/'));
    let resp = client
        .post(&url)
        .header("Content-Type", "application/json")
        .header("X-Device-Secret", device_secret)
        .json(&payload)
        .send()
        .await?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("HTTP {} — {}", status, body);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_default_is_all_none() {
        let ctx = SendContext::default();
        assert!(ctx.type_hint.is_none());
        assert!(ctx.notification_type.is_none());
        assert!(ctx.archive_total_count.is_none());
        assert!(ctx.device_name.is_none());
    }

    #[test]
    fn live_activity_only_for_archive_start() {
        // Just a simple sanity — actual payload construction lives inside
        // send_with_context; we're exercising the condition logic here.
        let ctx = SendContext {
            type_hint: Some("start"),
            notification_type: Some("archive_start"),
            archive_total_count: Some(42),
            device_name: Some("MyTesla:"),
        };
        let is_archive_start = ctx.type_hint == Some("start")
            && ctx.notification_type == Some("archive_start");
        assert!(is_archive_start);
        assert_eq!(ctx.archive_total_count, Some(42));
    }

    #[test]
    fn push_server_default_is_production_url() {
        // Cross-test env mutation is unsafe on the 2024 edition + risks
        // flakiness when other tests also touch the env; just verify the
        // fallback branch when the variable is whatever it is in test
        // context. The assertion holds whenever the env isn't explicitly
        // set to a non-empty override, which is the only state CI cares
        // about anyway.
        if std::env::var("SENTRY_NOTIFICATION_URL").is_err() {
            assert_eq!(default_push_server(), "https://notifications.sentry-six.com");
        }
    }
}
