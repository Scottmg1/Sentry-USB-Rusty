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
#   curl -fsSL https://raw.githubusercontent.com/Sentry-Six/Sentry-USB-Rusty/main/install-pi.sh | bash
#
# Or with a local binary:
#   bash install-pi.sh /path/to/sentryusb-binary

REPO="Sentry-Six/Sentry-USB-Rusty"
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

# Backward-compat: the Go install.sh accepted `norootshrink` as its
# first arg to skip the root-partition shrink step (used when an
# external USB/NVMe data drive supplies the storage). In the Rust
# port the shrink moved into the binary's setup wizard — it's
# automatically skipped when DATA_DRIVE is set on the Storage step
# (or in /root/sentryusb.conf). Recognize the legacy arg here so it
# doesn't silently look like a "local binary path" lookup, and
# clear it so it doesn't get treated as one.
case "${1:-}" in
    norootshrink|no-root-shrink|NOROOTSHRINK|norrotshrink)
        info "Note: '$1' was a Go-era install arg; in the Rust port,"
        info "  pick your external drive on the wizard's Storage step"
        info "  (sets DATA_DRIVE) to skip root-partition shrinking."
        shift || true
        ;;
esac

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

# ── Step 2: Install SentryUSB Binary(es) + Picker ──────────────────
#
# On aarch64 we stage three per-CPU-tuned variants (a53/a72/a76) so each
# Pi runs code matched to its microarchitecture. The runtime picker
# (sentryusb-pick-binary, installed below) symlinks the best one to
# sentryusb-current at every service start.
#
# On armv7 there's no microarchitectural split — single variant.
# Same picker handles both cases via /proc/cpuinfo detection.
#
# armv6 (Pi Zero W / Pi 1) is no longer supported: the original Pi Zero W
# is too underpowered to run the daemon and was dropped from CI to keep
# release artifact counts manageable.

mkdir -p "$INSTALL_DIR"

# Detect userspace arch first. The aarch64 case stages multiple binaries;
# the others stage one. Same detection logic the picker uses at boot,
# duplicated here only to decide which release files to download.
if command -v dpkg >/dev/null 2>&1; then
    DPKG_ARCH=$(dpkg --print-architecture)
    case "$DPKG_ARCH" in
        arm64)  ARCH_FAMILY="aarch64" ;;
        armhf)  ARCH_FAMILY="armv7" ;;
        armel)  error_exit "Unsupported architecture: armel (armv6 / Pi Zero W / Pi 1). SentryUSB requires Pi Zero 2 W or newer." ;;
        amd64)  ARCH_FAMILY="amd64" ;;
        *)      error_exit "Unsupported userspace architecture: $DPKG_ARCH" ;;
    esac
else
    case "$(uname -m)" in
        aarch64) ARCH_FAMILY="aarch64" ;;
        armv7l)  ARCH_FAMILY="armv7" ;;
        armv6l)  error_exit "Unsupported architecture: armv6l (Pi Zero W / Pi 1). SentryUSB requires Pi Zero 2 W or newer." ;;
        x86_64)  ARCH_FAMILY="amd64" ;;
        *)       error_exit "Unsupported architecture: $(uname -m)" ;;
    esac
fi

# Map the family → suffixes we need to download. aarch64 expands to three.
case "$ARCH_FAMILY" in
    aarch64) SUFFIXES="linux-arm64-a53 linux-arm64-a72 linux-arm64-a76" ;;
    armv7)   SUFFIXES="linux-armv7" ;;
    amd64)   SUFFIXES="linux-amd64" ;;
esac

if [ -n "${1:-}" ] && [ -f "${1:-}" ]; then
    # Local-binary mode — installer was invoked with a path to a binary on
    # disk. Skip downloads and stage that one binary under all matching
    # CPU suffixes so the picker always finds something. (This is a
    # convenience for local dev builds; production installs use the
    # download path below.)
    info "Installing binary from local path: $1"
    for sfx in $SUFFIXES; do
        cp "$1" "$INSTALL_DIR/$BINARY_NAME-$sfx"
        chmod +x "$INSTALL_DIR/$BINARY_NAME-$sfx"
    done
    ok "Local binary staged under $(echo $SUFFIXES | tr ' ' '\n' | wc -l) variant(s)"
