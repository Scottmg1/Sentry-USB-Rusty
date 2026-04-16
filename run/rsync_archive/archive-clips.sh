#!/bin/bash -eu

while [ -n "${1+x}" ]
do
  if ! (rsync -avhRL --timeout=600 --remove-source-files --no-perms --omit-dir-times \
        --stats --log-file=/tmp/archive-rsync-cmd.log --ignore-missing-args \
        --files-from="$2" "$1" "$RSYNC_USER@$RSYNC_SERVER:$RSYNC_PATH" &> /tmp/rsynclog || [[ "$?" = "24" ]] )
  then
    cat /tmp/archive-rsync-cmd.log /tmp/rsynclog > /tmp/archive-error.log
    exit 1
  fi
  shift 2
done
