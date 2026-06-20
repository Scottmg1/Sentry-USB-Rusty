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

# ── BLE non-fatal-adv patch (Rock 4C+ / BCM4345C0) ──────────────────────
#
# The BCM4345C0 firmware rejects BlueZ extended advertising with "Invalid
# Parameters (0x0d)". The shipped sentryusb-ble.py calls sys.exit(1) on
# that error, which tears down GATT and lets systemd re-spawn the daemon
# in a fast crash loop. The Pi's actual advertising is handled out-of-band
# by sentryusb-ble-adv.service via raw HCI, so the BlueZ failure is
# legitimately non-fatal for our use case — we just need the GATT server
# to stay up. Patch swallows the BlueZ adv error and logs it instead.
apply_ble_nonfatal_adv() {
    is_rock_4cplus || return 0
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
            err  "BLE non-fatal-adv: BOTH patch paths failed — SC discovery may be broken on this 4C+ install"
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

# ── Run all patches ─────────────────────────────────────────────────────

apply_ble_nonfatal_adv

# Future patches that must survive an OTA update get appended here. Each
# one self-checks board / precondition / marker so the whole script stays
# a safe no-op on non-applicable systems.
