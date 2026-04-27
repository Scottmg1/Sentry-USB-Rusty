#!/bin/bash -eu

# Connection monitor: poll the archive server every ~10s. Five
# consecutive misses kill rsync (and this script) so archiveloop can
# reach `connect_usb_drives_to_host` and put the gadget back online
# instead of hanging on a dropped SSH socket while the user drives away.
# rsync's `--timeout=600` only fires on socket-idle, not on a quietly-
# dropping link, so a bash-level monitor is the only way to bound the
# hang from outside the rsync process.
function connectionmonitor {
  while true
  do
    for _ in {1..5}
    do
      if timeout 6 /root/bin/archive-is-reachable.sh "$ARCHIVE_SERVER"
      then
        sleep 5
        continue 2
      fi
      sleep 1
    done
    log "connection dead, killing archive-clips"
    # Give rsync a chance to delete the source files it already copied
    # before we kill it hard.
    killall rsync || true
    sleep 2
    killall -9 rsync || true
    kill -9 "$1" || true
    return
  done
}

connectionmonitor $$ &

while [ -n "${1+x}" ]
do
  if ! (rsync -avhRL --timeout=600 --remove-source-files --no-perms --omit-dir-times \
        --stats --log-file=/tmp/archive-rsync-cmd.log --ignore-missing-args \
        --files-from="$2" "$1" "$RSYNC_USER@$RSYNC_SERVER:$RSYNC_PATH" &> /tmp/rsynclog || [[ "$?" = "24" ]] )
  then
    cat /tmp/archive-rsync-cmd.log /tmp/rsynclog > /tmp/archive-error.log
    kill %1 || true
    exit 1
  fi
  shift 2
done

# Stop the monitor.
kill %1 || true
