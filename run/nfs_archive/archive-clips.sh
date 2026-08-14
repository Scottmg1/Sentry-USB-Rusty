#!/bin/bash -eu

# MOUNTED_ARCHIVE_MONITOR_V1: the connection watchdog lives in
# mounted-archive-monitor.sh (shared with the CIFS variant) and only kills
# rsync when probes fail AND no file has completed for a grace window —
# probe-only monitoring aborted healthy transfers on lossy WiFi links.
source /root/bin/mounted-archive-monitor.sh

# Truncate before the monitor starts: a stale log from the previous run
# would seed the monitor's completed-file counter with the old total.
rm -f /tmp/archive-rsync-cmd.log /tmp/archive-error.log

connectionmonitor $$ &

# rsync's temp files may be left behind if the connection is lost,
# but rsync doesn't clean these up on subsequent runs. Putting
# them in a temp dir allows them to be easily cleaned up.
rsynctmp=".sentryusbtmp"
rm -rf "$ARCHIVE_MOUNT/${rsynctmp:?}" || true
mkdir -p "$ARCHIVE_MOUNT/$rsynctmp"

while [ -n "${1+x}" ]
do
  # Using --no-o --no-g to prevent permission errors on NFS root squashed shares
  # Low I/O + CPU priority so the archive reads never starve the car's
  # dashcam writes on the same disk (see run/rsync_archive/archive-clips.sh
  # for the full rationale; -c2 -n7 not -c3 so progress is guaranteed).
  # --log-file-format contains %b so records are written at the END of each
  # file's transfer — the watchdog counts them as its progress signal.
  if ! (ionice -c2 -n7 nice -n19 rsync -avhRL --no-o --no-g --remove-source-files --temp-dir="$rsynctmp" --no-perms --omit-dir-times --stats \
        --log-file=/tmp/archive-rsync-cmd.log --log-file-format='%i %b %n%L' --ignore-missing-args \
        --files-from="$2" "$1/" "$ARCHIVE_MOUNT" &> /tmp/rsynclog || [[ "$?" = "24" ]] )
  then
    cat /tmp/archive-rsync-cmd.log /tmp/rsynclog > /tmp/archive-error.log
    exit 1
  fi

  shift 2
done

rm -rf "$ARCHIVE_MOUNT/${rsynctmp:?}" || true

kill %1 || true
