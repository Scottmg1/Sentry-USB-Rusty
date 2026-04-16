//! Native notification providers for SentryUSB.
//!
//! Replaces the bash `send-push-message` script with direct HTTP calls,
//! eliminating subprocess overhead and Python/curl dependencies.

use anyhow::Result;
use async_trait::async_trait;
use tracing::{info, warn};

pub mod discord;
pub mod gotify;
pub mod ifttt;
pub mod matrix;
pub mod ntfy;
pub mod pushover;
pub mod sentry_connect;
pub mod signal;
pub mod slack;
pub mod sns;
pub mod telegram;
pub mod webhook;

/// Trait for notification providers.
#[async_trait]
pub trait NotificationProvider: Send + Sync {
    /// Send a notification with the given title and message.
    async fn send(&self, title: &str, message: &str) -> Result<()>;
    /// Provider name for logging/display.
    fn name(&self) -> &str;
}

/// Configuration for all notification providers, read from sentryusb.conf.
pub struct NotifyConfig {
    pub pushover_enabled: bool,
    pub pushover_app_key: String,
    pub pushover_user_key: String,

    pub discord_enabled: bool,
    pub discord_webhook_url: String,

    pub telegram_enabled: bool,
    pub telegram_bot_token: String,
    pub telegram_chat_id: String,
    pub telegram_silent: bool,

    pub slack_enabled: bool,
    pub slack_webhook_url: String,

    pub gotify_enabled: bool,
    pub gotify_domain: String,
    pub gotify_app_token: String,
    pub gotify_priority: String,

    pub ntfy_enabled: bool,
    pub ntfy_url: String,
    pub ntfy_token: String,
    pub ntfy_priority: String,

    pub ifttt_enabled: bool,
    pub ifttt_event_name: String,
    pub ifttt_key: String,

    pub webhook_enabled: bool,
    pub webhook_url: String,

    pub signal_enabled: bool,
    pub signal_url: String,
    pub signal_from_num: String,
    pub signal_to_num: String,

    pub matrix_enabled: bool,
    pub matrix_server_url: String,
    pub matrix_username: String,
    pub matrix_password: String,
    pub matrix_room: String,

    pub sns_enabled: bool,
    pub sns_topic_arn: String,
    pub sns_region: String,

    pub mobile_push_enabled: bool,
    pub mobile_push_device_id: String,
    pub mobile_push_secret: String,
}

impl NotifyConfig {
    /// Load notification config from sentryusb.conf.
    pub fn from_config() -> Self {
        let config_path = sentryusb_config::find_config_path();
        let (active, _) = sentryusb_config::parse_file(config_path)
            .unwrap_or_default();

        let get = |key: &str| -> String {
            active.get(key).cloned().unwrap_or_default()
        };
        let is_true = |key: &str| -> bool {
            get(key).to_lowercase() == "true"
        };

        NotifyConfig {
            pushover_enabled: is_true("PUSHOVER_ENABLED"),
            pushover_app_key: get("PUSHOVER_APP_KEY"),
            pushover_user_key: get("PUSHOVER_USER_KEY"),

            discord_enabled: is_true("DISCORD_ENABLED"),
            discord_webhook_url: get("DISCORD_WEBHOOK_URL"),

            telegram_enabled: is_true("TELEGRAM_ENABLED"),
            telegram_bot_token: get("TELEGRAM_BOT_TOKEN"),
            telegram_chat_id: get("TELEGRAM_CHAT_ID"),
            telegram_silent: get("TELEGRAM_SILENT_NOTIFY").to_lowercase() == "true",

            slack_enabled: is_true("SLACK_ENABLED"),
            slack_webhook_url: get("SLACK_WEBHOOK_URL"),

            gotify_enabled: is_true("GOTIFY_ENABLED"),
            gotify_domain: get("GOTIFY_DOMAIN"),
            gotify_app_token: get("GOTIFY_APP_TOKEN"),
            gotify_priority: get("GOTIFY_PRIORITY"),

            ntfy_enabled: is_true("NTFY_ENABLED"),
            ntfy_url: get("NTFY_URL"),
            ntfy_token: get("NTFY_TOKEN"),
            ntfy_priority: get("NTFY_PRIORITY"),

            ifttt_enabled: is_true("IFTTT_ENABLED"),
            ifttt_event_name: get("IFTTT_EVENT_NAME"),
            ifttt_key: get("IFTTT_KEY"),

            webhook_enabled: is_true("WEBHOOK_ENABLED"),
            webhook_url: get("WEBHOOK_URL"),

            signal_enabled: is_true("SIGNAL_ENABLED"),
            signal_url: get("SIGNAL_URL"),
            signal_from_num: get("SIGNAL_FROM_NUM"),
            signal_to_num: get("SIGNAL_TO_NUM"),

            matrix_enabled: is_true("MATRIX_ENABLED"),
            matrix_server_url: get("MATRIX_SERVER_URL"),
            matrix_username: get("MATRIX_USERNAME"),
            matrix_password: get("MATRIX_PASSWORD"),
            matrix_room: get("MATRIX_ROOM"),

            sns_enabled: is_true("SNS_ENABLED"),
            sns_topic_arn: get("AWS_SNS_TOPIC_ARN"),
            sns_region: get("AWS_REGION"),

            mobile_push_enabled: is_true("MOBILE_PUSH_ENABLED"),
            mobile_push_device_id: get("MOBILE_PUSH_DEVICE_ID"),
            mobile_push_secret: get("MOBILE_PUSH_SECRET"),
        }
    }
}

