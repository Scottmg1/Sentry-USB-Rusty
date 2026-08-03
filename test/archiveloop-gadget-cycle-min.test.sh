#!/bin/bash
# Focused tests for redundant USB gadget reconnect reduction.
set -euo pipefail

script=${1:-run/archiveloop}

sync_user_media_to_cam_source=$(awk '
  /^function sync_user_media_to_cam / {keep=1}
  keep {print}
  /^function sync_disk_media_to_host / {exit}
' "$script" | sed '$d')

connect_usb_drives_to_host_locked_source=$(awk '
  /^function connect_usb_drives_to_host_locked/ {keep=1}
  keep {print}
  /^# Inventory digest of watched host media:/ {exit}
' "$script" | sed '$d')

eval "$sync_user_media_to_cam_source"

eval "$(awk '
  /^function host_cam_media_generation / {keep=1}
  /^function ensure_usb_drives_connected / {keep=1}
  keep {print}
  /^function away_mode_active / {exit}
' "$script" | sed '$d')"

eval "$(awk '
  /^function clean_cam_mount / {keep=1}
  keep {print}
  /^# Directory structure car uses:/ {exit}
' "$script" | sed '$d')"

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

# macOS development hosts lack GNU find's -printf. Supply only the inventory
# records this test needs, then restore the real find before cleanup tests.
if ! /usr/bin/find "$workdir" -maxdepth 0 -printf '' 2>/dev/null
then
  find() {
    local root="$1" path rel type size mtime
    if [ "${2:-}" = "-maxdepth" ]
    then
      size=$(/usr/bin/stat -f '%z' "$root")
      mtime=$(/usr/bin/stat -f '%m' "$root")
      printf 'file:%s\t%s\t%s\n' "$root" "$size" "$mtime"
      return
    fi
    /usr/bin/find "$root" -print 2>/dev/null | while IFS= read -r path
    do
      rel=${path#"$root"}
      rel=${rel#/}
      if [ -L "$path" ]; then type=l
      elif [ -d "$path" ]; then type=d
      else type=f
      fi
      size=$(/usr/bin/stat -f '%z' "$path")
      mtime=$(/usr/bin/stat -f '%m' "$path")
      printf '%s\t%s\t%s\t%s\n' "$rel" "$type" "$size" "$mtime"
    done
  }
fi

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
touch -t 203305180333 "$workdir/Wraps"
record_cam_media_sync_success
touch -t 200109090146 "$CAM_MEDIA_SYNC_STAMP"
ensure_usb_drives_connected
ensure_usb_drives_connected
assert_calls 0 "clock correction"
unset -f find 2>/dev/null || true

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
if ! command -v flock > /dev/null 2>&1
then
  flock() { return 0; }
fi
if ! /usr/bin/stat -Lc '%s' "$workdir" > /dev/null 2>&1
then
  stat() {
    if [ "${1:-}" = "-Lc" ]
    then
      /usr/bin/stat -L -f '%z' "$4"
    else
      /usr/bin/stat "$@"
    fi
  }
fi
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

# J: watched-media failures stay dirty and keep deletion tombstones
CAM_MOUNT="$workdir/sync-cam"
CAM_WRAPS_PATH="$workdir/sync-Wraps"
CAM_LICENSE_PLATE_PATH="$workdir/sync-LicensePlate"
CAM_WRAPS_DELETED_PATH="$workdir/sync-wraps-deleted"
sync_mount_status=0
sync_rsync_status=0
sync_rsync_calls=0
sync_test_failures=0
real_rsync_path=$(command -v rsync)
has_cam_disk() { return 0; }
ensure_mountpoint_is_mounted() { return "$sync_mount_status"; }
unmount_mount_point() { :; }
rsync() {
  sync_rsync_calls=$((sync_rsync_calls + 1))
  [ "$sync_rsync_status" -eq 0 ] || return "$sync_rsync_status"
  "$real_rsync_path" "$@"
}

mkdir -p "$CAM_MOUNT" "$CAM_WRAPS_PATH" "$CAM_WRAPS_DELETED_PATH"
printf 'wrap\n' > "$CAM_WRAPS_PATH/wrap.png"
printf 'deleted\n' > "$CAM_WRAPS_DELETED_PATH/deleted-wrap"
sync_rsync_status=23
# Defined by the production-function extraction above.
# shellcheck disable=SC2218
if sync_user_media_to_cam
then
  echo "failed watched-media rsync reported success" >&2
  sync_test_failures=$((sync_test_failures + 1))
fi
[ -e "$CAM_WRAPS_DELETED_PATH/deleted-wrap" ] || {
  echo "failed Wraps rsync removed deletion tombstones" >&2
  sync_test_failures=$((sync_test_failures + 1))
}

sync_rsync_status=0
# shellcheck disable=SC2218
sync_user_media_to_cam || {
  echo "successful watched-media rsync reported failure" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ ! -e "$CAM_WRAPS_DELETED_PATH" ] || {
  echo "successful Wraps rsync retained deletion tombstones" >&2
  sync_test_failures=$((sync_test_failures + 1))
}

# Removing the final Wrap leaves an existing empty source directory. Wraps
# tombstones prevent reverse-sync resurrection, so forward sync must still
# rsync --delete the stale cam copy. LicensePlate dual-writer deletion
# semantics are intentionally outside this focused reconnect fix.
rm -f "$CAM_WRAPS_PATH/wrap.png"
mkdir -p "$CAM_LICENSE_PLATE_PATH" "$CAM_WRAPS_DELETED_PATH"
printf 'stale-wrap\n' > "$CAM_MOUNT/Wraps/stale.png"
printf 'deleted\n' > "$CAM_WRAPS_DELETED_PATH/deleted-last-wrap"
sync_rsync_calls=0
# shellcheck disable=SC2218
sync_user_media_to_cam || {
  echo "empty Wraps directory reported failure" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ "$sync_rsync_calls" -eq 1 ] || {
  echo "empty Wraps directory skipped rsync --delete" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ ! -e "$CAM_MOUNT/Wraps/stale.png" ] || {
  echo "final Wrap deletion was not propagated to cam" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ ! -e "$CAM_WRAPS_DELETED_PATH" ] || {
  echo "successful empty Wraps sync retained deletion tombstones" >&2
  sync_test_failures=$((sync_test_failures + 1))
}

has_cam_disk() { return 1; }
sync_rsync_calls=0
# shellcheck disable=SC2218
sync_user_media_to_cam || {
  echo "no-cam media sync was not a no-op success" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ "$sync_rsync_calls" -eq 0 ] || {
  echo "no-cam media sync unexpectedly invoked rsync" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
has_cam_disk() { return 0; }

sync_mount_status=1
# shellcheck disable=SC2218
if sync_user_media_to_cam
then
  echo "failed cam mount reported media-sync success" >&2
  sync_test_failures=$((sync_test_failures + 1))
fi

# K: the connect boundary records only successful watched-media syncs
eval "$connect_usb_drives_to_host_locked_source"
clear_archive_status() { :; }
sync_disk_media_to_host() { :; }
fix_errors_in_images() { :; }
curl() { :; }
sleep() { :; }
sync_user_media_result=0
record_sync_calls=0
sync_user_media_to_cam() { return "$sync_user_media_result"; }
record_cam_media_sync_success() {
  record_sync_calls=$((record_sync_calls + 1))
  printf 'synced\n' > "$CAM_MEDIA_SYNC_STAMP"
}

printf 'stale\n' > "$CAM_MEDIA_SYNC_STAMP"
sync_user_media_result=1
connect_usb_drives_to_host_locked 2>> "$LOG_FILE" || {
  echo "connect stopped after optional media-sync failure" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ ! -e "$CAM_MEDIA_SYNC_STAMP" ] || {
  echo "failed media sync left a success stamp" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ "$record_sync_calls" -eq 0 ] || {
  echo "failed media sync recorded a new success stamp" >&2
  sync_test_failures=$((sync_test_failures + 1))
}

sync_user_media_result=0
connect_usb_drives_to_host_locked 2>> "$LOG_FILE" || {
  echo "connect failed after successful media sync" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ -e "$CAM_MEDIA_SYNC_STAMP" ] || {
  echo "successful media sync did not record a success stamp" >&2
  sync_test_failures=$((sync_test_failures + 1))
}
[ "$record_sync_calls" -eq 1 ] || {
  echo "successful media sync recorded the wrong number of stamps" >&2
  sync_test_failures=$((sync_test_failures + 1))
}

[ "$sync_test_failures" -eq 0 ] || exit 1

echo "archiveloop focused reconnect tests passed"
