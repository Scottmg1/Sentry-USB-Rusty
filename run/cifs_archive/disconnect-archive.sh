#!/bin/bash -eu

# Unmount the archive. Without this, the archive mounts can get into a
# state where the archive is reachable via the network, appears to be
# mounted, but the mount is inoperable and any attempt to access it
# results in a "host is down" message.

# Must match ARCHIVE_MOUNT_LOCK_PATH in crates/drives/src/archive_mount_lock.rs
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
