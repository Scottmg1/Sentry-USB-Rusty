## Building a Sentry USB Image

The Sentry USB image is a custom Raspberry Pi OS image with everything pre-installed. Users just flash it, configure WiFi in Pi Imager, boot, and open the web UI — no SSH required.

### Quick Method (recommended)

From the repo root, run:

```bash
./build-image.sh
```

This will:
1. Build the Sentry USB binary (or download from releases)
2. Clone pi-gen and inject the binary
3. Build the image using Docker
4. Output a compressed `.img.gz` in `deploy/`

Takes 15–30 minutes. Requires **Docker**.

You can also pass a pre-built binary:
```bash
./build-image.sh server/bin/sentryusb-linux-arm64
```

### Manual Method

1. Clone pi-gen: `git clone https://github.com/RPi-Distro/pi-gen`
2. From the pi-gen folder, run: `bash /path/to/Sentry-USB/pi-gen-sources/prepare.sh`
3. Optionally set `SENTRYUSB_BINARY=/path/to/sentryusb-linux-arm64` to inject a local binary
4. Run `./build-docker.sh` (Docker recommended) or `./build.sh`
5. Image will be in `deploy/`. Flash with Raspberry Pi Imager.

### GitHub Actions (CI)

Images are automatically built on every GitHub Release. You can also trigger a build manually from the Actions tab → "Build Sentry USB Image" → "Run workflow".

### What's in the image

- Raspberry Pi OS Bookworm Lite (64-bit, arm64)
- SentryUSB binary pre-installed at `/opt/sentryusb/sentryusb`
- systemd service enabled (web UI on port 80)
- rc.local boot-loop for setup (creates partitions, configures archiving)
- SSH enabled, `dwc2` overlay for USB gadget
- Prerequisite packages: dos2unix, parted, fdisk, curl
- Unnecessary packages removed, swap disabled

### User experience after flashing

1. User flashes image with Pi Imager, configures WiFi + password in settings
2. Boots Pi → WiFi connects → Sentry USB web server starts on port 80
3. User opens `http://sentryusb.local` → sees the dashboard
4. Completes Setup Wizard → Pi reboots several times (10–20 min) → done
5. Plugs into Tesla
