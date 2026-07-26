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
- `sentryusb-tesla-telemetry` — BLE telemetry sampler (lazy-started after pairing)

### Cross-compile for the Pi

64-bit (Pi 3A+/4/5/Zero 2):
```
cross build --release --target aarch64-unknown-linux-gnu -p sentryusb
```

32-bit (armhf — Pi 3A+ with 32-bit Pi OS):
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

- `sentryusb-linux-arm64-a53` / `-a72` / `-a76` (per-CPU aarch64 variants)
- `sentryusb-linux-arm64` (backward-compat alias = a72 build)
- `sentryusb-linux-armv7`
- `sentryusb-tesla-telemetry-linux-*` (one per CPU variant)

armv6 (Pi Zero W / Pi 1) is no longer built — the board is too underpowered
to run the daemon comfortably, and dropping the matrix entry keeps the
release artifact count manageable.
