# Notifications

SentryUSB can send push notifications when archiving completes, errors occur, or other important events happen. You can enable any combination of the 11 supported providers.

## Configuring Notifications

The easiest way is through the **Setup Wizard** → **Notifications** step:

1. Open **http://sentryusb.local**
2. Go to **Settings** → **Open Wizard** → navigate to **Notifications**
3. Enable one or more providers and fill in the required fields
4. Optionally set a **Notification Title** (defaults to "SentryUSB")

---

## Providers

### Pushover

Free for up to 7,500 messages/month. iOS/Android apps have a one-time cost after a free trial.

1. Create an account at [pushover.net](https://pushover.net)
2. Install the Pushover app on your phone
3. Copy your **User Key** from the Pushover dashboard
4. [Create a new Application](https://pushover.net/apps/build) and copy the **Application Key**

| Field | What to Enter |
|-------|---------------|
| **User Key** | Your Pushover user key |
| **App Key** | Your application's API key |

### Gotify

Self-hosted notification service. Android client on [Google Play](https://play.google.com/store/apps/details?id=com.github.gotify), [F-Droid](https://f-droid.org/de/packages/com.github.gotify/), or [APK](https://github.com/gotify/android/releases/latest).

1. Install a Gotify server ([instructions](https://gotify.net/docs/install))
2. [Create a new Application](https://gotify.net/docs/pushmsg) and copy the token

| Field | What to Enter |
|-------|---------------|
| **Domain** | Your Gotify server URL (e.g., `https://gotify.example.com`) |
| **App Token** | Application token |
| **Priority** | Message priority (default: `5`) |

### Discord

1. Open Discord → Server Settings → **Integrations** → **Webhooks** → **New Webhook**
2. Choose a channel, click **Copy Webhook URL**

| Field | What to Enter |
|-------|---------------|
| **Webhook URL** | The full webhook URL |

### Telegram

Free. Requires the [Telegram app](https://telegram.org/apps).

1. Message [@userinfobot](https://telegram.me/userinfobot) to get your **Chat ID**
2. Create a bot via [@BotFather](https://core.telegram.org/bots#botfather) to get your **Bot Token**
3. Include the `bot` prefix in the token (e.g., `bot123456789:abc...`)

| Field | What to Enter |
|-------|---------------|
| **Chat ID** | Your numeric Telegram user ID |
| **Bot Token** | Bot token including `bot` prefix |

### IFTTT

Requires [IFTTT Pro](https://ifttt.com/plans) for Webhooks.

1. Connect the [Webhooks service](https://ifttt.com/maker_webhooks)
2. Create an applet: **Webhooks** trigger → **Notifications** action
3. Set a unique **Event Name**
4. Go to [Webhooks docs](https://ifttt.com/maker_webhooks) → click "Documentation" → copy your **Key**

| Field | What to Enter |
|-------|---------------|
| **Event Name** | Your applet's event name |
| **Key** | Your Webhooks service key |

### Slack

1. Go to [api.slack.com](https://api.slack.com) → **Create a custom app**
2. Enable **Incoming Webhooks** → **Add Webhook to Workspace**
3. Copy the webhook URL

| Field | What to Enter |
|-------|---------------|
| **Webhook URL** | The full Slack webhook URL |

### Signal

Requires a [signal-cli REST API](https://github.com/bbernhard/signal-cli-rest-api) instance on your network.

| Field | What to Enter |
|-------|---------------|
| **Signal CLI URL** | URL of the REST API (e.g., `http://localhost:8080`) |
| **From Number** | Sender phone number (e.g., `+1234567890`) |
| **To Number** | Recipient phone number |

### Matrix

Federated messaging via [Matrix.org](https://matrix.org) or self-hosted.

1. Create a bot account on your homeserver
2. Create/join a room and find the **Internal Room ID** in room settings

| Field | What to Enter |
|-------|---------------|
| **Server URL** | Homeserver URL (e.g., `https://matrix.org`) |
| **Username** | Bot username |
| **Password** | Bot password |
| **Room ID** | Target room (e.g., `!roomid:matrix.org`) |

### AWS SNS

Amazon Simple Notification Service. [Free tier](https://aws.amazon.com/sns/pricing/) available.

1. Create an [AWS account](https://aws.amazon.com/)
2. Create an IAM user with SNS permissions
3. Create an SNS topic and subscribe an endpoint

| Field | What to Enter |
|-------|---------------|
| **Region** | AWS region (e.g., `us-east-1`) |
| **Access Key ID** | IAM access key |
| **Secret Key** | IAM secret key |
| **Topic ARN** | Full ARN of the SNS topic |

### Webhook

Generic HTTP webhook for Home Assistant, Node-RED, or any endpoint.

| Field | What to Enter |
|-------|---------------|
| **Webhook URL** | Full URL to POST notifications to |

### ntfy

Free, open-source pub-sub notification service. Works with [ntfy.sh](https://ntfy.sh) (hosted) or a self-hosted instance. iOS/Android apps available.

1. Subscribe to a topic at [ntfy.sh](https://ntfy.sh) (e.g., `https://ntfy.sh/your-unique-topic`)
2. Or self-host ntfy and use your own server URL
3. If your topic is access-controlled, generate an **Access Token** in ntfy's account settings

| Field | What to Enter |
|-------|---------------|
| **URL & Topic** | Full URL including topic (e.g., `https://ntfy.sh/yourtopic`) |
| **Access Token** | Optional auth token for protected topics |
| **Priority** | Message priority 1–5 (default: `3`) |

---

## Sentry Connect (iOS Push Notifications)

If you have the **Sentry Connect** iPhone app, you can receive native iOS push notifications — no third-party service setup required. This also enables **Live Activities** with real-time archive progress on your Lock Screen and Dynamic Island.

Pairing is done with a one-time 6-character code. No accounts or API keys needed.

1. Open the SentryUSB web UI → **Settings** → **Mobile Notifications** → **Generate Pairing Code**
2. In the Sentry Connect app → **Settings** → **Pair for Notifications** → enter the code

Or, if your phone is connected to the Pi over WiFi, tap **Pair Automatically** in the app for a one-tap setup.

See [Sentry Connect](SentryConnect) for full details on the iOS app, Live Activities, and BLE connectivity.

---

## Manual Configuration

Notifications can also be configured by editing `/root/sentryusb.conf` via SSH. Set the `*_ENABLED` variable to `true` and provide the required fields:

```bash
export PUSHOVER_ENABLED=true
export PUSHOVER_USER_KEY=your_key
export PUSHOVER_APP_KEY=your_app_key
```

Then run `/root/bin/setup-sentryusb` to apply.

See the [sample config file](https://github.com/Sentry-Six/Sentry-USB-Rusty/blob/main-dev/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb.conf.sample) for all available config variables.