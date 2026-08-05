# Sentry USB Wiki

Turn a Raspberry Pi into a smart USB drive for your Tesla's dashcam.
Clips archive themselves. A web UI lets you review, search, and back
them up — no SSH, no config files.

## Install in one line

> **No prebuilt SD card image yet.** The install path is: flash stock **Raspberry Pi OS Lite (64-bit)**, SSH in, and run the one-liner below. A prebuilt image will come later — for now this is the only supported install route. Full step-by-step in [Getting Started](Getting-Started).

On a Pi already running Pi OS:

```bash
sudo apt update && sudo apt upgrade -y
sudo -i
curl -fsSL https://usb.sentry-six.com | bash
```

> Refresh the apt cache first — Pi OS images bake in package lists that go stale as Debian publishes point releases, and a stale cache makes the install hit `404` errors.

Then open `http://sentryusb.local` and the [Setup Wizard](Setup-Wizard-Guide) takes you the rest of the way.

## What you need

| Tier | Boards | Notes |
|------|--------|-------|
| **Recommended** | Raspberry Pi 4B, Raspberry Pi 5 | 1 GB RAM is sufficient. USB 2.0 OTG — fastest archiving, smoothest UI |
| **Tested** | Raspberry Pi Zero 2 W, Raspberry Pi 3A+ | USB 2.0 OTG — works fine, slower archive speeds. Pi 3A+ needs a USB-A-to-USB-A cable |
| **Community** | Radxa Rock Pi 4C+, Radxa Zero 3W | USB 3.0 OTG — reported working, not officially supported |
| **Not supported** | Raspberry Pi 3B/3B+, Pi 2, Pi Zero W (v1), Pi 1 | 3B/3B+/2/1: USB goes through a hub chip — host-only, can never appear as a drive to the car. Zero W v1: too slow, build retired |

Plus:
- A high-quality MicroSD card sized for the USB-drive layout and snapshot retention you choose
- A **USB 3.0 data cable** (not charge-only) — Pi to your Tesla.
  Use a 3.0 cable even on boards that only support USB 2.0 OTG: 3.0 cables
  deliver more power, which keeps lower-end boards stable.
- WiFi network with internet (for first-time setup and updates)

## Quick Links

| Page | What's on it |
|------|--------------|
| [Getting Started](Getting-Started) | Install in 10 minutes |
| [Setup Wizard Guide](Setup-Wizard-Guide) | Every wizard step explained |
| [Drives](Drives) | Trip tracking — route, distance, FSD usage, per-drive telemetry |
| [Tesla BLE Telemetry](Tesla-BLE-Telemetry) | What BLE pulls from the car and how it enriches drives |
| [Archive Methods](Archive-Methods) | CIFS, rsync, rclone, NFS |
| [Notifications](Notifications) | Push notifications to your phone |
| [Privacy](Privacy) | What we send, when, and why — and how to opt out |
| [Sentry Cloud](Sentry-Cloud) | Encrypted cloud backup (in beta) |
| [Troubleshooting](Troubleshooting) | Things that go wrong |
| [FAQ](FAQ) | Common questions |

## Links

- **Site**: [sentryusb.com](https://sentryusb.com)
- **GitHub**: [Sentry-Six/Sentry-USB-Rusty](https://github.com/Sentry-Six/Sentry-USB-Rusty)
- **Releases**: [Latest](https://github.com/Sentry-Six/Sentry-USB-Rusty/releases/latest)
- **Discord**: [Community chat](https://discord.gg/9QZEzVwdnt)
- **License**: PolyForm Noncommercial 1.0.0
