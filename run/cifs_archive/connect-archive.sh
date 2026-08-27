#!/bin/bash -eu

# Must match ARCHIVE_MOUNT_LOCK_PATH in crates/drives/src/archive_mount_lock.rs
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
