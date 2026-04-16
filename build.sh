#!/bin/bash
# Build script for SentryUSB Rust binary
# Usage: ./build.sh [target]
#   target: arm64 (default), armv7, native

set -e

# Build frontend (if web/ directory exists in parent project)
SENTRY_USB_DIR="$(dirname "$0")/../Sentry-USB"
if [ -d "$SENTRY_USB_DIR/web" ] && [ -f "$SENTRY_USB_DIR/web/package.json" ]; then
    echo "Building frontend..."
    (cd "$SENTRY_USB_DIR/web" && npm run build)
    echo "Copying frontend to static/"
    rm -rf crates/sentryusb/static/*
    cp -r "$SENTRY_USB_DIR/web/dist/"* crates/sentryusb/static/
fi

TARGET="${1:-arm64}"

case "$TARGET" in
    arm64)
        echo "Building for ARM64 (Pi 3/4/5, Pi Zero 2W 64-bit)..."
        cross build --release --target aarch64-unknown-linux-gnu
        BINARY="target/aarch64-unknown-linux-gnu/release/sentryusb"
        ;;
    armv7)
        echo "Building for ARMv7 (Pi 3 32-bit legacy)..."
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
