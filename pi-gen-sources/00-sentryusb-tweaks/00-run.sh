#!/bin/bash -e

# ── SentryUSB Image Setup ──
# This runs inside pi-gen's chroot during image build.
# Goal: produce an image where the user flashes, boots, and gets a web UI.

touch "${ROOTFS_DIR}/boot/ssh"

# Remove firstrun.sh and the firstboot init hook. WiFi/hostname setup is
# handled by the SentryUSB iOS app via BLE, so Raspberry Pi Imager
# customization is not needed. Stripping the firstboot init= parameter
# prevents the Bookworm initramfs from auto-expanding the root partition
# to fill the entire disk — the setup script needs that free space for
# backingfiles and mutable partitions.
rm -f "${ROOTFS_DIR}/boot/firmware/firstrun.sh"
rm -f "${ROOTFS_DIR}/boot/firmware/userconf.txt"
rm -f "${ROOTFS_DIR}/boot/firmware/custom.toml"
if [ -f "${ROOTFS_DIR}/boot/firmware/cmdline.txt" ]; then
    sed -i \
        -e 's| systemd\.run=/boot/firmware/firstrun\.sh||g' \
        -e 's| systemd\.run=/boot/firstrun\.sh||g' \
        -e 's| systemd\.run_success_action=reboot||g' \
        -e 's| systemd\.unit=kernel-command-line\.target||g' \
        -e 's| init=/usr/lib/raspberrypi-sys-mods/firstboot||g' \
        "${ROOTFS_DIR}/boot/firmware/cmdline.txt"
fi

install -m 755 files/rc.local                             "${ROOTFS_DIR}/etc/"
install -m 666 files/sentryusb.conf.sample                "${ROOTFS_DIR}/boot/firmware/sentryusb.conf"
install -m 666 files/wpa_supplicant.conf.sample           "${ROOTFS_DIR}/boot/firmware"
install -m 666 files/run_once                             "${ROOTFS_DIR}/boot/firmware"
install -d "${ROOTFS_DIR}/root/bin"
install -d "${ROOTFS_DIR}/opt/sentryusb"

# Create /sentryusb symlink → /boot/firmware
ln -sf /boot/firmware "${ROOTFS_DIR}/sentryusb"

# ensure dwc2 module is loaded for USB gadget
echo "dtoverlay=dwc2" >> "${ROOTFS_DIR}/boot/firmware/config.txt"

# ── Pre-install SentryUSB binary ──
# Detect target architecture from the pi-gen build context
REPO="Scottmg1/Sentry-USB"
case "$(dpkg --print-architecture 2>/dev/null || echo arm64)" in
    arm64|aarch64) BINARY_SUFFIX="linux-arm64" ;;
    armhf)         BINARY_SUFFIX="linux-armv6" ;;
    *)             BINARY_SUFFIX="linux-arm64" ;;
esac
BINARY_URL="https://github.com/${REPO}/releases/latest/download/sentryusb-${BINARY_SUFFIX}"

if [ -n "${SENTRYUSB_BINARY:-}" ] && [ -f "${SENTRYUSB_BINARY}" ]; then
    # Allow local binary override for CI builds
    cp "${SENTRYUSB_BINARY}" "${ROOTFS_DIR}/opt/sentryusb/sentryusb"
elif [ -f "files/sentryusb-binary" ]; then
    # Injected by build-image.sh or CI
    cp "files/sentryusb-binary" "${ROOTFS_DIR}/opt/sentryusb/sentryusb"
else
    curl -fsSL "${BINARY_URL}" -o "${ROOTFS_DIR}/opt/sentryusb/sentryusb" || {
        echo "WARNING: Could not download binary from releases. Image will need manual install."
    }
fi
chmod +x "${ROOTFS_DIR}/opt/sentryusb/sentryusb"

