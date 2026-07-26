<h1 align="center">Sentry USB</h1>

<p align="center">
  <strong>Turn a Raspberry Pi into a smart USB drive for your Tesla's dashcam.</strong><br>
  Auto-archives. Modern web UI. Multi-camera viewer. Privacy-first.
</p>

<p align="center">
  <a href="https://github.com/Sentry-Six/Sentry-USB-Rusty/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/Sentry-Six/Sentry-USB-Rusty"></a>
  <a href="https://discord.gg/9QZEzVwdnt"><img alt="Discord" src="https://img.shields.io/badge/Discord-join-5865F2?logo=discord&logoColor=white"></a>
  <a href="LICENSE"><img alt="License" src="https://img.shields.io/badge/license-PolyForm%20Noncommercial%201.0.0-blue.svg"></a>
</p>

<p align="center">
  <img src="docs/images/hero-dashboard.png" alt="Sentry USB dashboard" width="900">
</p>

---

## What it does

- **Plugs into your Tesla's USB port** and pretends to be a dashcam drive.
- **Tracks every drive** — route on a map, distance, speed, and the dashcam clips that recorded it — built from the metadata Tesla embeds in each clip.
- **Enriches drives with Tesla BLE telemetry** — battery, HVAC, cabin and exterior temps, TPMS, odometer, and location — pulled over Bluetooth and layered onto each trip for a much fuller picture than the dashcam metadata gives you on its own.
- **Archives clips automatically** to your NAS, cloud, or wherever — over WiFi, in the background.
- **Keeps the car awake** (and the dashcam recording) via the same BLE link — no Tesla API subscription needed.
- **Privacy-first.** No fingerprinting by default; everything sensitive is opt-in.

The Rust rewrite of the original Go version. Same `sentryusb.conf`, faster server, more reliable.

