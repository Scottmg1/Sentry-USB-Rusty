# Frequently Asked Questions

## General

### What is SentryUSB?

SentryUSB turns a Raspberry Pi into a smart USB drive for your Tesla's dashcam system. It automatically archives recordings to a server or cloud storage, serves a modern web UI for configuration and clip viewing, and can be fully managed from your browser.

### How is SentryUSB different from TeslaUSB?

SentryUSB is a modernized fork of [TeslaUSB](https://github.com/marcone/teslausb) with:
- A brand-new **React web UI** with dark glassmorphism design
- A **Go API server** (single binary) replacing nginx + CGI scripts
- A **9-step Setup Wizard** — no SSH or config file editing required
- **Multi-camera viewer** with synchronized playback of all 6 Tesla cameras
- **Drive Tracking** with GPS-extracted route visualization
- Numerous bug fixes

### Can I upgrade from TeslaUSB to SentryUSB?

No — a **fresh install is required**. There is no in-place upgrade path from TeslaUSB. You'll need to re-flash your SD card and reconfigure your settings via the web UI Setup Wizard.

---

## Hardware

### Which Raspberry Pi should I buy?

| Board | Recommendation |
|-------|---------------|
| **Pi 4B / Pi 5** | Best choice — fast, USB 3.0, reliable |
| **Pi 3 (A+/B/B+)** | Mid-range option — 5 GHz WiFi, requires USB-A to USB-A cable |
| **Pi Zero 2 W** | Good budget option with adequate performance |

### How big should my SD card be?

- **128 GB+ recommended** — gives plenty of room for dashcam, music, and snapshots
- **64 GB minimum** — will work but limits dashcam storage
- If using an external USB/NVMe drive, the SD card can be smaller (16 GB+) since it's only used for boot

### Can I use an external SSD instead of the SD card?

Yes. Connect a USB SSD or NVMe drive (in a USB enclosure) to a Pi 4 or Pi 5. The SD card is still required for boot, but all data storage moves to the external drive.

**Setup Wizard**: In the Storage step, click **Refresh** to detect connected drives, then select your external drive under "External Data Drive".

**Manual SSH**: Run `lsblk` to identify the drive (e.g., `/dev/sda` — use the disk path, not a partition), then add `export DATA_DRIVE=/dev/sda` to `/root/sentryusb.conf` and run `/root/bin/setup-sentryusb`.

> **Warning**: The selected external drive will be completely erased during setup.

When installing SentryUSB with an external drive, use the `norootshrink` variant: `curl -fsSL https://usb.sentry-six.com | bash -s norootshrink`

### What USB cable do I need?

- **Pi 4/5**: USB-A to USB-C cable
- **Pi 3**: USB-A to USB-A cable
- **Pi Zero 2W**: USB-A to Micro-USB cable
- **Important**: Use a **data** cable, not a charge-only cable. If your Tesla doesn't see the drive, try a different cable.

---

## Setup & Configuration

### Is SSH required?

No. Everything can be configured through the web UI Setup Wizard. SSH is only needed for:
- rsync archive method (SSH key setup)
- rclone archive method (`rclone config` interactive setup)
- Advanced troubleshooting

### How do I access the web UI?

| Location | URL |
|----------|-----|
| **At home** (on WiFi) | `http://sentryusb.local` |
| **On the road** (WiFi AP) | `http://192.168.66.1` |
| **Direct IP** | `http://<pi-ip-address>` (check your router's DHCP list) |

### How long does initial setup take?

About **10–20 minutes** after clicking "Apply & Run Setup" in the wizard. The Pi reboots several times during this process — this is completely normal. Do not power off the device.

### Can I re-run the Setup Wizard?

Yes. The wizard is safe to re-run at any time. Go to **Settings** → **Open Wizard**. Changed settings will be applied on the next setup run.

### Can I import my old sentryusb.conf / teslausb.conf?

Yes. On the wizard's **Welcome** step, you can drag and drop a `.conf` file. The wizard parses all `export` variables and pre-fills the corresponding fields. You can then review and adjust before applying.

---

## Day-to-Day Usage

### How do I update SentryUSB?

**From the web UI** (recommended):
1. Go to **Settings**
2. Click **Check for Updates**
3. SentryUSB downloads the latest release and restarts automatically

**From SSH**:
```bash
sudo -i
curl -fsSL https://usb.sentry-six.com | bash
```

### How do I view my dashcam clips?

Go to the **Viewer** page in the web UI. It supports:
- All 6 Tesla camera angles
- 6 layout options for synchronized multi-camera playback
- Timeline scrubbing

### What cameras does the viewer support?

All 6 Tesla cameras: front, rear, left repeater, right repeater, left pillar (B-pillar), and right pillar (B-pillar).

### How do I manage music, lightshow, and boombox files?

Go to the **Files** page in the web UI. You can browse, upload, download, and delete files for Music, LightShow, and Boombox drives (if configured in the Storage step).

### How do I check system status?

The **Dashboard** shows real-time information:
- CPU temperature
- WiFi signal strength
- Disk space usage
- USB drive connection status
- Recent archive snapshots
- Drive map (if enabled)

### How do I see logs?

Go to the **Logs** page. Available logs:
- **Archive Loop** — real-time archiving activity
- **Setup Log** — setup process output
- **Diagnostics** — system health information

The log viewer supports live tailing with a "Follow" button for auto-scroll.

---

## Sentry Connect (iOS App)

### Is there a mobile app?

Yes. **[Sentry Connect](https://apps.apple.com/app/sentry-connect/id6759679030)** is a native iOS/iPadOS app (requires 17.2+) for managing your SentryUSB Pi from your iPhone or iPad. It includes a dashboard, camera viewer, file browser, live logs, drive stats, and push notifications with Live Activity progress on your lock screen.

See [Sentry Connect](SentryConnect) for full details.

### How do I get push notifications on my iPhone?

1. Install [Sentry Connect](https://apps.apple.com/app/sentry-connect/id6759679030) from the App Store
2. Open the SentryUSB web UI → **Settings** → **Mobile Notifications** → **Generate Pairing Code**
3. In Sentry Connect → **Settings** → **Pair for Notifications** → enter the 6-character code

If your phone is on the same WiFi as the Pi, you can tap **Pair Automatically** instead.

### What are Live Activities?

When an archive is running, Sentry Connect shows real-time progress on your Lock Screen and Dynamic Island (iPhone 14 Pro+). It displays the current phase, file count, ETA, and a progress bar. The Pi can start a Live Activity remotely even when the app isn't open.

### Can I use Sentry Connect over Bluetooth?

Yes. When WiFi isn't available, the app connects over Bluetooth LE for access to the dashboard, logs, settings, and drive stats. Video playback and file transfers are not available over BLE. BLE is also used for initial WiFi setup before the Pi has joined a network.

### Is there an Android app?

Not currently. Sentry Connect is iOS-only. For Android, you can use any of the [11 notification providers](Notifications) (Pushover, Telegram, ntfy, etc.) and access the full web UI from your phone's browser.

---

## Troubleshooting

### My Tesla doesn't see the USB drive

- Use a **data** cable (not charge-only)
- Wait 2–3 minutes after plugging in
- Check the Dashboard — "USB Drives" should show "Connected"
- Try a different USB Cable

### Where can I get help?

- [Troubleshooting guide](Troubleshooting) — common issues and fixes
- [GitHub Issues](https://github.com/Sentry-Six/Sentry-USB-Rusty/issues) — bug reports
- [Discord](https://discord.gg/9QZEzVwdnt) — community chat