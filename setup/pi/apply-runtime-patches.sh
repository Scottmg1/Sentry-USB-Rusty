#!/bin/bash
# sentryusb-apply-runtime-patches.sh
#
# Idempotent re-application of all install-time patches that must survive
# a binary OTA update. Called by:
#   - install-pi.sh        — initial install / re-install via curl
#   - crates/api/src/update.rs — after every in-app binary swap
#
# Why this exists: the in-app updater (Settings → System → Check for
# Updates) only swaps the Rust binary. It does NOT re-run install-pi.sh.
# So install-time fixes (BLE non-fatal-adv on BCM4345C0, etc.) that are
# applied to shipped scripts on disk silently rot the moment a release
# replaces those scripts — leaving every existing 4C+ user with a
# crash-looped Bluetooth stack after their first update.
#
# This script is the bridge: it re-applies the patches every time the
# updater runs, so existing installs heal automatically on update without
# needing a re-install.
#
# Detection-gated: each patch's apply-block self-checks for the board /
# precondition it cares about, so running on a Pi 4 or Pi 5 (or amd64
# dev box) is a no-op.
#
# Safe to re-run anytime: every patch first checks if the marker is
# already present in the target file.

set -u

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

log()  { echo -e "${GREEN}[patches]${NC} $1"; }
warn() { echo -e "${YELLOW}[patches]${NC} $1" >&2; }
err()  { echo -e "${RED}[patches]${NC} $1" >&2; }

# ── Detection helpers ────────────────────────────────────────────────────

is_rock_4cplus() {
    grep -qai 'rock-4c-plus\|rockpi4c-plus\|ROCK 4C+' \
        /proc/device-tree/model /proc/device-tree/compatible 2>/dev/null
}

# Known-affected Broadcom chips where BlueZ's extended advertising fails OR
# defaults to non-connectable parameters — i.e., where SC's BLE pair fails
# without our raw-HCI ADV_IND helper. Detected by parsing the chip family ID
# kernel logs on first BT probe (e.g. "Bluetooth: hci0: BCM43430B0 (002.001.012)").
#
# Currently:
#   BCM4345C0 — Rock 4C+ (confirmed broken via field evidence)
#   BCM43430B0 — Pi Zero 2 W (confirmed broken via btmon trace 2026-06-20)
#   BCM43438 — Pi 3B/3B+, Pi Zero W (same chip family / same firmware tree)
#
# DELIBERATELY EXCLUDED until tested:
#   BCM43455 / CYW43455 — Pi 4 / Pi 5; their modern bluetoothd path is
#   reported to work fine, and running our raw-HCI helper there would
#   override their working ext-adv with legacy adv (regression). If a Pi
#   4/5 user does hit "GATT 147 bond=BOND_NONE" they can opt in with:
#       sudo touch /mutable/force-ble-adv-helper
#   That sentinel forces install regardless of chip detection. The next OTA
#   (or `sudo /usr/local/bin/sentryusb-apply-runtime-patches`) lands it.
is_known_broken_ble_chip() {
    # Operator override — for chips we haven't detection-listed yet but
    # field-confirmed need the helper.
    [ -f /mutable/force-ble-adv-helper ] && { log "BLE adv: /mutable/force-ble-adv-helper present — forcing install"; return 0; }
    local chips="BCM4345C0\|BCM43430B0\|BCM43438"
    dmesg 2>/dev/null | grep -qE "hci0: ($chips)" && return 0
    # dmesg may not retain that line on a long-running box; also check the
    # board model as a backstop (4C+'s 4345C0 + Zero 2 W's 43430B0 are
    # board-specific so model match is unambiguous).
    grep -qai 'rock-4c-plus\|rockpi4c-plus\|ROCK 4C+\|Raspberry Pi Zero 2 W\|Raspberry Pi 3 Model B\|Raspberry Pi Zero W' \
        /proc/device-tree/model 2>/dev/null && return 0
    return 1
}