else
    info "Downloading SentryUSB binary variants from GitHub..."

    for sfx in $SUFFIXES; do
        DOWNLOAD_URL="https://github.com/${REPO}/releases/latest/download/${BINARY_NAME}-${sfx}"
        TMP="/tmp/${BINARY_NAME}-${sfx}.new"
        success=false
        for attempt in $(seq 1 5); do
            if curl -fsSL "$DOWNLOAD_URL" -o "$TMP" 2>/dev/null; then
                chmod +x "$TMP"
                mv "$TMP" "$INSTALL_DIR/$BINARY_NAME-$sfx"
                ok "Downloaded $BINARY_NAME-$sfx"
                success=true
                break
            fi
            warn "Download of $sfx failed (attempt $attempt/5), retrying..."
            sleep 3
        done
        if [ "$success" != true ]; then
            error_exit "Failed to download $BINARY_NAME-$sfx after 5 attempts"
        fi
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

# ── Picker script (selects the right binary at every service start) ──
PICKER_URL="https://raw.githubusercontent.com/${REPO}/main/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb-pick-binary"
PICKER_DST="/usr/local/bin/sentryusb-pick-binary"
PICKER_LOCAL_FALLBACK="$(dirname "${1:-/dev/null}")/sentryusb-pick-binary"
if [ -f "$PICKER_LOCAL_FALLBACK" ]; then
    install -m 755 "$PICKER_LOCAL_FALLBACK" "$PICKER_DST"
    ok "Picker installed from local path"
elif curl -fsSL --max-time 10 "$PICKER_URL" -o "$PICKER_DST" 2>/dev/null; then
    chmod +x "$PICKER_DST"
    ok "Picker downloaded to $PICKER_DST"
else
    error_exit "Failed to install sentryusb-pick-binary — daemon won't start without it"
fi

# Run the picker once now so the -current symlink + active-variant file
# exist before systemd tries to start the service.
"$PICKER_DST" || error_exit "sentryusb-pick-binary failed on first run — check journalctl"

# Back-compat symlink at the old path so any third-party tooling or shell
# wrappers referencing /opt/sentryusb/sentryusb keep working.
ln -sfn "$INSTALL_DIR/sentryusb-current" "$INSTALL_DIR/$BINARY_NAME"

# Ensure binary is on PATH
if [ ! -L /usr/local/bin/sentryusb ]; then
    ln -sf "$INSTALL_DIR/sentryusb-current" /usr/local/bin/sentryusb
fi

# ── Step 3: Systemd Service ─────────────────────────────────────────

info "Installing systemd service..."

cat > /etc/systemd/system/sentryusb.service << 'EOF'
[Unit]
Description=SentryUSB Web Server
After=mutable.mount backingfiles.mount
Wants=mutable.mount backingfiles.mount
Conflicts=nginx.service

[Service]
Type=simple
ExecStartPre=-/bin/systemctl stop nginx
ExecStartPre=-/bin/systemctl disable nginx
# Re-pick the best per-CPU binary on every start so a hardware swap
# (re-flashing the SD card into a different Pi) is handled automatically.
ExecStartPre=/usr/local/bin/sentryusb-pick-binary
ExecStart=/opt/sentryusb/sentryusb-current --port 80
Restart=always
RestartSec=5
Environment=RUST_LOG=info
# Cap glibc malloc arenas to 2. Default on multicore ARM is 8× nproc
# arenas, each holding a fragmented heap fork that the kernel never
# reclaims. Steady-state RSS on Pi-class hardware drops ~40-50% with
# this cap, with no measurable throughput impact for our workload.
Environment=MALLOC_ARENA_MAX=2
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable sentryusb
ok "sentryusb.service installed and enabled"

# ── Step 3b: BLE daemon (Python) ───────────────────────────────────

