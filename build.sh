#!/bin/bash
# Usage: ./build.sh [arm64|armv7|native] (default: arm64)

set -e

# Populate the gitignored embed directory; otherwise build.rs supplies its
# frontend-not-built placeholder. Legacy checkouts may use the sibling web app.
WEB_DIR="$(dirname "$0")/web"
if [ ! -f "$WEB_DIR/package.json" ]; then
    WEB_DIR="$(dirname "$0")/../Sentry-USB/web"
fi
if [ -d "$WEB_DIR" ] && [ -f "$WEB_DIR/package.json" ]; then
    echo "Building frontend from $WEB_DIR..."
    (cd "$WEB_DIR" && npm run build)
    echo "Copying frontend to static/"
    # Remove the placeholder and stale pre-compressed siblings.
    rm -rf crates/sentryusb/static
    mkdir -p crates/sentryusb/static
    cp -r "$WEB_DIR/dist/"* crates/sentryusb/static/
else
    echo "WARNING: no web/ found — binary will embed the 'frontend not built' placeholder."
fi

# Pre-compress eligible assets to avoid per-request work on low-end Pis.
# Brotli is optional; the server falls back to gzip or dynamic compression.
if [ -d crates/sentryusb/static ]; then
    HAS_BROTLI=0
    if command -v brotli >/dev/null 2>&1; then
        HAS_BROTLI=1
    else
        echo "Note: 'brotli' not found — skipping .br precompression."
        echo "      Install with: apt install brotli  (or)  brew install brotli"
    fi

    echo "Pre-compressing static assets..."
    COUNT=0
    while IFS= read -r -d '' f; do
        # Skip compressed siblings and formats that are already compressed.
        case "$f" in
            *.br|*.gz|*.woff2|*.png|*.jpg|*.jpeg|*.webp|*.ico|*.gif|*.mp4|*.mp3|*.zip) continue ;;
        esac
        # Compression below 1 KiB usually increases the embedded size.
        SIZE=$(wc -c < "$f")
        if [ "$SIZE" -lt 1024 ]; then continue; fi

        gzip -9 -k -f "$f"
        if [ "$HAS_BROTLI" -eq 1 ]; then
            brotli -q 11 --keep --force "$f"
        fi
        COUNT=$((COUNT + 1))
    done < <(find crates/sentryusb/static -type f -print0)
    echo "  compressed $COUNT files"
fi

TARGET="${1:-arm64}"

case "$TARGET" in
    arm64)
        echo "Building for ARM64 (Pi 3A+/4/5, Pi Zero 2W 64-bit)..."
        cross build --release --target aarch64-unknown-linux-gnu
        BINARY="target/aarch64-unknown-linux-gnu/release/sentryusb"
        ;;
    armv7)
        echo "Building for ARMv7 (Pi 3A+ 32-bit legacy)..."
        cross build --release --target armv7-unknown-linux-gnueabihf
        BINARY="target/armv7-unknown-linux-gnueabihf/release/sentryusb"
        ;;
    native)
        echo "Building native binary..."
        cargo build --release
        BINARY="target/release/sentryusb"
        ;;
    *)
        echo "Unknown target: $TARGET"
        echo "Usage: ./build.sh [arm64|armv7|native]"
        exit 1
        ;;
esac

if [ -f "$BINARY" ]; then
    SIZE=$(du -h "$BINARY" | cut -f1)
    echo "Build complete: $BINARY ($SIZE)"
else
    echo "Build failed!"
    exit 1
fi

# Report the workspace-built telemetry sampler used by its systemd unit.
TELEMETRY_BINARY="$(dirname "$BINARY")/sentryusb-tesla-telemetry"
if [ -f "$TELEMETRY_BINARY" ]; then
    TSIZE=$(du -h "$TELEMETRY_BINARY" | cut -f1)
    echo "Telemetry binary: $TELEMETRY_BINARY ($TSIZE)"
fi
