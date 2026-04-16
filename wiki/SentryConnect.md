# Sentry Connect (iOS App)

Sentry Connect is a native iOS/iPadOS companion app for SentryUSB. It lets you manage your Tesla dashcam Pi directly from your iPhone or iPad — view status, browse clips, tail logs, and receive native push notifications with Live Activity progress on your lock screen.

## Features

| Feature | Description |
|---------|-------------|
| **Dashboard** | Real-time Pi status — CPU temp, storage, WiFi signal, drive info, archive progress |
| **Camera Viewer** | Multi-camera synchronized playback with 5 layout options |
| **File Browser** | Browse, download, upload, and manage files on the Pi |
| **Live Logs** | Tail system and archive logs in real time |
| **Drive Stats** | View drive history, distance traveled, and FSD statistics |
| **Settings** | Full configuration editor, system actions, update checks |
| **Support Chat** | In-app support ticket system |
| **Setup Wizard** | Run the 9-step setup wizard from your phone (requires WiFi connection to Pi) |

## Connectivity

Sentry Connect supports two connection methods:

### WiFi (Full Features)

When your iPhone or iPad is on the same network as the Pi (home WiFi or the Pi's WiFi Access Point), you get full access to all features.

The app automatically discovers your Pi on the local network — no IP address configuration needed.

### Bluetooth LE (On-the-Go)

When WiFi isn't available (e.g., walking up to your car in a parking lot), the app connects over Bluetooth Low Energy. BLE provides access to:

- Dashboard (status, temperatures, storage)
- Logs
- Settings
- Drive stats

BLE does **not** support video playback or file transfers due to bandwidth limitations.

**BLE is also used for initial WiFi setup** — you can configure your Pi's WiFi credentials via BLE before the Pi has ever connected to a network.

### BLE PIN Authentication

The first time you connect over BLE, the Pi displays a 6-digit PIN on the web UI (or via the LED if no display is connected). Enter this PIN in the app to authenticate. The PIN is saved for future connections.

---

## Push Notifications

Sentry Connect receives native iOS push notifications from your Pi — archive complete, errors, and other events appear as standard iPhone notifications.

### How It Works

1. The Pi sends notification events to a relay server
2. The relay server forwards them as Apple Push Notifications (APNS) to your device
3. No user accounts or sign-ups required — pairing is done with a one-time 6-character code

### Pairing

**Automatic (recommended)** — when connected to your Pi over WiFi:
1. Open Sentry Connect → **Settings** → **Pair for Notifications**
2. Tap **Pair Automatically**
3. The app generates a code on the Pi and completes the pairing in one step

**Manual** — when not connected to the Pi:
1. Open the SentryUSB web interface → **Settings** → **Mobile Notifications** → **Generate Pairing Code**
2. A 6-character code is displayed (expires after 5 minutes)
3. In Sentry Connect → **Settings** → **Pair for Notifications**, enter the code

You can pair multiple devices to the same Pi, and one device can be paired to multiple Pis.

### Managing Paired Devices

- **From the app**: Settings → Paired Devices — view and remove pairings
- **From the Pi web UI**: Settings → Mobile Notifications — view paired devices and remove them

---

## Live Activities

When an archive operation is running, Sentry Connect shows real-time progress on your **Lock Screen** and **Dynamic Island** (iPhone 14 Pro and later).

### What You See

- Current phase (Archiving, Processing, Complete, Error)
- File progress (e.g., "42 / 128")
- Estimated time remaining
- Progress bar
- Device name

### Push-to-Start

The Pi can **start a Live Activity remotely** even when the app isn't open. This means you'll see archive progress on your lock screen automatically — no need to open the app first.

### Dynamic Island

On supported iPhones, the Live Activity appears in the Dynamic Island in three forms:
- **Expanded** — full progress view with phase, device name, file count, ETA, and progress bar
- **Compact** — phase icon + ETA or file count
- **Minimal** — phase icon only

---

## Requirements

- iPhone or iPad with iOS/iPadOS 17.2 or later
- Dynamic Island requires iPhone 14 Pro or later

## Getting the App

[Download Sentry Connect on the App Store](https://apps.apple.com/app/sentry-connect/id6759679030)