info "Installing SentryUSB BLE daemon..."
BLE_REPO_URL="https://raw.githubusercontent.com/${REPO}/main/server/ble"
# Install at /root/bin/ — matches both the vendored service unit's
# hardcoded ExecStart path AND what pi-gen 00-run.sh installs, so the
# binary is reachable whether the user came via image-flash or
# install-pi.sh. Don't install elsewhere + sed-patch the unit: that can
# silently fail on older sed / SELinux, leaving the service pointing at
# a missing path.
BLE_INSTALL_PATH="/root/bin/sentryusb-ble.py"
mkdir -p /root/bin

if curl -fsSL "$BLE_REPO_URL/sentryusb-ble.py" -o "$BLE_INSTALL_PATH" 2>/dev/null; then
    chmod +x "$BLE_INSTALL_PATH"
    curl -fsSL "$BLE_REPO_URL/sentryusb-ble.service" -o /etc/systemd/system/sentryusb-ble.service 2>/dev/null || true
    curl -fsSL "$BLE_REPO_URL/com.sentryusb.ble.conf" -o /etc/dbus-1/system.d/com.sentryusb.ble.conf 2>/dev/null || true

    apt-get install -y python3-dbus python3-gi bluez >/dev/null 2>&1 || warn "BLE daemon apt deps install failed — the daemon may not start"
    systemctl daemon-reload
    systemctl enable sentryusb-ble 2>/dev/null || true
    # Reload (SIGHUP) — NOT restart. Restarting dbus on Pi OS kills logind,
    # which kills any active SSH session and can wedge the box hard enough
    # to need a power-cycle. Reload picks up the new policy file (which is
    # all we need — dbus rereads /etc/dbus-1/system.d/ on SIGHUP) without
    # dropping any clients.
    systemctl reload dbus 2>/dev/null || true
    ok "BLE daemon installed at $BLE_INSTALL_PATH"
else
    warn "Could not fetch BLE daemon — iOS app pairing will be unavailable"
fi

# ── Step 3b2: Disable EATT (no recurring BLE pairing prompt) ────────
# The BLE GATT is app-PIN over plain (unencrypted) ATT, but a phone's EATT
# (PSM 0x0027) open needs an encrypted link — bluetoothd refuses it and sends an
# SMP Security Request, popping a pair prompt on every connect. Channels=1 keeps
# plain ATT (same GATT), so no prompt. Safe on all boards (no security change).
MAIN_CONF=/etc/bluetooth/main.conf
if [ -f "$MAIN_CONF" ] && ! grep -qE '^Channels[[:space:]]*=[[:space:]]*1' "$MAIN_CONF"; then
    if grep -qE '^\[GATT\]' "$MAIN_CONF"; then
        if grep -qiE '^[# ]*Channels' "$MAIN_CONF"; then
            sed -i -E 's/^[# ]*Channels[ ]*=.*/Channels = 1/' "$MAIN_CONF"
        else
            sed -i '/^\[GATT\]/a Channels = 1' "$MAIN_CONF"
        fi
    else
        printf '\n[GATT]\nChannels = 1\n' >> "$MAIN_CONF"
    fi
    systemctl restart bluetooth 2>/dev/null || true
    ok "EATT disabled (no recurring BLE pairing prompt)"
fi

# ── Step 3c: archiveloop ↔ gadget shim scripts ─────────────────────
#
# archiveloop (shell) calls /root/bin/enable_gadget.sh and disable_gadget.sh
# directly. On a pre-existing Go install those are real configfs scripts; if
# we leave them alone they fight with the Rust handler — two concurrent
# writers to the same /sys/kernel/config/usb_gadget/sentryusb tree produces
# half-configured gadgets that enumerate without exposing LUNs.
#
# Replace them with thin curl shims so archiveloop drives the Rust API
# instead. The shims are idempotent — archiveloop can call enable while we're
# already enabled without side effects.

info "Installing archiveloop gadget shims..."
mkdir -p /root/bin

cat > /root/bin/enable_gadget.sh <<'SHIM'
#!/bin/bash
# Rust SentryUSB shim — archiveloop calls this; we forward to the Rust API.
# Loopback requests bypass the web auth middleware.
exec curl -fsS --max-time 30 -X POST http://127.0.0.1/api/system/gadget-enable
SHIM
chmod +x /root/bin/enable_gadget.sh

