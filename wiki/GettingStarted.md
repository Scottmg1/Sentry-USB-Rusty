# Getting Started

Get SentryUSB running on your Raspberry Pi in about 20 minutes.

## What You Need

| Part | Details |
|------|---------|
| **Raspberry Pi** | Pi 4B, Pi 5, Pi 3 (A+/B/B+), or Pi Zero 2 W |
| **MicroSD card** | 128 GB+ recommended (64 GB minimum) |
| **USB cable** | Pi 4/5: USB-A → USB-C · Pi 3: USB-A → USB-A · Pi Zero 2W: USB-A → Micro-USB |
| **Computer** | With an SD card reader |
| **WiFi** | Internet access for initial setup |

## Quick Start

### 1. Flash Raspberry Pi OS

1. Download [Raspberry Pi Imager](https://www.raspberrypi.com/software/)
2. In Pi Imager:
   - **Operating System** → **Raspberry Pi OS (other)** → **Raspberry Pi OS Lite (64-bit)**
   - **Storage** → select your SD card
   - Click the **⚙️ settings gear** and configure:
     - **Hostname**: `sentryusb`
     - **Enable SSH**: Yes, with password authentication
     - **Username**: `pi`
     - **Password**: choose a strong password
     - **WiFi**: your home SSID and password
     - **Locale**: your timezone and country
   - Click **Write**

### 2. First Boot

1. Insert the SD card into your Pi
2. Power on with a USB power supply — **do NOT plug into the Tesla yet**
3. Wait 2–3 minutes for WiFi to connect
4. Verify: `ping sentryusb.local`

### 3. Install SentryUSB

SSH into the Pi and run the installer:

```bash
ssh pi@sentryusb.local
sudo -i
curl -fsSL https://usb.sentry-six.com | bash
```

If you are using an **external USB/NVMe data drive** (instead of storing data on the SD card), use this variant to skip root partition shrinking:

```bash
curl -fsSL https://usb.sentry-six.com | bash -s norootshrink
```

The installer takes about 2–5 minutes. It downloads the correct binary for your Pi, installs it as a systemd service, and sets up the boot-loop mechanism for setup.

### 4. Configure via Web UI

1. Open **http://sentryusb.local** in your browser
2. Go to **Settings** → **Open Wizard**
3. Walk through all 9 steps:
   - **Welcome** → **Network** → **Storage** → **Archive** → **Keep Awake** → **Notifications** → **Security** → **Advanced** → **Review**
4. Click **Apply & Run Setup**
5. Wait 10–20 minutes — the Pi reboots several times (this is normal). Do not power off.

### 5. Plug Into Your Tesla

1. Disconnect the Pi from its power supply
2. Connect to your Tesla's USB port with a **data** cable
3. Wait 1–2 minutes — the dashcam icon should appear on the Tesla screen

> **Coming soon**: Pre-built SentryUSB images will be available in a future release for an even faster setup experience.

## Accessing the Web UI

| Location | How to Connect |
|----------|---------------|
| **At home** | `http://sentryusb.local` (Pi auto-connects to your WiFi) |
| **On the road** | Connect to the WiFi AP you configured, then `http://192.168.66.1` |
| **Via USB** | Plug Pi into your computer via USB, then `ssh pi@169.254.x.x` |

## Updating SentryUSB

Go to **Settings** → **Check for Updates** in the web UI, or update via SSH with `curl -fsSL https://usb.sentry-six.com | bash`. See the [FAQ](FAQ) for more details.

## Next Steps

- [Setup Wizard Guide](SetupWizard) — detailed walkthrough of every wizard step
- [Archive Methods](ArchiveMethods) — choose how to back up your clips
- [Troubleshooting](Troubleshooting) — common issues and fixes
