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
UUID_LE="9e ca dc 24 0e e5 a9 e0 93 f3 a3 b5 01 00 40 6e"   # 6e400001-b5a3-f393-e0a9-e50e24dcca9e, little-endian
ADV_DATA="15 02 01 06 11 07 ${UUID_LE} 00 00 00 00 00 00 00 00 00 00 00 00 00"

# Scan-response bytes = [len][0x09 Complete Local Name][name…]. Function is
# defined before the run-guard so it's sourceable for unit-style testing.
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
# The "0x00" after the intervals is the advertising-type byte — ADV_IND
# (connectable). DO NOT change to ADV_SCAN_IND (0x02) — the chip will then
# silently refuse incoming GATT connect requests, surfacing as the phone
# logging "GATT 147 bond=BOND_NONE" 10s into the attempt.
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
sleep 3   # let bluetoothd's (failing) ext-adv RegisterAdvertisement settle first
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
