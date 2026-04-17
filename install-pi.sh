#!/bin/bash -eu
#
# SentryUSB (Rust) Installer
#
# Minimal installer — downloads the Rust binary and installs the systemd
# service. The binary itself handles ALL setup (partitioning, disk images,
# system config, etc.) via the web UI setup wizard.
#
# Usage:
#   sudo -i
#   curl -fsSL https://raw.githubusercontent.com/Scottmg1/Sentry-USB-Rusty/main/install-pi.sh | bash
#
# Or with a local binary:
#   bash install-pi.sh /path/to/sentryusb-binary

REPO="Scottmg1/Sentry-USB-Rusty"
INSTALL_DIR="/opt/sentryusb"
BINARY_NAME="sentryusb"

RED='\033[0;31m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[0;33m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC} $1"; }
ok()    { echo -e "${GREEN}[OK]${NC} $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
error_exit() { echo -e "${RED}[ERROR]${NC} $1"; exit 1; }

if [[ $EUID -ne 0 ]]; then
    error_exit "This script must be run as root. Try: sudo -i"
fi

# ── Step 1: /sentryusb Symlink ─────────────────────────────────────

info "Setting up /sentryusb symlink..."
if [ ! -L /sentryusb ]; then
    rm -rf /sentryusb
    if [ -d /boot/firmware ] && findmnt --fstab /boot/firmware &> /dev/null; then
        ln -s /boot/firmware /sentryusb
    else
        ln -s /boot /sentryusb
    fi
fi
ok "/sentryusb -> $(readlink /sentryusb)"

# ── Step 2: Install SentryUSB Binary ────────────────────────────────

mkdir -p "$INSTALL_DIR"

if [ -n "${1:-}" ] && [ -f "${1:-}" ]; then
    info "Installing binary from local path: $1"
    cp "$1" "$INSTALL_DIR/$BINARY_NAME"
    chmod +x "$INSTALL_DIR/$BINARY_NAME"
    ok "Binary installed from $1"
else
    info "Downloading latest SentryUSB binary from GitHub..."

    ARCH=$(uname -m)
    case "$ARCH" in
        aarch64) SUFFIX="linux-arm64" ;;
        armv7l)  SUFFIX="linux-armv7" ;;
        armv6l)  SUFFIX="linux-armv6" ;;
        x86_64)  SUFFIX="linux-amd64" ;;
        *)       error_exit "Unsupported architecture: $ARCH" ;;
    esac

    DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}-${SUFFIX}"
    TMP="/tmp/${BINARY_NAME}-new"

    for attempt in $(seq 1 5); do
        if curl -fsSL "$DOWNLOAD_URL" -o "$TMP" 2>/dev/null; then
            chmod +x "$TMP"
            mv "$TMP" "$INSTALL_DIR/$BINARY_NAME"
            ok "Binary downloaded and installed"
            break
        fi
        if [ "$attempt" -eq 5 ]; then
            error_exit "Failed to download binary after 5 attempts"
        fi
        warn "Download failed (attempt $attempt/5), retrying..."
        sleep 3
    done

    RELEASE_TAG=$(curl -fsSL --max-time 10 \
        "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
        | grep '"tag_name"' | head -1 \
        | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/' || true)
    if [ -n "${RELEASE_TAG:-}" ]; then
        echo "$RELEASE_TAG" > "$INSTALL_DIR/version"
        ok "Version: $RELEASE_TAG"
    fi
fi

# Ensure binary is on PATH
if [ ! -L /usr/local/bin/sentryusb ]; then
    ln -sf "$INSTALL_DIR/$BINARY_NAME" /usr/local/bin/sentryusb
fi

# ── Step 3: Systemd Service ─────────────────────────────────────────

info "Installing systemd service..."

cat > /etc/systemd/system/sentryusb.service << 'EOF'
[Unit]
Description=SentryUSB Web Server
After=network-online.target
Wants=network-online.target
Conflicts=nginx.service

[Service]
Type=simple
ExecStartPre=-/bin/systemctl stop nginx
ExecStartPre=-/bin/systemctl disable nginx
ExecStart=/opt/sentryusb/sentryusb --port 80
Restart=always
RestartSec=5
Environment=RUST_LOG=info
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable sentryusb
ok "sentryusb.service installed and enabled"

# ── Step 3b: cttseraser FUSE helper ────────────────────────────────

info "Installing cttseraser FUSE helper..."
CTTS_INSTALL="$INSTALL_DIR/cttseraser"
if [ -n "${1:-}" ] && [ -f "$(dirname "$1")/cttseraser" ]; then
    cp "$(dirname "$1")/cttseraser" "$CTTS_INSTALL"
    chmod +x "$CTTS_INSTALL"
    ok "cttseraser installed from local path"
else
    CTTS_URL="https://github.com/${REPO}/releases/latest/download/cttseraser-${SUFFIX}"
    if curl -fsSL "$CTTS_URL" -o "$CTTS_INSTALL" 2>/dev/null; then
        chmod +x "$CTTS_INSTALL"
        ok "cttseraser downloaded"
    else
        warn "cttseraser binary not available — legacy FUSE feature disabled"
    fi
fi
ln -sf "$CTTS_INSTALL" /usr/local/bin/cttseraser 2>/dev/null || true

# ── Step 3c: BLE daemon (Python) ───────────────────────────────────

info "Installing SentryUSB BLE daemon..."
BLE_REPO_URL="https://raw.githubusercontent.com/${REPO}/main/server/ble"
BLE_INSTALL_DIR="/opt/sentryusb/ble"
mkdir -p "$BLE_INSTALL_DIR"

if curl -fsSL "$BLE_REPO_URL/sentryusb-ble.py" -o "$BLE_INSTALL_DIR/sentryusb-ble.py" 2>/dev/null; then
    chmod +x "$BLE_INSTALL_DIR/sentryusb-ble.py"
    curl -fsSL "$BLE_REPO_URL/sentryusb-ble.service" -o /etc/systemd/system/sentryusb-ble.service 2>/dev/null || true
    curl -fsSL "$BLE_REPO_URL/com.sentryusb.ble.conf" -o /etc/dbus-1/system.d/com.sentryusb.ble.conf 2>/dev/null || true

    # Rewrite service ExecStart to our install path
    if [ -f /etc/systemd/system/sentryusb-ble.service ]; then
        sed -i "s|ExecStart=.*sentryusb-ble.py|ExecStart=/usr/bin/python3 $BLE_INSTALL_DIR/sentryusb-ble.py|" /etc/systemd/system/sentryusb-ble.service || true
    fi

    apt-get install -y python3-dbus python3-gi bluez >/dev/null 2>&1 || warn "BLE daemon apt deps install failed — the daemon may not start"
    systemctl daemon-reload
    systemctl enable sentryusb-ble 2>/dev/null || true
    systemctl restart dbus 2>/dev/null || true
    ok "BLE daemon installed"
else
    warn "Could not fetch BLE daemon — iOS app pairing will be unavailable"
fi

# ── Step 4: Sample Config ───────────────────────────────────────────

if [ ! -f /root/sentryusb.conf ]; then
    info "Creating sample config..."
    cat > /root/sentryusb.conf << 'CONFEOF'
# SentryUSB Configuration
# Edit these values and run setup from the web UI.
#
# Required:
export CAM_SIZE=30G
#export MUSIC_SIZE=4G
#export LIGHTSHOW_SIZE=1G
#export BOOMBOX_SIZE=100M
#export WRAPS_SIZE=0

# Archive system: none, cifs, nfs, rsync, rclone
#export ARCHIVE_SYSTEM=none

# Optional: WiFi access point (min 8 char password)
#export AP_SSID=SentryUSB
#export AP_PASS=

# Optional: Hostname (default: sentryusb)
#export SENTRYUSB_HOSTNAME=sentryusb

# Optional: External USB drive instead of SD card
#export DATA_DRIVE=

# Optional: Use exFAT instead of FAT32
#export USE_EXFAT=false
CONFEOF
    ok "Sample config created at /root/sentryusb.conf"
fi

# ── Step 5: WiFi Marker ────────────────────────────────────────────

if [ ! -f /sentryusb/WIFI_ENABLED ]; then
    touch /sentryusb/WIFI_ENABLED
fi

# ── Step 5b: Hostname + mDNS (sentryusb.local works immediately) ───

TARGET_HOSTNAME="sentryusb"
CURRENT_HOSTNAME=$(hostname -s 2>/dev/null || echo "raspberrypi")

if [ "$CURRENT_HOSTNAME" != "$TARGET_HOSTNAME" ]; then
    info "Setting hostname to ${TARGET_HOSTNAME}..."
    hostnamectl set-hostname "$TARGET_HOSTNAME" 2>/dev/null \
        || echo "$TARGET_HOSTNAME" > /etc/hostname
    # Update /etc/hosts so sudo/local lookups don't warn
    if grep -qE "^127\.0\.1\.1\s" /etc/hosts; then
        sed -i "s/^127\.0\.1\.1\s.*/127.0.1.1\t${TARGET_HOSTNAME}/" /etc/hosts
    else
        echo -e "127.0.1.1\t${TARGET_HOSTNAME}" >> /etc/hosts
    fi
    hostname "$TARGET_HOSTNAME" 2>/dev/null || true
    ok "Hostname set to ${TARGET_HOSTNAME}"
fi

info "Ensuring avahi-daemon is installed for mDNS (${TARGET_HOSTNAME}.local)..."
if ! command -v avahi-daemon >/dev/null 2>&1; then
    apt-get install -y avahi-daemon >/dev/null 2>&1 \
        || warn "avahi-daemon install failed — ${TARGET_HOSTNAME}.local may not resolve"
fi
systemctl enable avahi-daemon >/dev/null 2>&1 || true
systemctl restart avahi-daemon >/dev/null 2>&1 || true
ok "mDNS active: http://${TARGET_HOSTNAME}.local"

# ── Step 6: Start the Service ──────────────────────────────────────

info "Starting SentryUSB..."
systemctl restart sentryusb

# Get IP address for the user — try multiple methods, network may have just bounced
IP=""
for _ in 1 2 3 4 5; do
    IP=$(hostname -I 2>/dev/null | awk '{print $1}')
    if [ -z "$IP" ]; then
        IP=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}')
    fi
    if [ -z "$IP" ]; then
        IP=$(ip -4 -o addr show scope global 2>/dev/null | awk '{print $4}' | cut -d/ -f1 | head -1)
    fi
    [ -n "$IP" ] && break
    sleep 1
done
HOSTNAME="$TARGET_HOSTNAME"

echo ""
echo -e "${GREEN}╔════════════════════════════════════════════════╗${NC}"
echo -e "${GREEN}║        SentryUSB Installation Complete         ║${NC}"
echo -e "${GREEN}╚════════════════════════════════════════════════╝${NC}"
echo ""
if [ -n "$IP" ]; then
    echo -e "  Web UI:  ${BLUE}http://${IP}${NC}"
else
    echo -e "  Web UI:  ${YELLOW}(no IP detected — check 'ip a' once network is up)${NC}"
fi
echo -e "  mDNS:    ${BLUE}http://${HOSTNAME}.local${NC}"
echo ""
echo -e "  Open the web UI to complete setup via the wizard."
echo -e "  All setup (partitions, drives, etc.) is handled by the binary."
echo ""
echo -e "  Config:  /root/sentryusb.conf"
echo -e "  Binary:  ${INSTALL_DIR}/${BINARY_NAME}"
echo -e "  Logs:    journalctl -u sentryusb -f"
echo ""