cat > /root/bin/disable_gadget.sh <<'SHIM'
#!/bin/bash
exec curl -fsS --max-time 30 -X POST http://127.0.0.1/api/system/gadget-disable
SHIM
chmod +x /root/bin/disable_gadget.sh

ok "Gadget shims installed at /root/bin/{enable,disable}_gadget.sh"

# ── Step 3d: remountfs_rw helper + /root/.bashrc reminder ──────────
# `remountfs_rw` is created by the pi-gen image build; install-pi.sh users
# (any non-pi-gen install, e.g. DietPi/Armbian) never get it. The BLE daemon
# calls it to remount root RW before saving the pairing PIN, and fails with
# "Failed to save PIN: No such file or directory: '/root/bin/remountfs_rw'"
# if absent — blocks BLE pair from SC. Always-install a tiny stub: works
# whether root is RO (does the remount) or already RW (no-op + exit 0).
mkdir -p /root/bin
if [ ! -f /root/bin/remountfs_rw ]; then
    cat > /root/bin/remountfs_rw <<'REMOUNT_RW'
#!/bin/bash
# remount root RW (no-op if already RW). Used by sentryusb-ble.py for PIN save.
mount -o remount,rw / 2>/dev/null
exit 0
REMOUNT_RW
    chmod +x /root/bin/remountfs_rw
    ok "Installed /root/bin/remountfs_rw stub (BLE daemon PIN save)"
fi
if ! grep -q SENTRYUSB_TIP1 /root/.bashrc 2>/dev/null; then
    cat >> /root/.bashrc <<- 'EOC'
	if [ -n "$PS1" ]; then
		cat << SENTRYUSB_TIP1
		The root partition is mounted read-only.
		Run 'bin/remountfs_rw' to allow writing to it.

		SENTRYUSB_TIP1
	fi
	EOC
    ok "Added remountfs_rw reminder to /root/.bashrc"
fi

# ── Step 3e: Rock Pi 4C+ (RK3399 / dwc3) hardware setup ────────────────────
# A NO-OP on Raspberry Pi and every non-4C+ board (detection-gated). On a Rock
# Pi 4C+ a generic install leaves three things broken, all fixed here so SC works
# with WiFi + BLE out of the box:
#   1. rfkill — the BLE daemon's unit calls /usr/sbin/rfkill; DietPi's minimal
#      base omits it, so sentryusb-ble.service fails 203/EXEC without it.
#   2. dwc3 overlay → OTG port to PERIPHERAL/high-speed (else /sys/class/udc is
#      empty → no USB mass-storage gadget → Tesla never sees the dashcam).
#   3. BT+WiFi firmware (AP6256/BCM4345C0 combo) + a legacy raw-HCI LE advertiser
#      (the chip rejects BlueZ extended advertising, so SC can't discover it).
# Best-effort: each sub-step warns on failure rather than aborting the install.
is_rock_4cplus() {
    grep -qai 'rock-4c-plus\|rockpi4c-plus\|ROCK 4C+' \
        /proc/device-tree/model /proc/device-tree/compatible 2>/dev/null
}
has_dietpi_overlays() {
    [ -f /boot/dietpiEnv.txt ] && grep -q '^overlay_path=' /boot/dietpiEnv.txt
}

NEEDS_REBOOT=0
if is_rock_4cplus; then
    info "Rock Pi 4C+ detected — applying USB-gadget + BLE hardware setup..."
    # Best-effort section: don't let a minor apt/systemd hiccup abort the install.
    set +e

    # 1. Apt dependencies — rfkill (BLE daemon calls it) and device-tree-compiler
    #    (sub-step 2 compiles a dwc3 overlay with `dtc`; DietPi minimal ships neither).
    if apt-get install -y rfkill device-tree-compiler >/dev/null 2>&1; then
        ok "rfkill + device-tree-compiler installed"
        systemctl reset-failed sentryusb-ble.service 2>/dev/null || true
        systemctl restart sentryusb-ble.service 2>/dev/null || true
    else
        warn "rfkill/dtc install failed — BLE daemon and dwc3 overlay may not work"
    fi

    # 2. High-speed dwc3 peripheral overlay (compiled on-device → self-contained)
    if has_dietpi_overlays; then
        apt-get install -y device-tree-compiler >/dev/null 2>&1 || true
        mkdir -p /boot/overlay-user
        cat > /tmp/sentryusb-dwc3-hs.dts <<'DTS'
