#!/bin/bash
# Focused tests for redundant USB gadget reconnect reduction.
set -euo pipefail

script=${1:-run/archiveloop}

eval "$(awk '
  /^function host_cam_media_generation / {keep=1}
  /^function ensure_usb_drives_connected / {keep=1}
  keep {print}
  /^function away_mode_active / {exit}
' "$script" | head -n -1)"

eval "$(awk '
  /^function clean_cam_mount / {keep=1}
  keep {print}
  /^# Directory structure car uses:/ {exit}
' "$script" | head -n -1)"

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

CAM_MEDIA_SYNC_STAMP="$workdir/media-stamp"
CAM_MEDIA_WATCH_PATHS=(
  "$workdir/Wraps"
  "$workdir/LicensePlate"
)
CAM_CLEANUP_PENDING="$workdir/cam-cleanup-pending"
LOG_FILE="$workdir/log"
: > "$LOG_FILE"
connect_calls=0
usb_active=true
logs=""

log() { logs+="$*"$'\n'; }
usb_gadget_is_active() { [ "$usb_active" = true ]; }
connect_usb_drives_to_host_locked() {
  connect_calls=$((connect_calls + 1))
  usb_active=true
  record_cam_media_sync_success
}
with_gadget_lock() { "$@"; }

reset() {
  connect_calls=0
  usb_active=true
  logs=""
  rm -f "$CAM_MEDIA_SYNC_STAMP" "$CAM_CLEANUP_PENDING"
  rm -rf "$workdir/Wraps" "$workdir/LicensePlate"
}

assert_calls() {
  local expected="$1" label="$2"
  [ "$connect_calls" -eq "$expected" ] || {
    echo "$label: expected $expected connect(s), got $connect_calls" >&2
    exit 1
  }
}

# A: healthy active gadget → no cycle
reset
record_cam_media_sync_success
ensure_usb_drives_connected
ensure_usb_drives_connected
assert_calls 0 "healthy gadget"

# B: inactive → one reconnect
reset
usb_active=false
record_cam_media_sync_success
ensure_usb_drives_connected
ensure_usb_drives_connected
assert_calls 1 "inactive recovery"

# C: host media without a recorded inventory → one cycle
reset
mkdir -p "$workdir/Wraps"
rm -f "$CAM_MEDIA_SYNC_STAMP"
ensure_usb_drives_connected
ensure_usb_drives_connected
assert_calls 1 "unrecorded host media"

# D: changed host media → one cycle, then clean
reset
mkdir -p "$workdir/Wraps"
printf 'first\n' > "$workdir/Wraps/first.png"
record_cam_media_sync_success
printf 'second\n' > "$workdir/Wraps/second.png"
ensure_usb_drives_connected
ensure_usb_drives_connected
assert_calls 1 "changed host media"

# E: an NTP step changing only the state file's mtime → no cycle
reset
mkdir -p "$workdir/Wraps"
touch -d '@2000000000' "$workdir/Wraps"
record_cam_media_sync_success
touch -d '@1000000000' "$CAM_MEDIA_SYNC_STAMP"
ensure_usb_drives_connected
ensure_usb_drives_connected
assert_calls 0 "clock correction"

# F: cleanup gating
should_cleanup() {
  local total="$1" ignore="$2" travel="$3" pending="$4"
  [ "$travel" = 1 ] && return 1
  [ "$pending" = 1 ] && return 0
  [ "$total" -eq 0 ] && [ "$ignore" -eq 0 ] && return 1
  return 0
}
should_cleanup 0 0 0 0 && { echo "zero-clip should skip cleanup" >&2; exit 1; }
should_cleanup 0 0 0 1 || { echo "pending should clean" >&2; exit 1; }
should_cleanup 3 0 0 0 || { echo "nonempty should clean" >&2; exit 1; }
should_cleanup 0 2 0 0 || { echo "shorts should clean" >&2; exit 1; }

# G: cleanup pending helpers
reset
mark_cam_cleanup_pending
[ -e "$CAM_CLEANUP_PENDING" ] || { echo "pending not published" >&2; exit 1; }
clear_cam_cleanup_pending
[ ! -e "$CAM_CLEANUP_PENDING" ] || { echo "pending not cleared" >&2; exit 1; }

# H: short recordings are removed relative to CAM_MOUNT, not the caller's cwd
CAM_MOUNT="$workdir/cam"
mkdir -p "$CAM_MOUNT/TeslaCam/RecentClips" \
  "$CAM_MOUNT/TeslaCam/SavedClips" \
  "$CAM_MOUNT/TeslaCam/SentryClips" \
  "$CAM_MOUNT/TeslaTrackMode" \
  "$workdir/runtime"
short_recording="$CAM_MOUNT/TeslaCam/RecentClips/short recording.mp4"
complete_recording="$CAM_MOUNT/TeslaCam/RecentClips/complete recording.mp4"
printf 'short\n' > "$short_recording"
truncate -s 100000 "$complete_recording"
ensure_cam_file_is_mounted() { :; }
trim_free_space() { :; }
unmount_cam_file() { :; }
(
  cd "$workdir/runtime"
  clean_cam_mount boot
)
[ ! -e "$short_recording" ] || {
  echo "short recording was not deleted" >&2
  exit 1
}
[ -e "$complete_recording" ] || {
  echo "complete recording was deleted" >&2
  exit 1
}

# I: a cleaned snapshot-only short is retired without deleting its snapshot
SNAPSHOT_LOCK_DIR="$workdir/snapshots"
index_root="$workdir/TeslaCam"
short_rel="SavedClips/2026-06-24_22-13-50/event.mp4"
complete_rel="SavedClips/2026-06-24_22-14-50/event.mp4"
short_target="$SNAPSHOT_LOCK_DIR/snap-000036/mnt/TeslaCam/$short_rel"
complete_target="$SNAPSHOT_LOCK_DIR/snap-000036/mnt/TeslaCam/$complete_rel"
short_link="$index_root/$short_rel"
complete_link="$index_root/$complete_rel"
mkdir -p "$(dirname "$short_target")" \
  "$(dirname "$complete_target")" \
  "$(dirname "$short_link")" \
  "$(dirname "$complete_link")" \
  "$index_root/RecentClips" "$index_root/SentryClips" "$index_root/TeslaTrackMode"
truncate -s 33528 "$short_target"
truncate -s 100000 "$complete_target"
ln -s "$short_target" "$short_link"
ln -s "$complete_target" "$complete_link"
snapshot_ignorelist="$workdir/snapshot-ignore-files"
printf '%s\n%s\n' "$short_rel" "$complete_rel" > "$snapshot_ignorelist"
retire_short_recording_links "$snapshot_ignorelist" "$index_root"
[ ! -L "$short_link" ] || {
  echo "cleaned snapshot short link was not retired" >&2
  exit 1
}
[ -e "$short_target" ] || {
  echo "historical snapshot short was deleted" >&2
  exit 1
}
[ -L "$complete_link" ] || {
  echo "non-short snapshot link was retired" >&2
  exit 1
}
[ -e "$complete_target" ] || {
  echo "historical complete recording was deleted" >&2
  exit 1
}

echo "archiveloop focused reconnect tests passed"
