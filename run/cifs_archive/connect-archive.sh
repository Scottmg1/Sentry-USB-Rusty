#!/bin/bash -eu

function mount_if_set() {
  local mount_point=$1
  [ -z "$mount_point" ] || ensure_mountpoint_is_mounted_with_retry "$mount_point"
}

mount_if_set "${ARCHIVE_MOUNT:-}"
mount_if_set "${MUSIC_ARCHIVE_MOUNT:-}"