Sentry USB is one of four free tools for Tesla owners from the [Sentry Six](https://sentry-six.com) project, alongside [Sentry Studio](https://github.com/Sentry-Six/Sentry-Six) (desktop TeslaCam viewer), [Sentry Drive](https://github.com/Sentry-Six/Sentry-Drive) (drive history and FSD analytics), and [Sentry Connect](https://sentry-six.com/sentry-connect/) (the iPhone companion app for this Pi). Not affiliated with Tesla, Inc. Unrelated to the Sentry (sentry.io) error-monitoring service.

> [!IMPORTANT]
> Beginning with update 2026.20, Tesla has begun offering the ability to encrypt Tesla cam footage, which is enabled by default. Encrypted files cannot be read by Sentry USB. To continue using Sentry USB, please disable the encryption.
> 
> You can find the relevant settings in your vehicle's menu under Safety, beneath the dashcam USB storage options.

---

## Screenshots

<p align="center">
  <img src="docs/images/drives-list.png" alt="Drives list" width="900"><br>
  <em>Every drive your Tesla recorded — route, distance, duration, battery deltas, and FSD usage — with rolling totals for the selected time range.</em>
</p>

<p align="center">
  <img src="docs/images/drive-detail.png" alt="Drive detail" width="900"><br>
  <em>Per-drive deep view: speed curve, battery deltas, climate with HVAC runtime, odometer, and FSD analytics — alongside the route map and synchronized multi-camera dashcam playback.</em>
</p>

<p align="center">
  <img src="docs/images/charging-list.png" alt="Charging history" width="900"><br>
  <em>Every charge session — energy added, duration, cost, and charging speed — with per-tag time-of-use electricity rates and rolling totals for the selected range.</em>
</p>

<p align="center">
  <img src="docs/images/setup-wizard.png" alt="Setup Wizard" width="900"><br>
  <em>11-step setup wizard — no SSH, no config files.</em>
</p>

<p align="center">
  <img src="docs/images/settings.png" alt="Settings" width="900"><br>
  <em>Everything reconfigurable from the browser.</em>
</p>

---

## Features

| | |
|---|---|
| **Drives tracking** | Every trip with route, distance, duration, battery deltas, and FSD usage — plus the dashcam clips one click in |
| **Tesla BLE telemetry** | Adds battery, HVAC, cabin/exterior temps, TPMS, odometer, and location to each drive — beyond what Tesla's dashcam metadata carries on its own |
| **Multi-camera viewer** | Synchronized 6-camera playback with HW3-aware adaptive grid and drift correction |
| **Auto-archive** | CIFS / SMB, rsync, rclone (cloud), NFS — kicks off whenever the Pi sees known WiFi |
| **Keep awake** | BLE (free, same link as telemetry), TeslaFi, Tessie, or generic webhook |
| **Sentry Cloud (beta)** | Encrypted cloud sync — drives are encrypted on the Pi before they leave your network |
| **Notifications** | Pushover, ntfy, Gotify, Discord, Telegram, Slack, Signal, Matrix, AWS SNS, IFTTT, Webhook, iOS app |
| **Privacy-first** | No device fingerprint by default. Opt-in analytics, full per-flow disclosure |

---

## Install

> [!NOTE]
> **No prebuilt SD card image yet.** You start from stock Raspberry Pi OS Lite and run a one-liner over SSH. A prebuilt image is on the roadmap — for now this is the only supported install path. See the [full Getting Started guide](https://github.com/Sentry-Six/Sentry-USB-Rusty/wiki/Getting-Started) for the click-by-click walkthrough.

On a Pi already running **Raspberry Pi OS Lite (64-bit)**:

```bash
sudo apt update && sudo apt upgrade -y
sudo -i
curl -fsSL https://usb.sentry-six.com | bash
```

> [!WARNING]
> The `apt update && apt upgrade` step is required. Pi OS images bake in an apt cache from whenever the image was built; if Debian has shipped with a newer version, the install will hit `404 Not Found` errors. Running the update and upgrade command will prevent this issue.

Then open `http://sentryusb.local` in a browser and the setup wizard takes you the rest of the way.

### Hardware

| Tier | Boards | Notes |
|------|--------|-------|
| **Recommended** | Raspberry Pi 4B, Raspberry Pi 5 | USB 2.0 OTG — fastest archiving, smoothest UI |
| **Tested** | Raspberry Pi Zero 2 W, Raspberry Pi 3A+ | USB 2.0 OTG — works fine, slower archive speeds. Pi 3A+ needs a USB-A-to-USB-A cable |
| **Community** | Radxa Rock Pi 4C+, Radxa Zero 3W | USB 3.0 OTG — reported working, not officially supported |
| **Not supported** | Raspberry Pi 3B/3B+, Pi 2, Pi Zero W (v1), Pi 1 | 3B/3B+/2/1: USB goes through a hub chip — host-only, can never appear as a drive to the car. Zero W v1: too slow, build retired |

Plus a **256 GB+ MicroSD card** and a **USB 3.0 data cable** (not charge-only) between the Pi and your Tesla. Use a 3.0 cable even on USB 2.0 boards — it delivers more power and keeps lower-end Pis stable.

---

## Privacy

By default, Sentry USB sends **no device identifier** to our servers. Here's everything it ever sends:

| When | What | Identifier? |
|---|---|---|
| Daily update check | Software version, CPU arch, board model | None by default |
| Once per install | Empty ping (no body) | None — anonymous counter |
| Wraps / lock chime submissions | The file + your IP for rate-limiting | None |
| Sentry Cloud (if signed in) | Your account + synced files | Account credentials |
| iOS push pairing (if enabled) | Random pairing ID | Not tied to hardware |

The only way a device fingerprint is sent is if you explicitly opt in to **Settings → Privacy → Analytics opt-in** (default: off). Full disclosure including legal basis, retention, and how to disable each flow lives in [`wiki/Privacy.md`](wiki/Privacy.md).

---

## Documentation

- **[Wiki](https://github.com/Sentry-Six/Sentry-USB-Rusty/wiki)** — Getting Started, Setup Wizard Guide, Archive Methods, Notifications, Privacy, Troubleshooting, FAQ
- **[BUILD.md](BUILD.md)** — Building from source

---

## Build from source

Prerequisites: Rust stable, Node 20+, and `cross` (`cargo install cross`).

```bash
# Web UI
cd web && npm ci && npm run build && cd ..
rm -rf crates/sentryusb/static && cp -r web/dist crates/sentryusb/static

# Binaries (aarch64)
cross build --release --target aarch64-unknown-linux-gnu -p sentryusb

# 32-bit (armhf — Pi 3A+ with 32-bit Pi OS):
cross build --release --target armv7-unknown-linux-gnueabihf -p sentryusb
```

See [BUILD.md](BUILD.md) for details.

### Build a full Pi image (advanced)

```bash
./build-image.sh                 # 64-bit image (Pi 3A+/4/5/Zero 2)
./build-image.sh --32bit         # 32-bit image (armhf — Pi 3A+ with 32-bit Pi OS)
```

The build-image script is for developers and CI; end users should use the curl-one-liner install above.

---

## Based on

A Rust rewrite of the original Go [`Scottmg1/Sentry-USB`](https://github.com/Scottmg1/Sentry-USB), itself a modernized fork of [TeslaUSB](https://github.com/marcone/teslausb) by marcone and contributors.

## Community

- **Discord** — [discord.gg/9QZEzVwdnt](https://discord.gg/9QZEzVwdnt) — fastest help, real humans
- **Issues** — [github.com/Sentry-Six/Sentry-USB-Rusty/issues](https://github.com/Sentry-Six/Sentry-USB-Rusty/issues) — reproducible bugs
- **Site** — [sentry-six.com/sentry-usb](https://sentry-six.com/sentry-usb/)

## License

PolyForm Noncommercial 1.0.0 — free for any noncommercial use; commercial use
requires a separate license. See [LICENSE](LICENSE). Some bundled shell scripts
are derived from [TeslaUSB](https://github.com/marcone/teslausb) and remain
under the MIT License; see [NOTICE](NOTICE) for the file-by-file breakdown.

This is source-available software, not open source — commercial use is not
permitted under this license. To inquire about a commercial license, contact
the authors.