# Write version file
RELEASE_TAG=$(curl -fsSL --max-time 10 "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null \
    | grep '"tag_name"' | head -1 \
    | sed 's/.*"tag_name": *"\([^"]*\)".*/\1/' || true)
if [ -n "${RELEASE_TAG:-}" ]; then
    echo "$RELEASE_TAG" > "${ROOTFS_DIR}/opt/sentryusb/version"
    echo "Version: $RELEASE_TAG"
fi

# ── Install BLE peripheral daemon ──
BLE_SCRIPT="${ROOTFS_DIR}/root/bin/sentryusb-ble.py"
if [ -f "files/sentryusb-ble.py" ]; then
    cp "files/sentryusb-ble.py" "${BLE_SCRIPT}"
elif [ -f "../../server/ble/sentryusb-ble.py" ]; then
    cp "../../server/ble/sentryusb-ble.py" "${BLE_SCRIPT}"
else
    curl -fsSL "https://raw.githubusercontent.com/${REPO}/main-dev/server/ble/sentryusb-ble.py" \
        -o "${BLE_SCRIPT}" 2>/dev/null || echo "WARNING: Could not fetch BLE daemon script"
fi
chmod +x "${BLE_SCRIPT}" 2>/dev/null || true

# ── Install D-Bus policy for BLE daemon (required on Pi 5 / Bookworm) ──
DBUS_CONF="${ROOTFS_DIR}/etc/dbus-1/system.d/com.sentryusb.ble.conf"
if [ -f "files/com.sentryusb.ble.conf" ]; then
    install -m 644 "files/com.sentryusb.ble.conf" "${DBUS_CONF}"
elif [ -f "../../server/ble/com.sentryusb.ble.conf" ]; then
    install -m 644 "../../server/ble/com.sentryusb.ble.conf" "${DBUS_CONF}"
else
    echo "WARNING: D-Bus policy file not found — BLE may fail on Pi 5"
fi

# ── Install remountfs_rw helper (needed by BLE daemon to save PIN on read-only rootfs) ──
if [ -f "../../run/remountfs_rw" ]; then
    install -m 755 "../../run/remountfs_rw" "${ROOTFS_DIR}/root/bin/remountfs_rw"
else
    # Inline fallback so the image always has this script
    cat > "${ROOTFS_DIR}/root/bin/remountfs_rw" << 'RWEOF'
#!/bin/bash
mount / -o remount,rw
for _mp in /sentryusb /teslausb; do
  if findmnt "$_mp" > /dev/null 2>&1; then
    mount "$_mp" -o remount,rw
    break
  fi
done
RWEOF
    chmod +x "${ROOTFS_DIR}/root/bin/remountfs_rw"
fi

BLE_SERVICE="${ROOTFS_DIR}/lib/systemd/system/sentryusb-ble.service"
if [ -f "files/sentryusb-ble.service" ]; then
    cp "files/sentryusb-ble.service" "${BLE_SERVICE}"
elif [ -f "../../server/ble/sentryusb-ble.service" ]; then
    cp "../../server/ble/sentryusb-ble.service" "${BLE_SERVICE}"
else
    curl -fsSL "https://raw.githubusercontent.com/${REPO}/main-dev/server/ble/sentryusb-ble.service" \
        -o "${BLE_SERVICE}" 2>/dev/null || echo "WARNING: Could not fetch BLE service file"
fi

# ── Install systemd service for the web UI ──
cat > "${ROOTFS_DIR}/lib/systemd/system/sentryusb.service" << 'SERVICEEOF'
[Unit]
Description=SentryUSB Web Server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/opt/sentryusb/sentryusb -port 80
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
SERVICEEOF

# ── Install prerequisite packages and clean up ──
on_chroot << EOF
# Enable the web server service
systemctl enable sentryusb.service
systemctl enable sentryusb-ble.service 2>/dev/null || true

# Install prerequisites needed by setup scripts
apt-get update -qq
apt-get install -y dos2unix parted fdisk sudo curl python3-dbus python3-gi

# Remove unwanted packages, disable unwanted services, and disable swap
# nginx conflicts with SentryUSB on port 80 — remove it to prevent fallback splash page
apt-get remove -y --purge nginx nginx-common nginx-full 2>/dev/null || true
apt-get remove -y --purge triggerhappy userconf-pi dphys-swapfile firmware-libertas firmware-realtek firmware-atheros mkvtoolnix 2>/dev/null || true
apt-get -y autoremove
systemctl disable keyboard-setup || true
systemctl disable resize2fs_once || true
systemctl disable dpkg-db-backup || true
update-rc.d resize2fs_once remove || true
rm -f /etc/init.d/resize2fs_once
update-initramfs -u || true

# Clean apt cache to reduce image size
apt-get clean
rm -rf /var/lib/apt/lists/*
EOF