/dts-v1/;
/plugin/;
/ {
    metadata {
        title = "SentryUSB: OTG peripheral high-speed (Rock 4C+)";
        compatible = "rockchip,rk3399";
        category = "misc";
        exclusive = "usbdrd_dwc3_0-dr_mode";
        description = "dwc3 OTG → peripheral mode, high-speed, for the USB gadget.";
    };
    fragment@0 {
        target = <&usbdrd_dwc3_0>;
        __overlay__ {
            status = "okay";
            dr_mode = "peripheral";
            maximum-speed = "high-speed";
        };
    };
};
DTS
        if dtc -@ -I dts -O dtb -o /boot/overlay-user/sentryusb-dwc3-hs.dtbo \
               /tmp/sentryusb-dwc3-hs.dts 2>/dev/null; then
            ok "Compiled high-speed dwc3 overlay → /boot/overlay-user/sentryusb-dwc3-hs.dtbo"
            cur=$(grep '^user_overlays=' /boot/dietpiEnv.txt | cut -d= -f2-)
            case " $cur " in
                *" sentryusb-dwc3-hs "*)
                    ok "Overlay already registered in user_overlays" ;;
                *)
                    new=$(echo "$cur sentryusb-dwc3-hs" | xargs)
                    cp /boot/dietpiEnv.txt /boot/dietpiEnv.txt.sentryusb.bak
                    sed -i "s/^user_overlays=.*/user_overlays=$new/" /boot/dietpiEnv.txt
                    ok "Registered overlay (user_overlays=$new)"
                    NEEDS_REBOOT=1 ;;
            esac
        else
            warn "dwc3 overlay compile failed — USB gadget will NOT appear until applied manually"
        fi
    else
        warn "Rock 4C+ but no DietPi/Armbian overlay mechanism found — apply a dwc3"
        warn "peripheral+high-speed overlay for your image manually, or no USB gadget."
    fi

    # 3. Bluetooth + WiFi firmware — AP6256 (BCM4345C0 WiFi+BT combo) coexistence.
    #    BT .hcd MUST be the GENERIC patch, NOT BCM4345C0.raspberrypi,*.hcd — the Pi
    #    profile kills the WiFi SDIO half (brcmf rxctl timeout / wlan0 I/O error).
    BRCM=/lib/firmware/brcm
    HCD=""
    for c in BCM4345C0_003.001.025.0162.0000_Generic_UART_37_4MHz_wlbga_ref_iLNA_iTR_eLG.hcd \
             BCM4345C0.raspberrypi,4-compute-module.hcd; do
        [ -e "$BRCM/$c" ] && { HCD="$c"; break; }
    done
    [ -z "$HCD" ] && HCD=$(cd "$BRCM" 2>/dev/null && ls BCM4345C0*.hcd 2>/dev/null | grep -vE 'radxa,rock-4c-plus|raspberrypi' | head -1)
    if [ -n "$HCD" ] && [ -e "$BRCM/$HCD" ]; then
        ln -sf "$HCD" "$BRCM/BCM4345C0.radxa,rock-4c-plus.hcd"
        ln -sf "$HCD" "$BRCM/BCM4345C0.hcd"
        ok "BT firmware → $HCD (generic AP6256 patch, NOT the Pi profile) — reboot to load"
        NEEDS_REBOOT=1
    else
        warn "BCM4345C0 .hcd not found — 'apt install --reinstall armbian-firmware', then"
        warn "symlink BCM4345C0.radxa,rock-4c-plus.hcd → the generic BCM4345C0 .hcd."
    fi
    if [ -e "$BRCM/nvram_ap6256.txt" ]; then
        ln -sf nvram_ap6256.txt "$BRCM/brcmfmac43455-sdio.radxa,rock-4c-plus.txt"
        [ -e "$BRCM/brcmfmac43455-sdio.bin" ] && \
            ln -sf brcmfmac43455-sdio.bin "$BRCM/brcmfmac43455-sdio.radxa,rock-4c-plus.bin"
        [ -e "$BRCM/brcmfmac43455-sdio.clm_blob" ] && \
            ln -sf brcmfmac43455-sdio.clm_blob "$BRCM/brcmfmac43455-sdio.radxa,rock-4c-plus.clm_blob"
        ok "WiFi nvram → nvram_ap6256.txt (AP6256 calibration) — WiFi now survives BT"
        NEEDS_REBOOT=1
    else
        warn "nvram_ap6256.txt not found — WiFi may be unstable with BT (generic calibration)."
    fi

    # 3b. THE BLE-advertising fix. The BCM4345C0 firmware rejects BlueZ EXTENDED
    #     advertising (mgmt 0x0054/55 → "Invalid Parameters 0x0d"), and even legacy
    #     `btmgmt add-adv` is unreliable (ActiveInstances:0 / ~1280ms interval →
    #     Android misses it → connectGatt 147). Fix = (i) daemon doesn't sys.exit on
    #     BlueZ adv failure; (ii) a helper programs legacy ADV_IND @ 100ms directly
    #     over raw HCI (SC then connects to the real MAC + authenticates); (iii) start
    #     event-driven when hci0 appears (UART BT attaches late on cold boot).
    BLE_PY=/root/bin/sentryusb-ble.py
    if [ -f "$BLE_PY" ] && ! grep -q 'legacy btmgmt advertising' "$BLE_PY"; then
        python3 - "$BLE_PY" <<'PYEOF' || true