# ── BLE non-fatal-adv patch (all Broadcom Pi-family chips) ──────────────
#
# Broadcom Pi-family chips (BCM4345C0 on Rock 4C+, BCM43430B0 on Pi Zero 2 W,
# the BCM43455 sibling on Pi 4/Compute Module, etc.) all reject BlueZ's
# extended advertising with "Invalid Parameters 0x0d". The shipped
# sentryusb-ble.py calls sys.exit(1) on that error, which tears down GATT
# and lets systemd re-spawn the daemon in a fast crash loop. The Pi's actual
# advertising is handled out-of-band by sentryusb-ble-adv.service via raw
# HCI (ADV_IND programmed directly), so the BlueZ failure is legitimately
# non-fatal — we just need the GATT server to stay up. Patch swallows the
# BlueZ adv error and logs it instead.
#
# Was 4C+-gated through v3.11.7; widened to all Pi families in v3.11.8.
apply_ble_nonfatal_adv() {
    local f=/root/bin/sentryusb-ble.py
    [ -f "$f" ] || { warn "BLE: $f missing — skipping non-fatal-adv patch"; return 0; }

    if grep -q 'legacy btmgmt advertising' "$f"; then
        log "BLE non-fatal-adv: already patched"
        return 0
    fi

    # Make root RW for the write (no-op if already RW). Shipped by
    # install-pi.sh; safe to call here.
    [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true

    # AST-aware Python patcher: surgically replaces register_ad_error_cb.
    local result
    result="$(python3 - "$f" 2>&1 <<'PYEOF'
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
)" || result="python-error"

    if [ "$result" = "patched" ] && grep -q 'legacy btmgmt advertising' "$f"; then
        log "BLE non-fatal-adv: applied via Python patcher"
    else
        warn "BLE non-fatal-adv: Python path failed ($result), trying sed fallback"
        # sed fallback rewrites register_ad_error_cb body line by line
        sed -i '/^def register_ad_error_cb(error):$/,/^def register_app_cb/{
            /^def register_ad_error_cb(error):$/!{
                /^def register_app_cb/!d
            }
        }' "$f"
        sed -i '/^def register_ad_error_cb(error):$/a\    log.warning(f"BlueZ advertisement registration failed ({error}); using legacy btmgmt advertising instead; GATT stays up.")\n' "$f"
        if grep -q 'legacy btmgmt advertising' "$f"; then
            log "BLE non-fatal-adv: applied via sed fallback"
        else
            err  "BLE non-fatal-adv: BOTH patch paths failed — SC discovery may be broken on this install"
            return 1
        fi
    fi

    # Restart the daemon so the patched version takes effect immediately
    # rather than waiting for the next reboot. reset-failed clears any
    # crash-loop backoff from the broken pre-patch state.
    systemctl reset-failed sentryusb-ble.service 2>/dev/null || true
    systemctl restart sentryusb-ble.service 2>/dev/null || true
    return 0
}

# ── EATT disable (all Pi boards) ────────────────────────────────────────
#
# Our BLE GATT is app-PIN over plain (unencrypted) ATT. Android (esp. 14+)
# opens EATT (PSM 0x0027) on connect, which bluetoothd refuses without an
# encrypted link and answers with an SMP Security Request — popping an OS
# pair prompt on every connect (or, on some phones, a silent GATT 147 /
# "Connection lost" tear-down loop with bond=BOND_NONE).
#
# Channels=1 keeps plain ATT (same GATT, same PIN), no prompt, no tear-down.
# Safe on every Pi board — no security change vs. our existing model.
# Universal patch (no board gate): pre-v3.11.x installs (e.g. v3.9.0 Zero 2W)
# never ran the install-time version of this, so OTA must heal it for them.
apply_eatt_disable() {
    local conf=/etc/bluetooth/main.conf
    [ -f "$conf" ] || { warn "EATT: $conf missing — skipping"; return 0; }

    if grep -qE '^Channels[[:space:]]*=[[:space:]]*1' "$conf"; then
        log "EATT disable: already applied"
        return 0
    fi

    if grep -qE '^\[GATT\]' "$conf"; then
        if grep -qiE '^[# ]*Channels' "$conf"; then
            sed -i -E 's/^[# ]*Channels[ ]*=.*/Channels = 1/' "$conf"
        else
            sed -i '/^\[GATT\]/a Channels = 1' "$conf"
        fi
    else
        printf '\n[GATT]\nChannels = 1\n' >> "$conf"
    fi

    if grep -qE '^Channels[[:space:]]*=[[:space:]]*1' "$conf"; then
        log "EATT disable: applied to $conf"
        systemctl restart bluetooth 2>/dev/null || true
    else
        err "EATT disable: write to $conf failed (read-only fs? check remountfs_rw)"
        return 1
    fi
    return 0
}

