#!/bin/bash
# BLE advertising-mode selector — once per boot, writes /run/.../ble-adv-mode
# (native|helper) read by sentryusb-ble.py + the helper. Positive allow-list
# against ble-native-manifest (a controller probe can't classify — it reports
# host-side success while radiating nothing). Read-only, safe pre-BLE-services.
# Precedence: force-ble-adv-helper > force-ble-adv-native > manifest > helper.
set -u

MARKER_DIR=/run/sentryusb
MARKER="$MARKER_DIR/ble-adv-mode"        # ephemeral: "native" | "helper"
LAST=/mutable/ble-adv-gate.last          # human-readable audit trail
FORCE_HELPER=/mutable/force-ble-adv-helper
FORCE_NATIVE=/mutable/force-ble-adv-native
MANIFEST="${BLE_NATIVE_MANIFEST:-/usr/local/share/sentryusb/ble-native-manifest}"

log() { echo "ble-adv-gate: $*"; }

mkdir -p "$MARKER_DIR" 2>/dev/null

model="$(tr -d '\0' </proc/device-tree/model 2>/dev/null)"
kernel="$(uname -r)"
btd="$(command -v bluetoothd 2>/dev/null)"
btd_sha="$( [ -n "$btd" ] && sha256sum "$btd" 2>/dev/null | cut -c1-16 )"

decide() {
    # 1) operator overrides win over everything, helper before native.
    if [ -f "$FORCE_HELPER" ]; then echo "helper force-helper"; return; fi
    if [ -f "$FORCE_NATIVE" ]; then echo "native force-native"; return; fi

    # 2) positive manifest match: model-substring AND kernel-family-substring.
    if [ -r "$MANIFEST" ]; then
        while IFS= read -r line; do
            case "$line" in ''|\#*) continue;; esac
            local msub ksub
            msub="${line%%|*}"
            ksub="${line#*|}"
            [ -z "$msub" ] || [ -z "$ksub" ] && continue
            case "$model" in *"$msub"*)
                case "$kernel" in *"$ksub"*)
                    echo "native manifest:$msub|$ksub"; return;;
                esac;;
            esac
        done < "$MANIFEST"
    else
        log "manifest $MANIFEST unreadable — defaulting to helper"
    fi

    # 3) fail-safe.
    echo "helper default"
}

read -r mode reason <<<"$(decide)"
[ -z "${mode:-}" ] && { mode=helper; reason=error-fallback; }   # never leave undecided

# Atomic write; on failure fall back to remove-then-overwrite-helper so a stale
# 'native' can't survive (absence => helper). Loud CRITICAL if /run is unwritable.
tmp="$MARKER.tmp.$$"
if printf '%s\n' "$mode" > "$tmp" 2>/dev/null && mv -f "$tmp" "$MARKER" 2>/dev/null; then
    :
else
    rm -f "$tmp" 2>/dev/null
    if rm -f "$MARKER" 2>/dev/null || printf 'helper\n' > "$MARKER" 2>/dev/null; then
        log "marker write failed — forced fail-safe (absent or helper)"
    else
        log "CRITICAL: cannot write OR clear $MARKER — /run unwritable; a stale mode may persist until reboot"
        exit 1
    fi
fi

log "mode=$mode reason=$reason model='$model' kernel='$kernel' btd_sha=${btd_sha:-?}"
# Best-effort audit copy on /mutable (never fatal on RO / missing mount).
{ printf 'mode=%s reason=%s model=%s kernel=%s btd_sha=%s\n' \
    "$mode" "$reason" "$model" "$kernel" "${btd_sha:-?}" > "$LAST"; } 2>/dev/null || true

exit 0