/// Send a notification to all enabled providers.
/// Returns the list of provider names that were attempted and their results.
pub async fn send_to_all(
    config: &NotifyConfig,
    title: &str,
    message: &str,
) -> Vec<(String, Result<()>)> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();

    let mut results = Vec::new();

    if config.pushover_enabled {
        let r = pushover::send(&client, &config.pushover_app_key, &config.pushover_user_key, title, message).await;
        log_result("Pushover", &r);
        results.push(("pushover".to_string(), r));
    }

    if config.discord_enabled {
        let r = discord::send(&client, &config.discord_webhook_url, title, message).await;
        log_result("Discord", &r);
        results.push(("discord".to_string(), r));
    }

    if config.telegram_enabled {
        let r = telegram::send(&client, &config.telegram_bot_token, &config.telegram_chat_id, title, message, config.telegram_silent).await;
        log_result("Telegram", &r);
        results.push(("telegram".to_string(), r));
    }

    if config.slack_enabled {
        let r = slack::send(&client, &config.slack_webhook_url, title, message).await;
        log_result("Slack", &r);
        results.push(("slack".to_string(), r));
    }

    if config.gotify_enabled {
        let r = gotify::send(&client, &config.gotify_domain, &config.gotify_app_token, &config.gotify_priority, title, message).await;
        log_result("Gotify", &r);
        results.push(("gotify".to_string(), r));
    }

    if config.ntfy_enabled {
        let r = ntfy::send(&client, &config.ntfy_url, &config.ntfy_token, &config.ntfy_priority, title, message).await;
        log_result("ntfy", &r);
        results.push(("ntfy".to_string(), r));
    }

    if config.ifttt_enabled {
        let r = ifttt::send(&client, &config.ifttt_event_name, &config.ifttt_key, title, message).await;
        log_result("IFTTT", &r);
        results.push(("ifttt".to_string(), r));
    }

    if config.webhook_enabled {
        let r = webhook::send(&client, &config.webhook_url, title, message).await;
        log_result("Webhook", &r);
        results.push(("webhook".to_string(), r));
    }

    if config.signal_enabled {
        let r = signal::send(&client, &config.signal_url, &config.signal_from_num, &config.signal_to_num, message).await;
        log_result("Signal", &r);
        results.push(("signal".to_string(), r));
    }

    if config.matrix_enabled {
        let r = matrix::send(&client, &config.matrix_server_url, &config.matrix_username, &config.matrix_password, &config.matrix_room, title, message).await;
        log_result("Matrix", &r);
        results.push(("matrix".to_string(), r));
    }

    if config.mobile_push_enabled {
        let r = sentry_connect::send(&client, &config.mobile_push_device_id, &config.mobile_push_secret, title, message).await;
        log_result("Mobile Push", &r);
        results.push(("mobile_push".to_string(), r));
    }

    if config.sns_enabled {
        let r = sns::send(&config.sns_topic_arn, title, message).await;
        log_result("SNS", &r);
        results.push(("sns".to_string(), r));
    }

    results
}

fn log_result(provider: &str, result: &Result<()>) {
    match result {
        Ok(()) => info!("[notify] {} — sent successfully", provider),
        Err(e) => warn!("[notify] {} — failed: {}", provider, e),
    }
}
