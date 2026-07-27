#!/bin/bash
# Legacy ADV_IND advertiser for Broadcom Pi-family chips where BlueZ's modern
# advertising path is unreliable (the BCM controllers reject EXTENDED
# advertising, and even `btmgmt advertising on` defaults to non-connectable
# parameters on some chip revisions). Programs advertising directly over raw
# HCI as connectable undirected (ADV_IND) at 100ms intervals.
#
# The SentryUSB apps filter scans by the "SentryUSB-" name prefix, so the
# local name MUST appear in the scan response (BlueZ doesn't include the
# name in this path by default, hence the explicit scan-rsp builder below).
# Fresh flag = a central connect is in flight; don't re-assert advertising then.
CONNECTING_FLAG="/tmp/ble_connecting"
CONNECTING_FLAG_MAX_AGE=15   # seconds; ignore a stale flag a crashed connect left behind

UUID_LE="9e ca dc 24 0e e5 a9 e0 93 f3 a3 b5 01 00 40 6e"   # 6e400001-b5a3-f393-e0a9-e50e24dcca9e, little-endian
ADV_DATA="15 02 01 06 11 07 ${UUID_LE} 00 00 00 00 00 00 00 00 00 00 00 00 00"

# Advertise on the adapter GATT uses: BLE_ADAPTER=hciN from sentryusb.conf,
# default hci0. Normalized like ble.py's read_ble_adapter_from_config (first
# line wins; "hciN" or bare "N" -> hciN; else hci0).
_cfg=$(sed -n 's/^[[:space:]]*BLE_ADAPTER[[:space:]]*=[[:space:]]*//p' /root/sentryusb.conf 2>/dev/null | head -1)
# Match ble.py's value.strip().strip('"').strip("'").strip(): trim edge
# whitespace, then all edge " then all edge ' (each independently), then edge ws.
_cfg=${_cfg%"${_cfg##*[![:space:]]}"}
while case "$_cfg" in \"*) true ;; *) false ;; esac; do _cfg=${_cfg#\"}; done
while case "$_cfg" in *\") true ;; *) false ;; esac; do _cfg=${_cfg%\"}; done
while case "$_cfg" in \'*) true ;; *) false ;; esac; do _cfg=${_cfg#\'}; done
while case "$_cfg" in *\') true ;; *) false ;; esac; do _cfg=${_cfg%\'}; done
_cfg=${_cfg%"${_cfg##*[![:space:]]}"}   # trailing whitespace
_cfg=${_cfg#"${_cfg%%[![:space:]]*}"}   # leading whitespace (ble.py's final .strip())
case "$_cfg" in
    hci[0-9]*) HCI="$_cfg" ;;
    [0-9]*)    HCI="hci$_cfg" ;;
    *)         HCI=hci0 ;;
esac
IDX="${HCI#hci}"; case "$IDX" in ''|*[!0-9]*) HCI=hci0; IDX=0 ;; esac

