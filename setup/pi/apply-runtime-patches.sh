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

# ── bfq scheduler on the backingfiles disk (all boards) ─────────────────
#
# The archive pipeline (rsync reads, snapshot cp) now runs under
# `ionice -c2 -n7` so the car's dashcam writes through the USB gadget
# always win disk access — but ionice only has effect under the bfq I/O
# scheduler (mq-deadline, the Pi OS default, ignores I/O priorities).
# Ship a udev rule so every sd disk gets bfq at hotplug/boot, and apply
# it to the live backingfiles disk immediately when that is safe.
apply_backingfiles_bfq() {
    local rule=/etc/udev/rules.d/60-sentryusb-bfq.rules
    local want='ACTION=="add|change", KERNEL=="sd[a-z]", SUBSYSTEM=="block", ATTR{queue/scheduler}="bfq"'

    modprobe bfq 2>/dev/null || true

    if [ ! -f "$rule" ] || [ "$(cat "$rule" 2>/dev/null)" != "$want" ]; then
        [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true
        if printf '%s\n' "$want" > "$rule" 2>/dev/null; then
            udevadm control --reload-rules 2>/dev/null || true
            log "bfq: installed $rule"
        else
            err "bfq: failed to write $rule (read-only fs? check remountfs_rw)"
        fi
    else
        log "bfq: udev rule already current"
    fi

    # Apply to the running system now — but only while the USB gadget is
    # NOT bound. Switching the elevator drains the disk's request queue,
    # which can briefly stall the car's in-flight dashcam writes — the very
    # SCSI-timeout drive-drop this patch exists to prevent. This script runs
    # mid-OTA while the car may be recording; when the gadget is bound, the
    # udev rule simply takes effect at the next boot instead.
    if [ -n "$(cat /sys/kernel/config/usb_gadget/sentryusb/UDC 2>/dev/null)" ]; then
        log "bfq: gadget is presented to the car — deferring live scheduler switch to next boot (udev rule covers it)"
        return 0
    fi
    # Resolve the disk backing /backingfiles (e.g. /dev/sda2 -> sda)
    # rather than assuming sda.
    local src disk sched
    src="$(findmnt -n -o SOURCE /backingfiles 2>/dev/null)" || true
    [ -n "${src:-}" ] || { log "bfq: /backingfiles not mounted — udev rule will cover next boot"; return 0; }
    disk="$(lsblk -n -o PKNAME "$src" 2>/dev/null | head -1)"
    [ -n "$disk" ] || disk="$(basename "$src" | sed 's/[0-9]*$//')"
    sched="/sys/block/$disk/queue/scheduler"
    if [ -w "$sched" ]; then
        if grep -q '\[bfq\]' "$sched"; then
            log "bfq: already active on $disk"
        elif echo bfq > "$sched" 2>/dev/null; then
            log "bfq: activated on $disk"
        else
            warn "bfq: could not activate on $disk (kernel without bfq?) — ionice will be a no-op"
        fi
    fi
    return 0
}

# ── systemd hardware watchdog (all boards) ──────────────────────────────
#
# journald on these installs is volatile, so a full kernel hang leaves the
# car with a dead drive indefinitely AND destroys the evidence. With the
# hardware watchdog armed, a hung kernel becomes a ~15s reboot and the
# gadget re-presents ~90s later. 15s is within the BCM283x/BCM2712
# watchdog hardware maximum (~15.9s). Userspace-only wedges don't trip
# this (systemd itself pets the watchdog) — it is strictly kernel-hang
# protection.
apply_hardware_watchdog() {
    local dropin_dir=/etc/systemd/system.conf.d
    local dropin=$dropin_dir/10-sentryusb-watchdog.conf
    local want='[Manager]
RuntimeWatchdogSec=15'

    if [ -f "$dropin" ] && [ "$(cat "$dropin" 2>/dev/null)" = "$want" ]; then
        log "watchdog: drop-in already current"
        return 0
    fi
    [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true
    mkdir -p "$dropin_dir" 2>/dev/null || true
    if printf '%s\n' "$want" > "$dropin" 2>/dev/null; then
        # Deliberately no `systemctl daemon-reexec` here: this script runs
        # mid-OTA, and re-executing PID 1 (and arming a 15s hardware
        # watchdog) at that moment adds risk for zero benefit — these boxes
        # reboot at least daily (car power), so the watchdog arms at the
        # next boot.
        log "watchdog: RuntimeWatchdogSec=15 installed (arms at next boot)"
    else
        err "watchdog: failed to write $dropin (read-only fs? check remountfs_rw)"
    fi
    return 0
}

# ── Archive mount lock (CIFS/NFS connect/disconnect scripts) ────────────
#
# The API's backup path and archiveloop now coordinate /mnt/archive
# ownership via a shared flock (/tmp/sentryusb_archive_mount.lock — see
# crates/api/src/archive_mount_lock.rs). The lock-aware connect/
# disconnect-archive.sh only land on disk at setup-wizard time
# (crates/setup/src/archive.rs bakes them into the binary), so existing
# CIFS/NFS installs need this refresh or archiveloop keeps running the
# lock-free scripts and the coordination is one-sided.
#
# The heredocs below MUST stay byte-identical to
# run/cifs_archive/{connect,disconnect}-archive.sh (the nfs copies are
# the same files).
apply_archive_mount_lock_scripts() {
    # Only CIFS/NFS archives mount /mnt/archive from fstab; rsync/rclone
    # (and archiveless) installs have nothing to lock.
    if ! grep -qE '[[:space:]]/mnt/archive[[:space:]]+(cifs|nfs)[[:space:]]' /etc/fstab 2>/dev/null; then
        log "archive-mount-lock: no CIFS/NFS /mnt/archive fstab entry — not applicable"
        return 0
    fi
    if grep -q 'ARCHIVE_MOUNT_LOCK' /root/bin/connect-archive.sh 2>/dev/null \
       && grep -q 'ARCHIVE_MOUNT_LOCK' /root/bin/disconnect-archive.sh 2>/dev/null; then
        log "archive-mount-lock: already patched"
        return 0
    fi
    [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true

    # Staged + atomic rename: a power loss or disk-full mid-write must
    # never leave a truncated live script (archiveloop may invoke these
    # at any moment, and a half-written file containing the marker would
    # make the next patch run report "already patched").
    cat > /root/bin/connect-archive.sh.new <<'CONNECT_EOF'
#!/bin/bash -eu

# Must match ARCHIVE_MOUNT_LOCK_PATH in crates/api/src/archive_mount_lock.rs
# and disconnect-archive.sh.
ARCHIVE_MOUNT_LOCK=/tmp/sentryusb_archive_mount.lock

function mount_if_set() {
  local mount_point=$1
  [ -z "$mount_point" ] || ensure_mountpoint_is_mounted_with_retry "$mount_point"
}

# The archive mount is shared with the API's backup path, which may
# mount /mnt/archive itself for a Backup Now and unmount it when done.
# Take the shared flock around the transition so we can't adopt a
# backup-owned mount that's about to be unmounted from under us. The
# API holds the lock for its whole mount+write+unmount (bounded well
# under the wait here). Fail-closed on lock timeout: mounting without
# the lock reopens the adoption race, and archiveloop already handles a
# failed connect by skipping the cycle and retrying next time.
function mount_archive_locked() {
  local mount_point=$1
  [ -z "$mount_point" ] && return 0
  (
    if ! flock -w 300 210
    then
      log "Archive mount lock busy for 300s — failing archive connect (retried next cycle)."
      exit 1
    fi
    ensure_mountpoint_is_mounted_with_retry "$mount_point"
  ) 210>"$ARCHIVE_MOUNT_LOCK"
}

mount_archive_locked "${ARCHIVE_MOUNT:-}"
mount_if_set "${MUSIC_ARCHIVE_MOUNT:-}"
CONNECT_EOF

    cat > /root/bin/disconnect-archive.sh.new <<'DISCONNECT_EOF'
#!/bin/bash -eu

# Unmount the archive. Without this, the archive mounts can get into a
# state where the archive is reachable via the network, appears to be
# mounted, but the mount is inoperable and any attempt to access it
# results in a "host is down" message.

# Must match ARCHIVE_MOUNT_LOCK_PATH in crates/api/src/archive_mount_lock.rs
# and connect-archive.sh.
ARCHIVE_MOUNT_LOCK=/tmp/sentryusb_archive_mount.lock

unmount_if_set() {
  local mount_point=$1
  if [ -n "$mount_point" ]
  then
    if findmnt --mountpoint "$mount_point" > /dev/null
    then
      if timeout 10 umount -f -l "$mount_point" >> "$LOG_FILE" 2>&1
      then
        log "Unmounted $mount_point."
      else
        log "Failed to unmount $mount_point."
      fi
    else
      log "$mount_point already unmounted."
    fi
  fi
}

# Archive unmount runs in the FOREGROUND under the shared flock, so an
# in-flight API backup (which holds the lock across its mount+write)
# can't have the mount force-lazy-unmounted mid-write. Bounded: the
# umount itself is capped at 10s and the lock wait at 300s, so this
# can't wedge the return to archiveloop the way an uncapped unmount
# once could. Fail-closed on lock timeout: unmounting without the lock
# is exactly the mid-write teardown the lock exists to prevent — skip,
# and the next cycle's disconnect gets another chance. Music has no
# API writer, so it keeps the old backgrounded, lock-free path.
(
  if ! flock -w 300 210
  then
    log "Archive mount lock busy for 300s — skipping archive unmount this cycle."
    exit 0
  fi
  unmount_if_set "${ARCHIVE_MOUNT:-}"
) 210>"$ARCHIVE_MOUNT_LOCK"
unmount_if_set "${MUSIC_ARCHIVE_MOUNT:-}" &
DISCONNECT_EOF

    chmod 755 /root/bin/connect-archive.sh.new /root/bin/disconnect-archive.sh.new
    if ! bash -n /root/bin/connect-archive.sh.new || ! bash -n /root/bin/disconnect-archive.sh.new; then
        err "archive-mount-lock: staged scripts failed bash -n — keeping existing scripts"
        rm -f /root/bin/connect-archive.sh.new /root/bin/disconnect-archive.sh.new
        return 1
    fi
    # The && marker check above heals a power loss between the renames.
    mv /root/bin/connect-archive.sh.new /root/bin/connect-archive.sh
    mv /root/bin/disconnect-archive.sh.new /root/bin/disconnect-archive.sh
    log "archive-mount-lock: lock-aware connect/disconnect-archive.sh installed"
}

# ── WiFi rfkill unblock — ROCK 4C+ only ─────────────────────────────────
#
# On the 4C+ the WLAN radio boots soft-blocked. systemd-rfkill would normally
# restore last boot's unblocked state, but make-root-fs-readonly puts
# /var/lib/systemd/rfkill on tmpfs (so a stale block can't be restored) — which
# also means the UNBLOCKED state has nothing to restore from. WiFi therefore
# stays dead on every boot after setup, locking out WiFi-only users. Bluetooth
# already has an equivalent boot service (BLE / Tesla key); this is the WiFi
# twin. Existing installs can't get it from the setup scripts (those only run
# at install time), so heal them here.
#
# Opt out on a board that should stay radio-silent:
#   echo off > /etc/sentryusb/wifi-radio
apply_rfkill_unblock_wifi() {
    is_rock_4cplus || return 0

    local unit=/etc/systemd/system/rfkill-unblock-wifi.service
    local helper=/usr/local/sbin/sentryusb-unblock-wifi

    # Marker: unit installed and pointing at an executable helper.
    if grep -q "$helper" "$unit" 2>/dev/null && [ -x "$helper" ]; then
        return 0
    fi

    # Root is read-only on a deployed unit: unlock, write, then lock back.
    # Leaving root writable on a device that loses power when the car sleeps
    # is what the read-only root exists to prevent.
    local ro_before=no
    findmnt -no OPTIONS / 2>/dev/null | grep -qE '(^|,)ro(,|$)' && ro_before=yes
    if [ "$ro_before" = yes ]; then
        [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 \
            || mount -o remount,rw / 2>/dev/null || true
    fi
    _relock() {
        [ "$ro_before" = yes ] || return 0
        sync
        mount -o remount,ro / 2>/dev/null || true
    }

    mkdir -p /usr/local/sbin /etc/systemd/system 2>/dev/null || true

    cat > "$helper" << 'WIFIHELPER'
#!/bin/sh
# Managed by SentryUSB (ROCK 4C+). Unblocks the WiFi radio at boot.
# To keep this board radio-silent:  echo off > /etc/sentryusb/wifi-radio
set -u

OVERRIDE=/etc/sentryusb/wifi-radio
if [ -r "$OVERRIDE" ]; then
  case "$(tr -d '[:space:]' < "$OVERRIDE" | tr '[:upper:]' '[:lower:]')" in
    off|0|false|disabled)
      exit 0 ;;
  esac
fi

i=0
while [ "$i" -lt 25 ]; do
  for d in /sys/class/rfkill/rfkill*; do
    [ -r "$d/type" ] || continue
    if [ "$(cat "$d/type" 2>/dev/null)" = "wlan" ]; then
      exec /usr/sbin/rfkill unblock wifi
    fi
  done
  i=$((i + 1))
  sleep 0.2
done
exec /usr/sbin/rfkill unblock wifi
WIFIHELPER
    chmod 755 "$helper" 2>/dev/null || true

    cat > "$unit" << 'WIFIUNIT'
[Unit]
Description=Unblock WiFi RF-kill (ROCK 4C+)
DefaultDependencies=no
Wants=network-pre.target
Before=network-pre.target NetworkManager.service
After=sysinit.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/sentryusb-unblock-wifi

[Install]
WantedBy=multi-user.target
WIFIUNIT

    # A silent write failure would leave the bug unfixed while reporting success.
    if ! grep -q "$helper" "$unit" 2>/dev/null || [ ! -x "$helper" ]; then
        err "rfkill-wifi: install failed (read-only fs? check remountfs_rw)"
        _relock
        return 1
    fi

    systemctl enable rfkill-unblock-wifi.service 2>/dev/null || true
    systemctl daemon-reload 2>/dev/null || true
    "$helper" 2>/dev/null || true   # take effect now, not just next boot
    _relock
    log "rfkill-wifi: WiFi unblock boot service installed (ROCK 4C+)"
}

# ── Run all patches ─────────────────────────────────────────────────────

apply_ble_nonfatal_adv
apply_ble_adv_helper
apply_eatt_disable
apply_backingfiles_bfq
apply_hardware_watchdog
apply_archive_mount_lock_scripts
apply_rfkill_unblock_wifi

# Future patches that must survive an OTA update get appended here. Each
# one self-checks board / precondition / marker so the whole script stays
# a safe no-op on non-applicable systems.
