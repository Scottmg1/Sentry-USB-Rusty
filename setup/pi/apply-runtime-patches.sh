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
#   BCM4345C0 — Rock 4C+ (confirmed broken via field evidence) AND
#     Raspberry Pi 4 Model B (confirmed broken via btmon trace 2026-07-25:
#     "Add Extended Advertising Data (0x0055) → Invalid Parameters (0x0d)";
#     without the helper only LE Set Advertising Parameters + Enable are
#     issued — no Set Advertising Data (0x0008) — so the Pi advertises an
#     EMPTY packet and is undiscoverable by name/UUID). Pi 4B's BT loads
#     firmware brcm/BCM4345C0.raspberrypi,4-model-b.hcd — same silicon.
#   BCM43430B0 — Pi Zero 2 W (confirmed broken via btmon trace 2026-06-20)
#   BCM43438 — Pi 3B/3B+, Pi Zero W (same chip family / same firmware tree)
#
# DELIBERATELY EXCLUDED until tested:
#   BCM43455 / CYW43455 — Pi 5; its modern bluetoothd path is reported to
#   work fine, and running our raw-HCI helper there would override its
#   working ext-adv with legacy adv (regression). (Note: Raspberry Pi 4
#   Model B was previously excluded here on the assumption it uses
#   BCM43455 — in the field its BT identifies as BCM4345C0 and rejects
#   ext-adv, so it is now in the broken list above.) If an excluded board
#   does hit "GATT 147 bond=BOND_NONE" the operator can opt in with:
#       sudo touch /mutable/force-ble-adv-helper
#   That sentinel forces install regardless of chip detection. The next OTA
#   (or `sudo /usr/local/bin/sentryusb-apply-runtime-patches`) lands it.
# NOTE: the old is_known_broken_ble_chip() gate was removed here. It keyed on
# the BT chip (BCM4345C0 et al.), which cannot classify these boards — the
# working Pi 5, the broken 4C+, and the broken Pi 4B are all BCM4345C0. Whether
# the helper runs is now decided per-boot by the RF-verified native manifest
# (select-ble-adv-mode.sh + ble-native-manifest); apply_ble_adv_helper below
# stages the files on every board and the mode marker gates the advertiser.
# /mutable/force-ble-adv-helper still works — the selector honors it first.

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
    # Newer ble.py removed register_ad_error_cb entirely (native mode retries
    # registration itself) — nothing to patch.
    if ! grep -q 'def register_ad_error_cb' "$f"; then
        log "BLE non-fatal-adv: obsolete on this build (retry built in)"
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
# Files installed (staged on every board; the mode marker gates the advertiser):
#   /usr/local/bin/sentryusb-ble-adv.sh
#   /usr/local/bin/select-ble-adv-mode.sh
#   /usr/local/share/sentryusb/ble-native-manifest
#   /etc/systemd/system/sentryusb-ble-adv.service
#   /etc/systemd/system/sentryusb-ble-mode.service
#   /etc/udev/rules.d/99-sentryusb-ble-hci.rules
#   /etc/systemd/system/sentryusb-ble.service.d/wants-bluetooth.conf
#   /root/bin/sentryusb-ble.py   (re-fetched whole so existing installs converge)
apply_ble_adv_helper() {
    # No chip gate: stage everything on every board; the boot-time mode marker
    # (select-ble-adv-mode.sh + ble-native-manifest) picks the advertiser.
    local repo="${REPO:-Sentry-Six/Sentry-USB-Rusty}"
    local base="https://raw.githubusercontent.com/${repo}/main/setup/pi"
    local ble_base="https://raw.githubusercontent.com/${repo}/main/server/ble"

    # (url, dst, mode) set. ble.py is re-fetched whole (the mode-gating adds
    # three edit sites the old in-place patcher can't reliably reach; the repo
    # copy is the source of truth and already carries the non-fatal-adv fix).
    local specs=(
        "$base/sentryusb-ble-adv.sh|/usr/local/bin/sentryusb-ble-adv.sh|755"
        "$base/select-ble-adv-mode.sh|/usr/local/bin/select-ble-adv-mode.sh|755"
        "$base/ble-native-manifest|/usr/local/share/sentryusb/ble-native-manifest|644"
        "$base/sentryusb-ble-adv.service|/etc/systemd/system/sentryusb-ble-adv.service|644"
        "$base/sentryusb-ble-mode.service|/etc/systemd/system/sentryusb-ble-mode.service|644"
        "$base/99-sentryusb-ble-hci.rules|/etc/udev/rules.d/99-sentryusb-ble-hci.rules|644"
        "$base/sentryusb-ble-wants-bluetooth.conf|/etc/systemd/system/sentryusb-ble.service.d/wants-bluetooth.conf|644"
        "$ble_base/sentryusb-ble.py|/root/bin/sentryusb-ble.py|755"
    )

    # All-or-nothing: fetch everything to temp first; any fetch failure aborts
    # untouched (a half-applied set can dark or dual-own the board).
    local tmpdir; tmpdir="$(mktemp -d)" || { warn "BLE adv: mktemp -d failed"; return 0; }
    local -a tmps dsts modes
    local i=0 spec rest dst mode url
    for spec in "${specs[@]}"; do
        url="${spec%%|*}"; rest="${spec#*|}"; dst="${rest%%|*}"; mode="${rest##*|}"
        if ! curl -fsSL --max-time 15 "$url" -o "$tmpdir/$i" 2>/dev/null; then
            warn "BLE adv: fetch failed for ${url##*/} — aborting BLE update (no partial apply)"
            rm -rf "$tmpdir"; return 0
        fi
        tmps[i]="$tmpdir/$i"; dsts[i]="$dst"; modes[i]="$mode"; i=$((i+1))
    done

    # Atomic install: back up changed files / track new ones, and on ANY install
    # failure roll the whole set back (a mixed set would dark/dual-own on reboot).
    local changed=0 install_failed=0 n=$i
    [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true
    local -a bak_files new_files
    for (( i = 0; i < n; i++ )); do
        [ -f "${dsts[i]}" ] && cmp -s "${tmps[i]}" "${dsts[i]}" && continue
        if [ -f "${dsts[i]}" ]; then
            # Back up before changing; if the backup fails, abort before the
            # install (can't roll back an untracked change).
            if cp -p "${dsts[i]}" "${dsts[i]}.subak" 2>/dev/null; then
                bak_files+=("${dsts[i]}")
            else
                rm -f "${dsts[i]}.subak" 2>/dev/null
                install_failed=1
                warn "BLE adv: could not back up ${dsts[i]} — aborting before install"
                break
            fi
        else
            new_files+=("${dsts[i]}")
        fi
        if install -D -m "${modes[i]}" "${tmps[i]}" "${dsts[i]}"; then
            changed=1
            log "BLE adv: installed/refreshed ${dsts[i]}"
        else
            install_failed=1
            warn "BLE adv: install failed for ${dsts[i]}"
            break
        fi
    done
    rm -rf "$tmpdir"

    if [ "$install_failed" = "1" ]; then
        warn "BLE adv: install failed — rolling back to the previous consistent set"
        for f in "${bak_files[@]}"; do mv -f "$f.subak" "$f" 2>/dev/null || true; done
        for f in "${new_files[@]}"; do rm -f "$f" 2>/dev/null || true; done
        return 0
    fi
    # Success — discard the backups.
    for f in "${bak_files[@]}"; do rm -f "$f.subak" 2>/dev/null || true; done

    if [ "$changed" = "1" ]; then
        systemctl daemon-reload 2>/dev/null || true
        udevadm control --reload-rules 2>/dev/null || true
        systemctl enable sentryusb-ble-mode.service >/dev/null 2>&1 || true
        systemctl enable sentryusb-ble-adv.service  >/dev/null 2>&1 || true
        # ble.py reads the marker once at startup, so restart it on ANY BLE file
        # change (a manifest/selector-only change can flip the mode).
        systemctl restart sentryusb-ble-mode.service 2>/dev/null || \
            /usr/local/bin/select-ble-adv-mode.sh 2>/dev/null || true
        systemctl reset-failed sentryusb-ble.service 2>/dev/null || true
        systemctl restart sentryusb-ble.service 2>/dev/null || true
        systemctl restart sentryusb-ble-adv.service 2>/dev/null || true
        log "BLE adv: gate + advertiser refreshed; mode re-selected; daemons restarted"
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

# ── rclone config → /mutable migration ──────────────────────────────────
#
# rclone persists refreshed OAuth tokens (Google Drive et al.) by rewriting
# rclone.conf in place. On fielded devices /root/.config/rclone is a real
# directory on the read-only root, so every refresh fails ("Failed to save
# config after 10 tries: ... read-only file system") and archiving dies
# once the token expires. The legacy bash installer migrated the dir to
# /mutable/configs/rclone behind a symlink; the Rust setup flow never ran
# that migration, so heal it here. Mirrors
# crates/setup/src/archive.rs::ensure_rclone_config_on_mutable.
apply_rclone_config_mutable_migration() {
    # Only relevant when rclone is the archive backend (its archive-clips.sh
    # variant is the only one invoking "rclone --config") or a config already
    # exists at the legacy location.
    if ! grep -q 'rclone --config' /root/bin/archive-clips.sh 2>/dev/null \
       && [ ! -e /root/.config/rclone ] && [ ! -L /root/.config/rclone ]; then
        log "rclone-config: not applicable"
        return 0
    fi

    if [ -L /root/.config/rclone ] \
       && [ "$(readlink /root/.config/rclone)" = /mutable/configs/rclone ] \
       && [ -d /mutable/configs/rclone ]; then
        log "rclone-config: already on /mutable"
        return 0
    fi

    # The whole point is persisting onto the mutable partition — if it can't
    # be mounted, migrating now would strand the config. Retry next OTA.
    if ! findmnt --mountpoint /mutable >/dev/null 2>&1; then
        mount /mutable 2>/dev/null || true
    fi
    if ! findmnt --mountpoint /mutable >/dev/null 2>&1; then
        warn "rclone-config: /mutable not mounted — skipping migration"
        return 0
    fi

    local ro_before=no
    findmnt -no OPTIONS / 2>/dev/null | grep -qE '(^|,)ro(,|$)' && ro_before=yes
    if [ "$ro_before" = yes ]; then
        [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 \
            || mount -o remount,rw / 2>/dev/null || true
    fi
    _rclone_relock() {
        [ "$ro_before" = yes ] || return 0
        sync
        mount -o remount,ro / 2>/dev/null || true
    }

    mkdir -p /mutable/configs /root/.config 2>/dev/null || true

    if [ -L /root/.config/rclone ]; then
        # Wrong or dangling symlink → recreate it.
        mkdir -p /mutable/configs/rclone
        rm -f /root/.config/rclone
        ln -s /mutable/configs/rclone /root/.config/rclone
    elif [ -d /root/.config/rclone ]; then
        if [ -d /mutable/configs/rclone ]; then
            # Half-migrated (e.g. power loss mid-move): the mutable copy wins
            # conflicts — it holds the freshest OAuth tokens. cp -n never
            # overwrites; copy anything only present on the root side.
            cp -an /root/.config/rclone/. /mutable/configs/rclone/ 2>/dev/null || true
            rm -rf /root/.config/rclone
        else
            mv /root/.config/rclone /mutable/configs/
        fi
        ln -s /mutable/configs/rclone /root/.config/rclone
    else
        # Nothing configured yet: pre-provision so a later `rclone config`
        # over SSH writes through to /mutable without needing an rw root.
        mkdir -p /mutable/configs/rclone
        ln -s /mutable/configs/rclone /root/.config/rclone
    fi
    chmod 700 /mutable/configs/rclone 2>/dev/null || true

    _rclone_relock

    if [ -L /root/.config/rclone ] && [ -d /mutable/configs/rclone ]; then
        log "rclone-config: migrated to /mutable/configs/rclone"
    else
        err "rclone-config: migration failed (read-only fs? check remountfs_rw)"
        return 1
    fi
}

# ── rclone watchdog fix ──────────────────────────────────────────────────
#
# The shipped rclone archive-clips.sh probed "$RCLONE_DRIVE" — the rclone
# remote *name* (e.g. "gdrive"), not a hostname — so the connection monitor
# could never see a live connection and killed rclone ~7s into every
# archive run ("connection dead, killing rclone archive", exit 143,
# "Archived 0 files"). The fixed script probes ARCHIVE_SERVER (a pingable
# IP, 8.8.8.8 default), honors Travel Mode, and scopes the kill to its own
# rclone invocation. /root/bin scripts are only rewritten at wizard time,
# so existing installs need this refresh.
#
# The heredocs below MUST stay byte-identical to
# run/rclone_archive/archive-clips.sh and
# run/rclone_archive/archive-is-reachable.sh.
apply_rclone_watchdog_fix() {
    # Only the rclone variant invokes "rclone --config"; cifs/nfs/rsync
    # installs have their own archive-clips.sh which must stay untouched.
    if ! grep -q 'rclone --config' /root/bin/archive-clips.sh 2>/dev/null; then
        log "rclone-watchdog: not applicable"
        return 0
    fi
    if grep -q 'RCLONE_MONITOR_V2' /root/bin/archive-clips.sh 2>/dev/null \
       && grep -q 'ARCHIVE_PING_TIMEOUT' /root/bin/archive-is-reachable.sh 2>/dev/null; then
        log "rclone-watchdog: already patched"
        return 0
    fi

    local ro_before=no
    findmnt -no OPTIONS / 2>/dev/null | grep -qE '(^|,)ro(,|$)' && ro_before=yes
    if [ "$ro_before" = yes ]; then
        [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 \
            || mount -o remount,rw / 2>/dev/null || true
    fi
    _rclone_wd_relock() {
        [ "$ro_before" = yes ] || return 0
        sync
        mount -o remount,ro / 2>/dev/null || true
    }

    # Staged + atomic rename: a power loss or disk-full mid-write must never
    # leave a truncated live script (archiveloop invokes these every cycle,
    # and a half-written file containing the marker would make the next
    # patch run report "already patched").
    cat > /root/bin/archive-clips.sh.new <<'RCLONECLIPS_EOF'
#!/bin/bash -eu

# read the setup variables again because arrays, like RCLONE_FLAGS, don't export to subshells/child scripts
source /root/bin/envsetup.sh

# RCLONE_MONITOR_V2: probes ARCHIVE_SERVER, travel-mode aware, scoped kill.
#
# Connection monitor: poll the liveness IP every ~10s. Consecutive misses
# kill rclone (and this script) so archiveloop can reach
# `connect_usb_drives_to_host` and put the gadget back online instead of
# hanging on a dropped TCP/cloud connection while the user drives away.
# The `--timeout`/`--contimeout` flags below give rclone its own internal
# floor; the monitor is a hard outer bound for cases where rclone's retry
# loop takes too long to surrender.
#
# The probe target is ARCHIVE_SERVER (a pingable IP, 8.8.8.8 by default for
# cloud remotes — see run/archiveloop), NOT $RCLONE_DRIVE: that is rclone's
# remote *name* (e.g. "gdrive"), which is not a hostname and can never
# answer a ping, so probing it killed every archive run within seconds.
#
# Travel Mode (passed fresh by archiveloop as TRAVEL_MODE_ACTIVE) relaxes the
# thresholds for slow, high-latency mobile links, mirroring
# run/rsync_archive/archive-clips.sh. Normal mode keeps the original snappy
# values so "drive away from home" recovery is unchanged.
if [ "${TRAVEL_MODE_ACTIVE:-0}" = "1" ]; then
  MONITOR_MISSES=20            # ~minutes of sustained loss before giving up
  MONITOR_TIMEOUT=20           # must exceed the patient probe below
  export ARCHIVE_PING_TIMEOUT=4
else
  MONITOR_MISSES=5
  MONITOR_TIMEOUT=6
fi

function connectionmonitor {
  while true
  do
    for (( i = 1; i <= MONITOR_MISSES; i++ ))
    do
      if timeout "$MONITOR_TIMEOUT" /root/bin/archive-is-reachable.sh "${ARCHIVE_SERVER:-8.8.8.8}"
      then
        sleep 5
        continue 2
      fi
      sleep 1
    done
    log "connection dead, killing rclone archive"
    # Scoped to this script's own move invocation: a plain `killall rclone`
    # would also take out unrelated rclone processes (the drive-data sync in
    # post-archive-process.sh, an in-flight cloud config backup).
    pkill -f 'rclone --config /root/\.config/rclone/rclone\.conf move' || true
    sleep 2
    pkill -9 -f 'rclone --config /root/\.config/rclone/rclone\.conf move' || true
    kill -9 "$1" || true
    return
  done
}

connectionmonitor $$ &

# Layer-1 (rclone-level) safety nets. The bash monitor is layer-2.
flags=("-L" "--transfers=1" "--timeout=30s" "--contimeout=10s" "--retries=1")
if [[ -v RCLONE_FLAGS ]]
then
  flags+=("${RCLONE_FLAGS[@]}")
fi

while [ -n "${1+x}" ]
do
  # Low I/O + CPU priority so the archive reads never starve the car's
  # dashcam writes on the same disk (see run/rsync_archive/archive-clips.sh
  # for the full rationale; -c2 -n7 not -c3 so progress is guaranteed).
  ionice -c2 -n7 nice -n19 rclone --config /root/.config/rclone/rclone.conf move "${flags[@]}" --files-from "$2" "$1" "$RCLONE_DRIVE:$RCLONE_PATH" >> "$LOG_FILE" 2>&1
  shift 2
done

# Stop the monitor so it doesn't leak past archive completion.
kill %1 || true
RCLONECLIPS_EOF

    cat > /root/bin/archive-is-reachable.sh.new <<'RCLONEREACH_EOF'
#!/bin/bash -eu

# $1 is a pingable liveness IP (ARCHIVE_SERVER, default 8.8.8.8 for cloud
# remotes). ARCHIVE_PING_TIMEOUT is exported by archive-clips.sh in Travel
# Mode for slow mobile links; default matches the original 1s probe.
ping -q -w "${ARCHIVE_PING_TIMEOUT:-1}" -c 1 "$1" > /dev/null 2>&1
RCLONEREACH_EOF

    chmod 755 /root/bin/archive-clips.sh.new /root/bin/archive-is-reachable.sh.new 2>/dev/null || true

    if ! bash -n /root/bin/archive-clips.sh.new 2>/dev/null \
       || ! bash -n /root/bin/archive-is-reachable.sh.new 2>/dev/null; then
        err "rclone-watchdog: staged script failed bash -n (disk full? truncated write?)"
        rm -f /root/bin/archive-clips.sh.new /root/bin/archive-is-reachable.sh.new
        _rclone_wd_relock
        return 1
    fi

    mv /root/bin/archive-clips.sh.new /root/bin/archive-clips.sh
    mv /root/bin/archive-is-reachable.sh.new /root/bin/archive-is-reachable.sh
    _rclone_wd_relock
    log "rclone-watchdog: archive scripts refreshed (probe ARCHIVE_SERVER, scoped kill)"
}

# ── Rock 4C+ WiFi NVRAM: remove the TX-collapsing AP6256 relink ──────────
# The old 4C+ installer symlinked the board WiFi NVRAM to nvram_ap6256.txt,
# which collapses TX to ~6 Mbit/s (sole TX-power source, no txcap_blob).
# Remove it → driver falls back to the generic brcmfmac43455-sdio.txt. Heals
# existing boxes on OTA (reboots on completion). BT coexistence is the .hcd
# patch, not this.
apply_4cplus_wifi_nvram_fix() {
    is_rock_4cplus || return 0
    local brcm=/lib/firmware/brcm
    local link="$brcm/brcmfmac43455-sdio.radxa,rock-4c-plus.txt"
    # Only our exact relink (symlink -> nvram_ap6256.txt); leave anything else alone.
    [ -L "$link" ] || { log "4c+ wifi nvram: no board relink — generic in use"; return 0; }
    if [ "$(basename "$(readlink "$link")")" != "nvram_ap6256.txt" ]; then
        log "4c+ wifi nvram: board .txt not the AP6256 relink — leaving as-is"
        return 0
    fi
    # Re-lock only if we unlocked (leave root as found).
    local ro_before=no
    findmnt -no OPTIONS / 2>/dev/null | grep -qE '(^|,)ro(,|$)' && ro_before=yes
    [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true
    if rm -f "$link" 2>/dev/null; then
        log "4c+ wifi nvram: removed AP6256 relink → generic fallback (REBOOT to apply)"
    else
        err "4c+ wifi nvram: could not remove $link (read-only fs? check remountfs_rw)"
    fi
    if [ "$ro_before" = yes ]; then
        sync
        mount -o remount,ro / 2>/dev/null || true
    fi
}


# ── Snapshot eviction/slot-order fixes (all boards) ─────────────────────
#
# Snapshot slot names are NOT time-monotonic in the field: a reflash can
# leave a stale high-numbered snapshot above a freshly restarted sequence
# (real device: snap-000414 from Jul 9 sitting over snap-000413 from
# Aug 8, numbering restarted at snap-000000 beneath it). Two legacy-bash
# consequences fixed here for installs that still run the FULL scripts:
#   - manage_free_space.sh picked the "oldest" snapshot by NAME
#     (`sort | head -1`) and could evict newer footage while sparing the
#     genuinely oldest snapshot;
#   - make_snapshot.sh derived its next slot from snap.bin paths, so a
#     tree with dirs but no snap.bin restarts numbering at 0.
# Modern installs have thin wrappers calling `sentryusb space manage` /
# `sentryusb snapshot make`; the Rust binary carries these fixes there,
# and the wrappers are deliberately left untouched.

apply_snapshot_eviction_by_age() {
    local f=/root/bin/manage_free_space.sh
    [ -f "$f" ] || { warn "eviction-by-age: $f missing — skipping"; return 0; }
    if grep -q 'sentryusb space manage' "$f"; then
        log "eviction-by-age: thin Rust wrapper — binary carries the fix"
        return 0
    fi
    if grep -q 'oldest by snap.bin mtime' "$f"; then
        log "eviction-by-age: already patched"
        return 0
    fi
    # Known legacy body fingerprint (unchanged since the original port).
    if ! grep -qF "oldest=\$(find /backingfiles/snapshots -maxdepth 1 -name 'snap-*' | sort | head -1)" "$f"; then
        warn "eviction-by-age: unknown local variant of $f — leaving untouched"
        return 0
    fi
    [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true

    # Whole-file staged replace (this script was byte-stable since the
    # port, so the fingerprint above proves we know exactly what we are
    # replacing). Atomic rename so archiveloop can never see a torn file.
    cat > "$f.new" <<'MFS_EOF'
#!/bin/bash -eu

if [ "${BASH_SOURCE[0]}" != "$0" ]
then
  echo "${BASH_SOURCE[0]} must be executed, not sourced"
  return 1 # shouldn't use exit when sourced
fi

if [ "${FLOCKED:-}" != "$0" ]
then
  mkdir -p /backingfiles/snapshots
  if FLOCKED="$0" flock -E 99 /backingfiles/snapshots "$0" "$@" || case "$?" in
  99) echo "failed to lock snapshots dir"
      exit 99
      ;;
  *)  exit $?
      ;;
  esac
  then
    # success
    exit 0
  fi
fi

function manage_free_space {
  # Try to make free space equal to 10 GB plus three percent of the total
  # available space. This should be enough to hold the next hour of
  # recordings without completely filling up the filesystem.
  # todo: this could be put in a background task and with a lower free
  # space requirement, to delete old snapshots just before running out
  # of space and thus make better use of space
  local reserve="$1"
  while true
  do
    local freespace
    freespace=$(eval "$(stat --file-system --format="echo \$((%f*%S))" /backingfiles/cam_disk.bin)")
    if [ "$freespace" -gt "$reserve" ]
    then
      exit 0
    fi
    # Candidates = snapshot dirs that actually hold a snap.bin, ordered by
    # the bin's mtime (TRUE age), name as tie-break. Slot numbers are not
    # time-monotonic in the field — a reflash can leave a stale
    # high-numbered snapshot above a restarted sequence — and the old
    # name-order pick (`sort | head -1`) deleted newer footage while
    # sparing the genuinely oldest snapshot. The same list feeds the
    # "fewer than two" guard so the count and the pick can't disagree.
    local candidates
    candidates=$(
      LC_ALL=C find /backingfiles/snapshots \
        -mindepth 2 -maxdepth 2 -type f \
        -path '/backingfiles/snapshots/snap-*/snap.bin' \
        -printf '%T@\t%h\n' 2>/dev/null |
      LC_ALL=C sort -t $'\t' -k1,1n -k2,2
    )
    if [ -z "$candidates" ]
    then
      log "Warning: low space for new snapshots, but no snapshots exist."
      log "Please use a larger storage medium or reduce CAM_SIZE"
      exit 1
    fi
    # if there's only one snapshot then we likely just took it, so don't immediately delete it
    if [ "$(printf '%s\n' "$candidates" | wc -l)" -lt 2 ]
    then
      # there's only one snapshot and yet we're low on space
      log "Warning: low space for new snapshots, but only one snapshot exists."
      log "Please use a larger storage medium or reduce CAM_SIZE"
      exit 1
    fi

    oldest=$(printf '%s\n' "$candidates" | head -1 | cut -f2-)
    if [ -z "$oldest" ]
    then
      log "unable to select oldest snapshot"
      exit 1
    fi
    log "low space, deleting $oldest (oldest by snap.bin mtime)"
    /root/bin/release_snapshot.sh "$oldest"
    rm -rf "$oldest"
  done
}

# Normally called by archiveloop's freespacemanager with "10G + 3% of
# total space". When invoked by hand with no argument, compute the SAME
# policy rather than falling back to a flat 20G: a size-blind default is
# harsher than the real policy on small drives and far weaker on
# multi-TB ones, which is exactly the vintage-dependent divergence this
# shares with crates/usb_gadget/src/space.rs (default_reserve_bytes).
reserve="${1:-}"
if [ -z "$reserve" ]
then
  reserve=$(eval "$(stat --file-system --format="echo \$((10737418240 + %b*%S/33))" /backingfiles/cam_disk.bin)")
fi
manage_free_space "$reserve"
MFS_EOF
    if ! bash -n "$f.new" 2>/dev/null; then
        err "eviction-by-age: staged replacement failed bash -n — aborting"
        rm -f "$f.new"
        return 0
    fi
    chmod --reference="$f" "$f.new" 2>/dev/null || chmod 755 "$f.new"
    mv "$f.new" "$f"
    log "eviction-by-age: replaced legacy manage_free_space.sh (mtime-ordered eviction)"
}

apply_snapshot_slot_pick_hardening() {
    local f=/root/bin/make_snapshot.sh
    [ -f "$f" ] || { warn "slot-pick: $f missing — skipping"; return 0; }
    if grep -q 'sentryusb snapshot make' "$f"; then
        log "slot-pick: thin Rust wrapper — binary carries the fix"
        return 0
    fi
    if grep -q 'slot pick: picker=bash' "$f"; then
        log "slot-pick: already patched"
        return 0
    fi
    # The picker block is byte-stable since the original port even though
    # the rest of make_snapshot.sh has drifted across releases; anchor on
    # its first and last lines and replace just that block.
    if ! grep -qF 'oldnum=$(find /backingfiles/snapshots/snap-* -maxdepth 1 -name snap.bin | sort | tail -1' "$f"; then
        warn "slot-pick: unknown local variant of $f — leaving untouched"
        return 0
    fi
    [ -x /root/bin/remountfs_rw ] && /root/bin/remountfs_rw >/dev/null 2>&1 || true

    local result
    result="$(python3 - "$f" 2>&1 <<'SLOT_PYEOF'
import sys
p = sys.argv[1]; s = open(p).read()
a = s.find("  local oldnum=-1")
b = s.find("  newsnapdir=/backingfiles/snapshots/snap-", a)
if a < 0 or b <= a:
    print("anchor-not-found"); raise SystemExit
new = """  # Highest slot from snapshot DIRECTORY names, not snap.bin presence
  # (slot pick hardening — see run/make_snapshot.sh in the repo).
  local oldnum=-1
  local newnum=0
  local maxdir
  maxdir=$(LC_ALL=C find /backingfiles/snapshots -mindepth 1 -maxdepth 1 \
             -type d -name 'snap-*' 2>/dev/null |
           grep -E '/snap-[0-9]+$' | LC_ALL=C sort | tail -1)
  if [ -n "$maxdir" ]
  then
    oldnum=$(basename "$maxdir" | tr -c -d '[:digit:]' | sed 's/^0*//')
    oldnum=${oldnum:-0}
    newnum=$((oldnum + 1))
  fi
  local oldname
  local newsnapdir
  oldname=/backingfiles/snapshots/snap-$(printf "%06d" "$oldnum")/snap.bin

  # check that the previous snapshot is complete (TOC AND bin present)
  if [ "$oldnum" != "-1" ] && { [ ! -e "${oldname}.toc" ] || [ ! -e "$oldname" ]; }
  then
    if grep -q "/backingfiles/snapshots/snap-$(printf "%06d" "$oldnum")/" /proc/mounts 2>/dev/null || \
       grep -q "/tmp/snapshots/snap-$(printf "%06d" "$oldnum")" /proc/mounts 2>/dev/null
    then
      log "previous snapshot snap-$(printf "%06d" "$oldnum") incomplete but mounted — appending instead of reusing"
      oldnum=$((oldnum - 1))
      oldname=/backingfiles/snapshots/snap-$(printf "%06d" "$oldnum")/snap.bin
    else
      log "previous snapshot was incomplete, deleting"
      rm -rf "$(dirname "$oldname")"
      newnum=$((oldnum))
      oldnum=$((oldnum - 1))
      oldname=/backingfiles/snapshots/snap-$(printf "%06d" "$oldnum")/snap.bin
    fi
  fi
  log "slot pick: picker=bash max_seen=${maxdir:-none} prev=$oldnum next=$newnum"

"""
staged = p + ".new"
open(staged, "w").write(s[:a] + new + s[b:])
print("staged")
SLOT_PYEOF
)" || result="python-error"

    if [ "$result" != "staged" ] || [ ! -f "$f.new" ]; then
        err "slot-pick: staging failed ($result) — leaving $f untouched"
        rm -f "$f.new"
        return 0
    fi
    if ! bash -n "$f.new" 2>/dev/null || ! grep -q 'slot pick: picker=bash' "$f.new"; then
        err "slot-pick: staged file failed verification — aborting"
        rm -f "$f.new"
        return 0
    fi
    chmod --reference="$f" "$f.new" 2>/dev/null || chmod 755 "$f.new"
    mv "$f.new" "$f"
    log "slot-pick: patched legacy make_snapshot.sh picker (dir-name max + mounted guard)"
}

# ── Run all patches ─────────────────────────────────────────────────────

apply_ble_nonfatal_adv
apply_ble_adv_helper
apply_eatt_disable
apply_backingfiles_bfq
apply_hardware_watchdog
apply_archive_mount_lock_scripts
apply_rfkill_unblock_wifi
apply_4cplus_wifi_nvram_fix
apply_rclone_config_mutable_migration
apply_rclone_watchdog_fix
apply_snapshot_eviction_by_age
apply_snapshot_slot_pick_hardening

# Future patches that must survive an OTA update get appended here. Each
# one self-checks board / precondition / marker so the whole script stays
# a safe no-op on non-applicable systems.