import sys
p = sys.argv[1]; s = open(p).read()
a = s.find('def register_ad_error_cb(error):'); b = s.find('\ndef register_app_cb', a)
if a >= 0 and b >= 0:
    cb = ("def register_ad_error_cb(error):\n"
          "    # BCM4345C0 (Rock 4C+): BlueZ uses EXTENDED advertising which this chip\n"
          "    # rejects ('Invalid Parameters 0x0d'). Do NOT exit (that tears down GATT\n"
          "    # and loops forever); keep GATT up. Legacy btmgmt advertising is enabled\n"
          "    # out-of-band by sentryusb-ble-adv.service.\n"
          "    log.warning(f'BlueZ advertisement registration failed ({error}); '\n"
          "                'using legacy btmgmt advertising instead; GATT stays up.')\n")
    open(p, 'w').write(s[:a] + cb + s[b+1:]); print('patched')
else:
    print('anchor-not-found')
PYEOF
        ok "Patched sentryusb-ble.py: BlueZ adv failure no longer kills the GATT server"
    fi
    cat > /usr/local/bin/sentryusb-ble-adv.sh <<'ADVSH'
#!/bin/bash
# Raw legacy LE advertising for the BCM4345C0 (Rock Pi 4C+ / AP6256 module).
#
# WHY RAW HCI instead of btmgmt (validated on-device 2026-06-19): on this chip
# BlueZ's mgmt advertiser is unreliable. Extended advertising (mgmt 0x0054/55) is
# rejected outright; the LEGACY `btmgmt add-adv` REPORTS success but
# `bluetoothctl show` reports ActiveInstances:0, AND it uses the controller's slow
# ~1280ms default interval — so Android rarely catches the advert and connectGatt
# fails with GATT_CONNECTION_TIMEOUT(147). Programming legacy ADV_IND directly over
# HCI at a 100ms interval is the only reliable path: SC then connects to the real
# MAC (2C:3B:70:69:4B:88) and authenticates. The SentryUSB apps filter scans by the
# "SentryUSB-" NAME prefix, so the name MUST be in the scan response or the advert
# is invisible to them. (hcitool ships with the `bluez` package.)
#
# Byte layout below is the exact validated sequence — do not "tidy" it.
UUID_LE="9e ca dc 24 0e e5 a9 e0 93 f3 a3 b5 01 00 40 6e"   # 6e400001-b5a3-f393-e0a9-e50e24dcca9e, little-endian
ADV_DATA="15 02 01 06 11 07 ${UUID_LE} 00 00 00 00 00 00 00 00 00 00 00 00 00"

