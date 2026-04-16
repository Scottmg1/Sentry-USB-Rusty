#!/bin/bash
# SentryUSB Rust — Pi Installer
# Usage: curl -sSL https://raw.githubusercontent.com/Scottmg1/Sentry-USB-Rusty/main/install-pi.sh | sudo bash
set -e

REPO="Scottmg1/Sentry-USB-Rusty"
BINARY_NAME="sentryusb-linux-arm64"
INSTALL_DIR="/opt/sentryusb"
SERVICE_NAME="sentryusb"

echo "=== SentryUSB Rust Installer ==="
echo ""

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    aarch64)
        BINARY_NAME="sentryusb-linux-arm64"
        echo "Detected: ARM64 (Pi 3/4/5/Zero2W 64-bit)"
        ;;
    armv7l)
        BINARY_NAME="sentryusb-linux-armv7"
        echo "Detected: ARMv7 (Pi 2/3/Zero2W 32-bit)"
        ;;
    armv6l)
        BINARY_NAME="sentryusb-linux-armv6"
        echo "Detected: ARMv6 (Pi Zero W/1)"
        ;;
    *)
        echo "ERROR: Unsupported architecture: $ARCH"
        echo "Supported: aarch64, armv7l, armv6l"
        exit 1
        ;;
esac

if [ "$(id -u)" -ne 0 ]; then
    echo "ERROR: Run as root (sudo)"
    exit 1
fi

# Get latest release URL (or use main branch artifact)
echo "Fetching latest release..."
RELEASE_URL=$(curl -sSL "https://api.github.com/repos/$REPO/releases/latest" | grep -o "https://.*${BINARY_NAME}" | head -1)

if [ -z "$RELEASE_URL" ]; then
    echo "No release found. Building from source..."

    # Install build deps
    apt-get update -qq
    apt-get install -y -qq build-essential cmake pkg-config libssl-dev curl

    # Install Rust if not present
    if ! command -v cargo &>/dev/null; then
        echo "Installing Rust..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source "$HOME/.cargo/env"
    fi

    # Clone and build
    TMPDIR=$(mktemp -d)
    cd "$TMPDIR"
    git clone --depth 1 "https://github.com/$REPO.git" .

    # Ensure static dir exists
    mkdir -p crates/sentryusb/static
    if [ ! -f crates/sentryusb/static/index.html ]; then
        echo '<!DOCTYPE html><html><body>Frontend not bundled — install from Sentry-USB repo</body></html>' > crates/sentryusb/static/index.html
    fi

    echo "Building (this takes ~10-15 min on Pi 4)..."
    cargo build --release

    mkdir -p "$INSTALL_DIR"
    cp target/release/sentryusb "$INSTALL_DIR/sentryusb"
    cd /
    rm -rf "$TMPDIR"
else
    echo "Downloading binary from: $RELEASE_URL"
    mkdir -p "$INSTALL_DIR"
    curl -sSL "$RELEASE_URL" -o "$INSTALL_DIR/sentryusb"
fi

chmod +x "$INSTALL_DIR/sentryusb"
echo "Binary installed to $INSTALL_DIR/sentryusb"
ls -lh "$INSTALL_DIR/sentryusb"

# Create config file if it doesn't exist
if [ ! -f /root/sentryusb.conf ]; then
    echo "# SentryUSB Configuration" > /root/sentryusb.conf
fi

# Create /mutable for drive data
mkdir -p /mutable

# Create systemd service
cat > /etc/systemd/system/$SERVICE_NAME.service << 'EOF'
[Unit]
Description=SentryUSB Web Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/sentryusb/sentryusb --port 80
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable $SERVICE_NAME
systemctl restart $SERVICE_NAME

echo ""
echo "=== Installation Complete ==="
echo ""

# Get IP address
IP=$(hostname -I | awk '{print $1}')
echo "SentryUSB is running at: http://$IP"
echo ""
echo "Service commands:"
echo "  sudo systemctl status sentryusb    # check status"
echo "  sudo systemctl restart sentryusb   # restart"
echo "  sudo journalctl -u sentryusb -f    # view logs"
echo ""

# Show memory usage
sleep 2
RSS=$(ps aux | grep "/opt/sentryusb/sentryusb" | grep -v grep | awk '{print $6/1024}')
echo "Memory usage: ${RSS} MB RSS"
