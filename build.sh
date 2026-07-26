#!/bin/bash
# Build script for SentryUSB Rust binary
# Usage: ./build.sh [target]
#   target: arm64 (default), armv7, native

set -e

# Build frontend. Prefer this repo's own web/ (current layout); fall back
# to the legacy ../Sentry-USB/web sibling for old checkouts. static/ is
# gitignored (never committed — a stale committed copy once silently
# shipped an old UI), so this step is what populates it; without it a
# bare cargo build embeds the "frontend not built" placeholder that
# crates/sentryusb/build.rs writes.
WEB_DIR="$(dirname "$0")/web"
if [ ! -f "$WEB_DIR/package.json" ]; then
    WEB_DIR="$(dirname "$0")/../Sentry-USB/web"
fi
if [ -d "$WEB_DIR" ] && [ -f "$WEB_DIR/package.json" ]; then
    echo "Building frontend from $WEB_DIR..."
    (cd "$WEB_DIR" && npm run build)
    echo "Copying frontend to static/"
    # Full wipe (not /*) so a stale placeholder index.html and old
    # pre-compressed .br/.gz siblings can't survive into the embed.
    rm -rf crates/sentryusb/static
    mkdir -p crates/sentryusb/static
    cp -r "$WEB_DIR/dist/"* crates/sentryusb/static/
else
    echo "WARNING: no web/ found — binary will embed the 'frontend not built' placeholder."
fi

# Pre-compress static assets so embed.rs can serve raw .br / .gz bytes
# without burning per-request CPU on the Pi Zero 2W. Skips already-
# compressed formats (woff2, png, jpg, ico). Brotli is optional —
# without it the server falls back to gzip, and without gzip it falls
# back to identity + the tower-http CompressionLayer.
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
        # Skip if the file is already a compressed sibling or already-
        # compressed binary format. Anything passing this filter is
        # text/SVG/JSON/JS/CSS/HTML.
        case "$f" in
            *.br|*.gz|*.woff2|*.png|*.jpg|*.jpeg|*.webp|*.ico|*.gif|*.mp4|*.mp3|*.zip) continue ;;
        esac
        # Only worth compressing files above ~1 KB. Smaller files have
        # no compression win and just clutter the binary.
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

# Workspace cross-build above produces every workspace binary; report
# the telemetry sampler too so the release flow / dev knows it's
# there. Pi-gen install must place it at /root/bin/sentryusb-tesla-telemetry
# for the sentryusb-telemetry.service unit to find it.
TELEMETRY_BINARY="$(dirname "$BINARY")/sentryusb-tesla-telemetry"
if [ -f "$TELEMETRY_BINARY" ]; then
    TSIZE=$(du -h "$TELEMETRY_BINARY" | cut -f1)
    echo "Telemetry binary: $TELEMETRY_BINARY ($TSIZE)"
fi
