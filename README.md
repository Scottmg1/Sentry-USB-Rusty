# SentryUSB (Rust)

Raspberry Pi USB gadget that exposes a Tesla dashcam drive with a modern web UI
for review, archive, and lock-chime customization. Rust rewrite of the original
Go version, feature-parity and drop-in compatible with existing SD-card layouts.

## Install (fresh Pi)

```
sudo -i
curl -fsSL https://raw.githubusercontent.com/Sentry-Six/Sentry-USB-Rusty/main/install-pi.sh | bash
```

Then open `http://sentryusb.local` in a browser and follow the setup wizard.

## Build from source

Prerequisites: Rust stable, Node 20+, and `cross` (`cargo install cross`).

```
# Web UI
cd web && npm ci && npm run build && cd ..
# Copy built assets into embedded static dir
rm -rf crates/sentryusb/static && cp -r web/dist crates/sentryusb/static

# Binaries (aarch64)
cross build --release --target aarch64-unknown-linux-gnu -p sentryusb
cross build --release --target aarch64-unknown-linux-gnu -p cttseraser

# For Pi Zero W / ARMv7:
cross build --release --target armv7-unknown-linux-gnueabihf -p sentryusb
```

See `BUILD.md` for details.

## Build a full Pi image

```
./build-image.sh                 # 64-bit image (Pi 3/4/5/Zero 2)
./build-image.sh --32bit         # 32-bit image (Pi Zero W)
```

Output: `deploy/sentryusb-*.img.gz`, flash with Raspberry Pi Imager.

## Architecture

- `crates/sentryusb` — main daemon binary (embeds web UI via `rust_embed`)
- `crates/api` — axum HTTP + WebSocket handlers
- `crates/config` — configuration parser
- `crates/drives` — USB gadget drive management
- `crates/notify` — push-notification fan-out
- `crates/setup` — partition, image, and system setup phases
- `crates/shell` — shell command helpers
- `crates/usb_gadget` — configfs gadget control
- `crates/ws` — WebSocket hub
- `crates/cttseraser` — FUSE helper for CTTS frame atom stripping
- `server/ble/` — Python BLE GATT peripheral (iOS app pairing)
- `run/`, `setup/pi/`, `tools/`, `tests/` — shell helpers & pi-gen stage scripts
- `pi-gen-sources/` — Raspberry Pi OS image build configuration
- `web/` — React + Vite web UI source

## License

MIT — see `LICENSE`.
