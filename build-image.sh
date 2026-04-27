#!/bin/bash -eu
#
# Build a SentryUSB (Rust) Raspberry Pi image locally using pi-gen + Docker.
#
# Prerequisites:
#   - Docker installed and running
#   - Internet access (to download Raspberry Pi OS base)
#   - For local builds: cargo + cross (cargo install cross)
#
# Usage:
#   ./build-image.sh                         # 64-bit image (Pi 3/4/5/Zero 2)
#   ./build-image.sh --32bit                 # 32-bit image (Pi Zero W)
#   ./build-image.sh /path/to/binary         # 64-bit with local binary
#   ./build-image.sh --32bit /path/to/binary # 32-bit with local binary
#
# Output:
#   deploy/sentryusb-*.img.gz — ready to flash with Raspberry Pi Imager
#

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORK_DIR="/tmp/sentryusb/pi-gen"
REPO="Sentry-Six/Sentry-USB-Rusty"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC} $1"; }
ok()    { echo -e "${GREEN}[OK]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

# ── Parse arguments ──
BUILD_32BIT=false
LOCAL_BINARY=""
for arg in "$@"; do
    case "$arg" in
        --32bit|--32|--armhf|--pizero)
            BUILD_32BIT=true
            ;;
        *)
            if [ -f "$arg" ]; then
                LOCAL_BINARY="$(cd "$(dirname "$arg")" && pwd)/$(basename "$arg")"
            fi
            ;;
    esac
done

if $BUILD_32BIT; then
    ARCH_LABEL="32-bit (armhf — Pi Zero W)"
    BINARY_SUFFIX="linux-armv7"
    RUST_TARGET="armv7-unknown-linux-gnueabihf"
    # Tesla vehicle-command is Go; cross-compile targets map independently
    # from our Rust targets. GOARM=6 keeps the tesla binaries runnable on
    # the original Pi Zero W, which is the lowest bar on this image path.
    GO_ARCH="arm"
    GO_ARM="6"
    CONFIG_FILE="pi-gen-config-32bit"
else
    ARCH_LABEL="64-bit (arm64 — Pi 3/4/5/Zero 2)"
    BINARY_SUFFIX="linux-arm64"
    RUST_TARGET="aarch64-unknown-linux-gnu"
    GO_ARCH="arm64"
    GO_ARM=""
    CONFIG_FILE="pi-gen-config"
fi

info "Building $ARCH_LABEL image"

# Check prerequisites
command -v docker &>/dev/null || error "Docker is required. Install it first."

# ── Step 1: Get the SentryUSB binary ──
BINARY_PATH=""
if [ -n "$LOCAL_BINARY" ]; then
    BINARY_PATH="$LOCAL_BINARY"
    info "Using local binary: $BINARY_PATH"
else
    info "Building binary from source..."
    if command -v cross &>/dev/null && command -v node &>/dev/null; then
        (
            cd "$SCRIPT_DIR/web"
            npm ci --no-audit --no-fund 2>&1 | tail -3
            npm run build 2>&1 | tail -3
            # Copy web build into the crate's embedded static/ directory
            rm -rf "$SCRIPT_DIR/crates/sentryusb/static"
            mkdir -p "$SCRIPT_DIR/crates/sentryusb/static"
            cp -r dist/. "$SCRIPT_DIR/crates/sentryusb/static/"
        )
        (
            cd "$SCRIPT_DIR"
            cross build --release --target "$RUST_TARGET" -p sentryusb
            cross build --release --target "$RUST_TARGET" -p cttseraser
        )
        BINARY_PATH="$SCRIPT_DIR/target/$RUST_TARGET/release/sentryusb"
        CTTS_BINARY="$SCRIPT_DIR/target/$RUST_TARGET/release/cttseraser"
        ok "Binary built: $BINARY_PATH"
    else
        info "cross/Node not available locally. Downloading from GitHub releases..."
        BINARY_PATH="/tmp/sentryusb-$BINARY_SUFFIX"
        curl -fsSL "https://github.com/$REPO/releases/latest/download/sentryusb-$BINARY_SUFFIX" -o "$BINARY_PATH" \
            || error "Failed to download binary. Build locally with:\n  cargo install cross\n  cd web && npm ci && npm run build\n  cross build --release --target $RUST_TARGET -p sentryusb"
        CTTS_BINARY="/tmp/cttseraser-$BINARY_SUFFIX"
        curl -fsSL "https://github.com/$REPO/releases/latest/download/cttseraser-$BINARY_SUFFIX" -o "$CTTS_BINARY" 2>/dev/null || true
        ok "Binary downloaded"
    fi
fi

[ -f "$BINARY_PATH" ] || error "Binary not found at $BINARY_PATH"

