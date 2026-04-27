#!/bin/bash -eu

DST="/mnt/music"
LOG="/tmp/rsyncmusiclog.txt"

# check that DST is the mounted disk image, not the mountpoint directory
if ! findmnt --mountpoint $DST > /dev/null
then
  log "$DST not mounted, skipping music sync"
  exit
fi

function connectionmonitor {
  while true
  do
    for _ in {1..10}
    do
      if timeout 3 /root/bin/archive-is-reachable.sh "$ARCHIVE_SERVER"
      then
        # sleep and then continue outer loop
        sleep 5
        continue 2
      fi
      sleep 1
    done
    log "connection dead, killing copy-music"
    # Give rsync a chance to clean up before killing it hard.
    killall rsync || true
    sleep 2
    killall -9 rsync || true
    kill -9 "$1" || true
    return
  done
}

function do_music_sync {
  log "Syncing music from archive via rsync..."

  connectionmonitor $$ &

  # Capture rsync's exit BEFORE any conditional inverts it. With
  # `if ! rsync ; then log "...$?"; fi`, $? inside the then block
  # referred to the inverted condition (always 0), so every failure
  # was logged as "rsync failed with error 0" — uselessly opaque.
  # `set -e` is on (line 1), so disable it for this one command.
  set +e
  rsync -rum --no-human-readable --exclude=.fseventsd/*** --exclude=*.DS_Store --exclude=.metadata_never_index \
                --exclude="System Volume Information/***" \
                --delete --modify-window=2 --info=stats2 \
                "$RSYNC_USER@$RSYNC_SERVER:$MUSIC_SHARE_NAME/" "$DST" &> "$LOG"
  RSYNC_EXIT=$?
  set -e
  if [ "$RSYNC_EXIT" -ne 0 ]
  then
    log "rsync failed with error $RSYNC_EXIT (see $LOG for stderr)"
    # Surface the last few lines of stderr so the archiveloop log is
    # actionable without needing to ssh in for /tmp/rsyncmusiclog.txt.
    tail -n 5 "$LOG" 2>/dev/null | while IFS= read -r _line; do
      log "  rsync: $_line"
    done
  fi

  # Stop the connection monitor.
  kill %1 || true

  # remove empty directories
  find $DST -depth -type d -empty -delete || true

  # parse log for relevant info
  declare -i NUM_FILES_COPIED
  NUM_FILES_COPIED=$(($(sed -n -e 's/\(^Number of regular files transferred: \)\([[:digit:]]\+\).*/\2/p' "$LOG")))
  declare -i NUM_FILES_DELETED
  NUM_FILES_DELETED=$(($(sed -n -e 's/\(^Number of deleted files: [[:digit:]]\+ (reg: \)\([[:digit:]]\+\)*.*/\2/p' "$LOG")))
  declare -i TOTAL_FILES
  TOTAL_FILES=$(sed -n -e 's/\(^Number of files: [[:digit:]]\+ (reg: \)\([[:digit:]]\+\)*.*/\2/p' "$LOG")
  declare -i NUM_FILES_ERROR
  NUM_FILES_ERROR=$(($(grep -c "failed to open" $LOG || true)))

  declare -i NUM_FILES_SKIPPED=$((TOTAL_FILES-NUM_FILES_COPIED))
  NUM_FILES_COPIED=$((NUM_FILES_COPIED-NUM_FILES_ERROR))

  local message="Copied $NUM_FILES_COPIED music file(s), deleted $NUM_FILES_DELETED, skipped $NUM_FILES_SKIPPED previously-copied files, and encountered $NUM_FILES_ERROR errors."

  if [ $NUM_FILES_COPIED -ne 0 ] || [ $NUM_FILES_DELETED -ne 0 ] || [ $NUM_FILES_ERROR -ne 0 ]
  then
    /root/bin/send-push-message "$NOTIFICATION_TITLE:" "$message" "" music_sync
  else
    log "$message"
  fi
}

if ! do_music_sync
then
  log "Error while syncing music"
fi
