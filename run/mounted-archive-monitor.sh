#!/bin/bash -eu

# MOUNTED_ARCHIVE_MONITOR_V1: dual-signal connection monitor for mounted
# (CIFS/NFS) clip archiving. Sourced by archive-clips.sh.
#
# The previous monitor killed rsync after five consecutive failed port
# probes (~35s). A probe only tests whether a NEW TCP connection can be
# opened; at the edge of WiFi range a loss burst blocks fresh SYNs while
# the established SMB/NFS session keeps retransmitting and delivering, so
# transfers that would have completed were aborted mid-run.
#
# Liveness is now either signal:
#   - a successful reachability probe, or
#   - a new completed-file record in rsync's --log-file. The rsync
#     invocation must pass a --log-file-format containing %b: a
#     transfer-statistic escape makes rsync log at the END of each file's
#     transfer, where the default format logs before it. Received regular
#     files itemize as ">f"; pre-transfer names, vanished-file warnings
#     and directory records never match.
#
# Kill when both signals have been absent for ARCHIVE_STALL_GRACE_SECONDS
# (default 60, 180 under Travel Mode, clamped 30-240), or unconditionally
# after 300s of continuous probe failure: writes into a dead CIFS mount
# can complete into page cache for a while, so completed-file records can
# lag reality — the ceiling bounds a real drive-away so archiveloop gets
# the gadget back to normal housekeeping.
#
# The kill is scoped to this run's rsync via its unique --log-file path;
# `killall rsync` would also take out an unrelated music or media sync.

MONITOR_RSYNC_LOG=/tmp/archive-rsync-cmd.log

function connectionmonitor {
  local grace
  local -r hard_cap=300
  if [ "${TRAVEL_MODE_ACTIVE:-0}" = "1" ]
  then
    grace="${ARCHIVE_STALL_GRACE_SECONDS:-180}"
  else
    grace="${ARCHIVE_STALL_GRACE_SECONDS:-60}"
  fi
  case "$grace" in
    ''|*[!0-9]*) grace=60 ;;
  esac
  if [ "$grace" -lt 30 ]; then grace=30; fi
  if [ "$grace" -gt 240 ]; then grace=240; fi

  local now probe_down stalled
  local probe_failed_since=0
  local completed prev_completed=0
  local last_progress
  last_progress=$(date +%s)

  while true
  do
    if timeout 6 /root/bin/archive-is-reachable.sh "$ARCHIVE_SERVER"
    then
      probe_failed_since=0
    elif [ "$probe_failed_since" = 0 ]
    then
      probe_failed_since=$(date +%s)
    fi

    completed=$(grep -c ' >f' "$MONITOR_RSYNC_LOG" 2>/dev/null || true)
    completed="${completed:-0}"
    if [ "$completed" != "$prev_completed" ]
    then
      # != rather than -gt: archive-clips.sh truncates the log per run, so
      # a shrinking count is a fresh run, not a stall.
      prev_completed=$completed
      last_progress=$(date +%s)
    fi

    if [ "$probe_failed_since" != 0 ]
    then
      now=$(date +%s)
      probe_down=$(( now - probe_failed_since ))
      stalled=$(( now - last_progress ))
      if [ "$probe_down" -ge "$hard_cap" ] || { [ "$probe_down" -ge "$grace" ] && [ "$stalled" -ge "$grace" ]; }
      then
        log "connection dead (probes failing ${probe_down}s, no completed file for ${stalled}s), killing archive-clips"
        # TERM first: rsync may still delete already-archived source files.
        pkill -f 'rsync .*--log-file=/tmp/archive-rsync-cmd\.log' || true
        sleep 2
        pkill -9 -f 'rsync .*--log-file=/tmp/archive-rsync-cmd\.log' || true
        kill -9 "$1" || true
        return
      fi
    fi
    sleep 5
  done
}
