#!/bin/bash -eu

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

unmount_if_set "${ARCHIVE_MOUNT:-}" &
unmount_if_set "${MUSIC_ARCHIVE_MOUNT:-}" &