# ── Step 1b: Build tesla-control + tesla-keygen ────────────────────────
# These binaries drive Tesla vehicles over BLE — they power the `awake_start`
# Keep-Awake BLE mode. Tesla does not publish pre-built binaries, so we
# cross-compile from their vehicle-command repo. Go 1.23+ required.
#
# Without these in the image, the iOS app's Keep-Awake toggle has no way
# to reach the car and the Tesla pairing flow can't hand out keys.
TESLA_CONTROL_PATH=""
TESLA_KEYGEN_PATH=""
if command -v go &>/dev/null; then
    info "Building tesla-control and tesla-keygen from source..."
    TESLA_VC_DIR="/tmp/sentryusb-vehicle-command"
    rm -rf "$TESLA_VC_DIR"
    if git clone --depth 1 https://github.com/teslamotors/vehicle-command.git "$TESLA_VC_DIR" 2>/dev/null; then
        (
            cd "$TESLA_VC_DIR"
            if [ -n "$GO_ARM" ]; then
                GOOS=linux GOARCH=$GO_ARCH GOARM=$GO_ARM go build -o tesla-control ./cmd/tesla-control
                GOOS=linux GOARCH=$GO_ARCH GOARM=$GO_ARM go build -o tesla-keygen ./cmd/tesla-keygen
            else
                GOOS=linux GOARCH=$GO_ARCH go build -o tesla-control ./cmd/tesla-control
                GOOS=linux GOARCH=$GO_ARCH go build -o tesla-keygen ./cmd/tesla-keygen
            fi
        )
        TESLA_CONTROL_PATH="$TESLA_VC_DIR/tesla-control"
        TESLA_KEYGEN_PATH="$TESLA_VC_DIR/tesla-keygen"
        ok "tesla-control and tesla-keygen built"
    else
        info "Could not clone vehicle-command — tesla binaries will NOT be bundled. Keep-Awake BLE mode will be unavailable on the resulting image."
    fi
else
    info "Go not available locally — tesla binaries will NOT be bundled. Keep-Awake BLE mode will be unavailable on the resulting image."
fi

# ── Step 2: Clone pi-gen ──
info "Setting up pi-gen..."
rm -rf "$WORK_DIR"
if $BUILD_32BIT; then
    git clone --depth 1 https://github.com/RPi-Distro/pi-gen.git "$WORK_DIR"
else
    git clone --depth 1 --branch arm64 https://github.com/RPi-Distro/pi-gen.git "$WORK_DIR"
fi

# ── Step 3: Prepare pi-gen with SentryUSB config ──
cd "$WORK_DIR"
bash "$SCRIPT_DIR/pi-gen-sources/prepare.sh"

cp "$SCRIPT_DIR/pi-gen-sources/$CONFIG_FILE" "$WORK_DIR/config"

# ── Step 4: Inject the pre-built binaries and BLE daemon ──
info "Injecting SentryUSB binary into image build..."
cp "$BINARY_PATH" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/sentryusb-binary"
chmod +x "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/sentryusb-binary"

if [ -n "${CTTS_BINARY:-}" ] && [ -f "$CTTS_BINARY" ]; then
    info "Injecting cttseraser FUSE binary..."
    cp "$CTTS_BINARY" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/cttseraser"
    chmod +x "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/cttseraser"
fi

if [ -n "$TESLA_CONTROL_PATH" ] && [ -f "$TESLA_CONTROL_PATH" ]; then
    info "Injecting tesla-control and tesla-keygen..."
    cp "$TESLA_CONTROL_PATH" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/tesla-control"
    cp "$TESLA_KEYGEN_PATH"  "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/tesla-keygen"
    chmod +x "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/tesla-control"
    chmod +x "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/tesla-keygen"
fi

info "Injecting BLE daemon files..."
cp "$SCRIPT_DIR/server/ble/sentryusb-ble.py" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/sentryusb-ble.py" 2>/dev/null \
    || cp "$SCRIPT_DIR/../Sentry-USB/server/ble/sentryusb-ble.py" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/sentryusb-ble.py"
cp "$SCRIPT_DIR/server/ble/sentryusb-ble.service" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/sentryusb-ble.service" 2>/dev/null \
    || cp "$SCRIPT_DIR/../Sentry-USB/server/ble/sentryusb-ble.service" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/sentryusb-ble.service"
cp "$SCRIPT_DIR/server/ble/com.sentryusb.ble.conf" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/com.sentryusb.ble.conf" 2>/dev/null \
    || cp "$SCRIPT_DIR/../Sentry-USB/server/ble/com.sentryusb.ble.conf" "$WORK_DIR/stage_sentryusb/00-sentryusb-tweaks/files/com.sentryusb.ble.conf"

# Trixie apt indices are much larger; increase export image margin
if [[ "$OSTYPE" == darwin* ]]; then
    sed -i '' 's/200 \* 1024 \* 1024/800 * 1024 * 1024/' "$WORK_DIR/export-image/prerun.sh"
else
    sed -i 's/200 \* 1024 \* 1024/800 * 1024 * 1024/' "$WORK_DIR/export-image/prerun.sh"
fi

# ── Step 5: Build the image ──
info "Building image with Docker (this takes 15-30 minutes)..."
./build-docker.sh

# ── Step 6: Copy output ──
IMAGE=$(find "$WORK_DIR/deploy" -name '*.img' | head -1)
if [ -z "$IMAGE" ]; then
    error "Build failed — no image found in deploy/"
fi

mkdir -p "$SCRIPT_DIR/deploy"
info "Compressing image..."
gzip -9 -c "$IMAGE" > "$SCRIPT_DIR/deploy/$(basename "$IMAGE").gz"

ok "Image built successfully!"
echo ""
echo -e "  ${GREEN}Output:${NC} $SCRIPT_DIR/deploy/$(basename "$IMAGE").gz"
echo -e "  ${GREEN}Arch:${NC}   $ARCH_LABEL"
echo ""
echo "  Flash with Raspberry Pi Imager:"
echo "    1. Select 'Use custom' → choose the .img.gz file"
echo "    2. Configure WiFi, hostname (sentryusb), SSH, password in settings"
echo "    3. Write to SD card"
echo ""
echo "  After first boot, open http://sentryusb.local in your browser."
echo ""

# Cleanup
rm -rf "$WORK_DIR"
