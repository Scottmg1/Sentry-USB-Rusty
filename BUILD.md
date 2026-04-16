# Building SentryUSB (Rust)

## Prerequisites

- Rust stable (1.82+, edition 2024)
- Node 20+ and npm
- `cross` for cross-compilation: `cargo install cross`
- Docker (for `cross` and image builds)

## Web UI

The web UI is React + Vite. Source lives in `web/`; the build output is embedded
into the `sentryusb` binary at compile time via `rust_embed`.

```
cd web
npm ci --no-audit --no-fund
npm run build
cd ..
rm -rf crates/sentryusb/static
cp -r web/dist crates/sentryusb/static
```

## Rust binaries

Two binaries ship with the project:
- `sentryusb` — main daemon (HTTP + WebSocket + setup orchestrator)
- `cttseraser` — FUSE helper that rewrites `ctts` atoms in Tesla dashcam MP4s

### Cross-compile for the Pi

64-bit (Pi 3/4/5/Zero 2):
```
cross build --release --target aarch64-unknown-linux-gnu -p sentryusb
cross build --release --target aarch64-unknown-linux-gnu -p cttseraser
```

32-bit (Pi Zero W / armhf):
```
cross build --release --target armv7-unknown-linux-gnueabihf -p sentryusb
```

Binaries land in `target/<target>/release/`.

### Native (Linux dev box)

```
cargo build --release
```

## Full OS image

`build-image.sh` wraps pi-gen with the SentryUSB stage overlay:
```
./build-image.sh                  # arm64
./build-image.sh --32bit          # armhf
./build-image.sh /path/to/binary  # use a pre-built binary
```

Output: `deploy/sentryusb-*.img.gz`.

## Deploy to an existing Pi

```
# Copy binary to the Pi and run install-pi.sh with its local path
scp target/aarch64-unknown-linux-gnu/release/sentryusb pi@<ip>:/tmp/
ssh pi@<ip> sudo -i
bash install-pi.sh /tmp/sentryusb
```

## Testing

- Rust unit tests: `cargo test`
- Shell harnesses: `tests/*.sh`
- Lint: `./check.sh` (runs shellcheck against vendored shell scripts)

## Releasing

GitHub Releases are expected to host these artifacts (naming consumed by
`install-pi.sh` and `build-image.sh`):

- `sentryusb-linux-arm64`
- `sentryusb-linux-armv7`
- `sentryusb-linux-armv6`
- `cttseraser-linux-arm64`
- `cttseraser-linux-armv7`