# ── BLE legacy-advertising helper install (all Broadcom Pi-family chips) ──
#
# Fresh installs get these files from install-pi.sh; this function brings
# existing v3.11.7-and-earlier installs up to parity. Idempotent — each file
# is only written when missing OR when the on-disk contents differ from the
# current upstream version.
#
# Files installed:
#   /usr/local/bin/sentryusb-ble-adv.sh
#   /etc/systemd/system/sentryusb-ble-adv.service
#   /etc/udev/rules.d/99-sentryusb-ble-hci.rules
#   /etc/systemd/system/sentryusb-ble.service.d/wants-bluetooth.conf
apply_ble_adv_helper() {
    # Gate to known-affected chips so Pi 4/5 (where bluetoothd's modern
    # ext-adv works) don't get the raw-HCI helper overriding their good
    # advertising. See is_known_broken_ble_chip above for the full list.
    is_known_broken_ble_chip || { log "BLE adv: chip not in known-broken list — skipping helper install"; return 0; }
    local repo="${REPO:-Sentry-Six/Sentry-USB-Rusty}"
    local base="https://raw.githubusercontent.com/${repo}/main/setup/pi"
    local changed=0

    install_one() {
        # $1 = source filename, $2 = destination path, $3 = mode
        local src="$1" dst="$2" mode="$3"
        local tmp; tmp="$(mktemp)" || { warn "BLE adv: mktemp failed"; return 1; }
        if ! curl -fsSL --max-time 15 "$base/$src" -o "$tmp" 2>/dev/null; then
            rm -f "$tmp"
            warn "BLE adv: failed to fetch $src — leaving any existing copy alone"
            return 1
        fi
        if [ -f "$dst" ] && cmp -s "$tmp" "$dst"; then
            rm -f "$tmp"
            return 0  # already up to date
        fi
        [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true
        install -m "$mode" "$tmp" "$dst"
        rm -f "$tmp"
        changed=1
        log "BLE adv: installed/refreshed $dst"
    }

    install_one sentryusb-ble-adv.sh /usr/local/bin/sentryusb-ble-adv.sh 755 || return 0
    install_one sentryusb-ble-adv.service /etc/systemd/system/sentryusb-ble-adv.service 644
    install_one 99-sentryusb-ble-hci.rules /etc/udev/rules.d/99-sentryusb-ble-hci.rules 644
    mkdir -p /etc/systemd/system/sentryusb-ble.service.d
    install_one sentryusb-ble-wants-bluetooth.conf \
                /etc/systemd/system/sentryusb-ble.service.d/wants-bluetooth.conf 644

    if [ "$changed" = "1" ]; then
        systemctl daemon-reload 2>/dev/null || true
        udevadm control --reload-rules 2>/dev/null || true
        systemctl enable sentryusb-ble-adv.service >/dev/null 2>&1 || true
        systemctl restart sentryusb-ble-adv.service 2>/dev/null || true
        log "BLE adv: service enabled + restarted"
    else
        log "BLE adv: all files current, nothing to do"
    fi
    return 0
}

# ── Run all patches ─────────────────────────────────────────────────────

apply_ble_nonfatal_adv
apply_ble_adv_helper
apply_eatt_disable

# Future patches that must survive an OTA update get appended here. Each
# one self-checks board / precondition / marker so the whole script stays
# a safe no-op on non-applicable systems.