# Scan-response bytes = [len][0x09 Complete Local Name][name…]. Function is
# defined before the run-guard so it's sourceable for unit-style testing.
build_scanrsp() {
    local name hex namebytes adlen siglen pad i
    # `printf '' |` closes stdin — under systemd a bare `btmgmt info` busy-loops
    # on /dev/null and returns empty (→ hostname fallback). Closed pipe = EOF.
    name=$(printf '' | timeout 5 btmgmt --index "$IDX" info 2>/dev/null | sed -n 's/^[[:space:]]*name[[:space:]]*//p' | head -1)
    [ -z "$name" ] && name=$(hostname)
    name=${name:0:29}                                   # scan-rsp budget: 31 - 2 (len+type)
    hex=$(printf '%s' "$name" | od -An -tx1 | tr -d ' \n')
    namebytes=$(( ${#hex} / 2 ))
    adlen=$(( namebytes + 1 ))                           # AD length: type byte + name
    siglen=$(( adlen + 1 ))                              # significant bytes in the 31-byte field
    pad=$(( 31 - siglen ))                               # zero-fill the rest of the 31-byte field
    # LE Set Scan Response Data needs EXACTLY 32 bytes: [siglen][31-byte data].
    # Zero-pad by name length — a fixed pad overruns and strict controllers reject it.
    printf '%02x %02x 09 %s' "$siglen" "$adlen" "$(echo "$hex" | sed 's/../& /g')"
    local i; for (( i = 0; i < pad; i++ )); do printf ' 00'; done
}

# Program legacy ADV_IND directly via HCI (bypasses BlueZ mgmt):
#   disable → set adv data → set scan-rsp (name, zero-padded to 31) →
#   adv params (100ms min/max ADV_IND connectable undirected, public addr, all 3 chans) → enable
# The "0x00" after the intervals is the advertising-type byte — ADV_IND
# (connectable). DO NOT change to ADV_SCAN_IND (0x02) — the chip will then
# silently refuse incoming GATT connect requests, surfacing as the phone
# logging "GATT 147 bond=BOND_NONE" 10s into the attempt.
assert_raw_adv() {
    local scanrsp; scanrsp=$(build_scanrsp)
    hcitool -i "$HCI" cmd 0x08 0x000a 00 >/dev/null 2>&1 || true
    hcitool -i "$HCI" cmd 0x08 0x0008 $ADV_DATA >/dev/null 2>&1 || true
    # build_scanrsp already emits the full 32 bytes (sig-length + 31-byte data,
    # zero-padded) — do NOT append more zeros here or the param exceeds 32.
    hcitool -i "$HCI" cmd 0x08 0x0009 $scanrsp >/dev/null 2>&1 || true
    hcitool -i "$HCI" cmd 0x08 0x0006 a0 00 a0 00 00 00 00 00 00 00 00 00 00 07 00 >/dev/null 2>&1 || true
    # Re-check right before enabling so we never re-arm advertising mid-connect.
    connect_in_flight && return 0
    hcitool -i "$HCI" cmd 0x08 0x000a 01 >/dev/null 2>&1 || true
}

# True while a central connect is in flight (flag fresh within max-age).
connect_in_flight() {
    local mtime now age
    mtime=$(stat -c %Y "$CONNECTING_FLAG" 2>/dev/null) || return 1
    now=$(date +%s)
    age=$(( now - mtime ))
    [ "$age" -ge 0 ] && [ "$age" -lt "$CONNECTING_FLAG_MAX_AGE" ]
}

# True only when a central (a phone) is connected TO us — i.e. we hold a
# PERIPHERAL-role LE link. The Tesla keep-awake link is the OPPOSITE: the Pi
# dials out to the car, so it is a CENTRAL-role link. That link must NOT
# suppress advertising, otherwise the Pi is undiscoverable to the phone the
# whole time keep-awake holds the car connection (the in-app "connect over
# Bluetooth" step then fails). The controller does advertising + an outbound
# central link simultaneously without issue (field-verified: asserting adv
# while the car link is up left the car link intact). hcitool reports the
# LOCAL role as "lm PERIPHERAL" (older BlueZ: "lm SLAVE").
peripheral_link_up() {
    hcitool -i "$HCI" con 2>/dev/null | grep -qiE 'lm (PERIPHERAL|SLAVE)'
}

# Run the advertising loop ONLY when executed directly (the systemd ExecStart),
# never when sourced — otherwise testing build_scanrsp() would loop forever.
if [ "${BASH_SOURCE[0]}" != "${0}" ]; then
    return 0 2>/dev/null || exit 0
fi

# Advertising-mode gate: on boards RF-qualified to advertise natively,
# bluetoothd owns advertising and this helper must stand down or the two fight
# over the controller. select-ble-adv-mode.sh writes the marker each boot;
# absence means "helper" (fail-safe), so a missing marker still advertises.
if [ "$(cat /run/sentryusb/ble-adv-mode 2>/dev/null)" = "native" ]; then
    echo "sentryusb-ble-adv: native mode — bluetoothd owns advertising, standing down"
    exit 0
fi

for i in $(seq 1 30); do busctl status org.bluez >/dev/null 2>&1 && break; sleep 1; done
sleep 3   # let bluetoothd's (failing) ext-adv RegisterAdvertisement settle first
timeout 5 btmgmt --index "$IDX" le on >/dev/null 2>&1 || true
timeout 5 btmgmt --index "$IDX" connectable on >/dev/null 2>&1 || true
while true; do
    # Re-assert while IDLE: skip only while a phone connect is in flight, or a
    # phone is already connected (a PERIPHERAL-role link). Do NOT skip merely
    # because an LE connection exists — the Tesla keep-awake is a CENTRAL-role
    # link and must not silence advertising, or the phone can never discover us
    # while the car is awake.
    if ! connect_in_flight && ! peripheral_link_up; then
        assert_raw_adv
    fi
    sleep 5
done
