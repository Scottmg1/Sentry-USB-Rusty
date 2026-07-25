#!/bin/bash -eu

# read the setup variables again because arrays, like RCLONE_FLAGS, don't export to subshells/child scripts
source /root/bin/envsetup.sh

# RCLONE_MONITOR_V2: probes ARCHIVE_SERVER, travel-mode aware, scoped kill.
#
# Connection monitor: poll the liveness IP every ~10s. Consecutive misses
# kill rclone (and this script) so archiveloop can reach
# `connect_usb_drives_to_host` and put the gadget back online instead of
# hanging on a dropped TCP/cloud connection while the user drives away.
# The `--timeout`/`--contimeout` flags below give rclone its own internal
# floor; the monitor is a hard outer bound for cases where rclone's retry
# loop takes too long to surrender.
#
# The probe target is ARCHIVE_SERVER (a pingable IP, 8.8.8.8 by default for
# cloud remotes — see run/archiveloop), NOT $RCLONE_DRIVE: that is rclone's
# remote *name* (e.g. "gdrive"), which is not a hostname and can never
# answer a ping, so probing it killed every archive run within seconds.
#
# Travel Mode (passed fresh by archiveloop as TRAVEL_MODE_ACTIVE) relaxes the
# thresholds for slow, high-latency mobile links, mirroring
# run/rsync_archive/archive-clips.sh. Normal mode keeps the original snappy
# values so "drive away from home" recovery is unchanged.
if [ "${TRAVEL_MODE_ACTIVE:-0}" = "1" ]; then
  MONITOR_MISSES=20            # ~minutes of sustained loss before giving up
  MONITOR_TIMEOUT=20           # must exceed the patient probe below
  export ARCHIVE_PING_TIMEOUT=4
else
  MONITOR_MISSES=5
  MONITOR_TIMEOUT=6
fi

function connectionmonitor {
  while true
  do
    for (( i = 1; i <= MONITOR_MISSES; i++ ))
    do
      if timeout "$MONITOR_TIMEOUT" /root/bin/archive-is-reachable.sh "${ARCHIVE_SERVER:-8.8.8.8}"
      then
        sleep 5
        continue 2
      fi
      sleep 1
    done
    log "connection dead, killing rclone archive"
    # Scoped to this script's own move invocation: a plain `killall rclone`
    # would also take out unrelated rclone processes (the drive-data sync in
    # post-archive-process.sh, an in-flight cloud config backup).
    pkill -f 'rclone --config /root/\.config/rclone/rclone\.conf move' || true
    sleep 2
    pkill -9 -f 'rclone --config /root/\.config/rclone/rclone\.conf move' || true
    kill -9 "$1" || true
    return
  done
}

connectionmonitor $$ &

# Layer-1 (rclone-level) safety nets. The bash monitor is layer-2.
flags=("-L" "--transfers=1" "--timeout=30s" "--contimeout=10s" "--retries=1")
if [[ -v RCLONE_FLAGS ]]
then
  flags+=("${RCLONE_FLAGS[@]}")
fi

while [ -n "${1+x}" ]
do
  # Low I/O + CPU priority so the archive reads never starve the car's
  # dashcam writes on the same disk (see run/rsync_archive/archive-clips.sh
  # for the full rationale; -c2 -n7 not -c3 so progress is guaranteed).
  ionice -c2 -n7 nice -n19 rclone --config /root/.config/rclone/rclone.conf move "${flags[@]}" --files-from "$2" "$1" "$RCLONE_DRIVE:$RCLONE_PATH" >> "$LOG_FILE" 2>&1
  shift 2
done

# Stop the monitor so it doesn't leak past archive completion.
kill %1 || true