# Scan-response bytes = [len][0x09 Complete Local Name][name…], space-separated for
# hcitool. Defined BEFORE the run-guard so `source sentryusb-ble-adv.sh;
# build_scanrsp` is testable without entering the advertising loop below.
build_scanrsp() {
    local name hex namebytes len
    name=$(timeout 5 btmgmt info 2>/dev/null | sed -n 's/^[[:space:]]*name[[:space:]]*//p' | head -1)
    [ -z "$name" ] && name=$(hostname)
    name=${name:0:29}                                   # scan-rsp budget: 31 - 2 (len+type)
    hex=$(printf '%s' "$name" | od -An -tx1 | tr -d ' \n')
    namebytes=$(( ${#hex} / 2 ))
    len=$(( namebytes + 1 ))                             # +1 for the AD type byte
    printf '%02x 09 %s' "$len" "$(echo "$hex" | sed 's/../& /g')"
}

# Program legacy ADV_IND directly via HCI (bypasses BlueZ mgmt):
#   disable → set adv data → set scan-rsp (name, zero-padded to 31) →
#   adv params (100ms min/max ADV_IND connectable undirected, public addr, all 3 chans) → enable
assert_raw_adv() {
    local scanrsp; scanrsp=$(build_scanrsp)
    hcitool -i hci0 cmd 0x08 0x000a 00 >/dev/null 2>&1 || true
    hcitool -i hci0 cmd 0x08 0x0008 $ADV_DATA >/dev/null 2>&1 || true
    hcitool -i hci0 cmd 0x08 0x0009 $scanrsp 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 00 >/dev/null 2>&1 || true
    hcitool -i hci0 cmd 0x08 0x0006 a0 00 a0 00 00 00 00 00 00 00 00 00 00 07 00 >/dev/null 2>&1 || true
    hcitool -i hci0 cmd 0x08 0x000a 01 >/dev/null 2>&1 || true
}

# Run the advertising loop ONLY when executed directly (the systemd ExecStart),
# never when sourced — otherwise testing build_scanrsp() would loop forever.
if [ "${BASH_SOURCE[0]}" != "${0}" ]; then
    return 0 2>/dev/null || exit 0
fi

for i in $(seq 1 30); do busctl status org.bluez >/dev/null 2>&1 && break; sleep 1; done
sleep 3   # let the daemon's (failing) ext-adv RegisterAdvertisement settle first
timeout 5 btmgmt le on >/dev/null 2>&1 || true
timeout 5 btmgmt connectable on >/dev/null 2>&1 || true
while true; do
    # Re-assert only while IDLE — reprogramming advertising while a phone is
    # connected can disturb Broadcom controllers and isn't needed for a live link.
    if ! timeout 3 btmgmt con 2>/dev/null | grep -q 'type LE'; then
        assert_raw_adv
    fi
    sleep 5
done
ADVSH
    chmod +x /usr/local/bin/sentryusb-ble-adv.sh
    cat > /etc/systemd/system/sentryusb-ble-adv.service <<'ADVSVC'
[Unit]
Description=SentryUSB: legacy LE advertising (Rock 4C+ BCM4345C0 ext-adv workaround)
After=bluetooth.service sentryusb-ble.service
Wants=bluetooth.service
BindsTo=sentryusb-ble.service

[Service]
Type=simple
ExecStart=/usr/local/bin/sentryusb-ble-adv.sh
Restart=always
RestartSec=10

[Install]
WantedBy=multi-user.target
ADVSVC
    cat > /etc/udev/rules.d/99-sentryusb-ble-hci.rules <<'UDEV'
# Rock 4C+ (BCM4345C0): the UART BT controller attaches a few seconds into boot,
# AFTER systemd checks bluetooth.service's ConditionPathIsDirectory, so the BLE
# stack is skipped on a cold boot. Start it the moment hci0 appears (condition now
# passes; the daemon's Wants= pulls bluetooth.service + the advertising helper).
ACTION=="add", SUBSYSTEM=="bluetooth", KERNEL=="hci0", TAG+="systemd", ENV{SYSTEMD_WANTS}+="sentryusb-ble.service"
UDEV
    mkdir -p /etc/systemd/system/sentryusb-ble.service.d
    cat > /etc/systemd/system/sentryusb-ble.service.d/wants-bluetooth.conf <<'WANTS'
[Unit]
Wants=bluetooth.service sentryusb-ble-adv.service
WANTS
    systemctl disable --now sentryusb-ble-le.service 2>/dev/null || true
    rm -f /etc/systemd/system/sentryusb-ble-le.service 2>/dev/null
    rm -rf /etc/systemd/system/sentryusb-ble-le.service.d 2>/dev/null
    systemctl enable bluetooth.service >/dev/null 2>&1 || true
    systemctl daemon-reload 2>/dev/null || true
    udevadm control --reload-rules 2>/dev/null || true
    systemctl enable sentryusb-ble-adv.service >/dev/null 2>&1 || true
    ok "BLE legacy-advertising fix installed (daemon patch + adv service + hci0 udev rule)"
    NEEDS_REBOOT=1

    # 4. (Recommended) OpenSSH instead of Dropbear — Dropbear ships no SFTP
    #    subsystem, so scp/sftp to the Pi fail.
    if command -v dropbear >/dev/null 2>&1 && [ -x /boot/dietpi/func/dietpi-set_software ]; then
        if /boot/dietpi/func/dietpi-set_software ssh-server openssh >/dev/null 2>&1; then
            ok "Switched SSH server to OpenSSH (scp/sftp support)"
        else
            warn "OpenSSH switch failed — Dropbear left in place (scp/sftp unavailable)"
        fi
    fi

    set -e  # end best-effort section
fi

# ── Step 4: Sample Config ───────────────────────────────────────────

if [ ! -f /root/sentryusb.conf ]; then
    info "Creating sample config..."
    # NOTE: this MUST be the Rust port repo (Sentry-USB-Rusty). Earlier
    # versions pointed at the legacy Go repo, so the download silently
    # returned the Go-era sample OR fell back to the tiny offline stub
    # below — both of which left the "raw config editor" in the web UI
    # showing only a handful of keys instead of the full documented set.
    SAMPLE_URL="https://raw.githubusercontent.com/${REPO}/main/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb.conf.sample"
    if curl -fsSL --max-time 15 "$SAMPLE_URL" -o /root/sentryusb.conf; then
        ok "Sample config downloaded to /root/sentryusb.conf"
    else
        # Fallback minimal template if offline/download fails.
        cat > /root/sentryusb.conf << 'CONFEOF'
# SentryUSB Configuration
# Edit these values and run setup from the web UI.
#
# Required:
export CAM_SIZE=30G
#export MUSIC_SIZE=4G
#export LIGHTSHOW_SIZE=1G
#export BOOMBOX_SIZE=100M

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
        ok "Sample config created at /root/sentryusb.conf (offline fallback)"
    fi
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
for _ in $(seq 1 30); do
    IP=$(hostname -I 2>/dev/null | awk '{print $1}')
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
echo -e "  Binary:  ${INSTALL_DIR}/sentryusb-current → $(readlink "${INSTALL_DIR}/sentryusb-current" 2>/dev/null || echo "<picker has not run yet>")"
echo -e "  Logs:    journalctl -u sentryusb -f"
echo ""

if [ "${NEEDS_REBOOT:-0}" = "1" ]; then
    warn "Rock 4C+: a REBOOT is required to activate the USB gadget (dwc3 → peripheral)"
    warn "          and load the BT/WiFi firmware."
    echo -e "  Run:  ${BLUE}reboot${NC}  — afterward /sys/class/udc/ shows fe800000.usb (Tesla"
    echo -e "        sees the dashcam) and SC can discover + BLE-pair the 4C+."
    echo ""
fi
