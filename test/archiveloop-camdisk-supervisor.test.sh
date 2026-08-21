#!/bin/bash
# Regression test for the cam-disk worker supervisor (camdiskworkers).
#
# archiveloop runs under `bash -eu`, and `wait -n` returns the dead
# worker's nonzero exit status. Without the `|| true` guard, a worker
# crash (e.g. OOM-killed freespacemanager, the process that watches for
# /mutable inode exhaustion) killed the supervisor itself before it
# could log, reap, or restart anything — leaving the service "active"
# with no space management for the rest of the boot. This test stubs
# the trio, crashes one worker with exit 7 on its first spawn, and
# asserts the supervisor restarts it.
set -euo pipefail

script=${1:-run/archiveloop}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

eval "$(awk '/^function camdiskworkers /{keep=1} keep{print} keep && /^}/{exit}' "$script")"

function log { echo "$@" >> "$tmp/log"; }
function has_cam_disk { return 0; }
function snapshotloop { sleep 60; }
function gadget_stall_watchdog { sleep 60; }
function freespacemanager {
  echo x >> "$tmp/fsm_spawns"
  if [ "$(wc -l < "$tmp/fsm_spawns")" -eq 1 ]
  then
    exit 7 # simulate a crash on the first spawn only
  fi
  sleep 60
}

camdiskworkers > /dev/null 2>&1 &
sup=$!

# The supervisor sleeps 10s between restarts; allow up to 20s.
for _ in $(seq 1 40)
do
  if [ -f "$tmp/fsm_spawns" ] && [ "$(wc -l < "$tmp/fsm_spawns")" -ge 2 ]
  then
    break
  fi
  sleep 0.5
done

kill "$sup" 2>/dev/null || true

if [ ! -f "$tmp/fsm_spawns" ] || [ "$(wc -l < "$tmp/fsm_spawns")" -lt 2 ]
then
  echo "FAIL: supervisor did not restart the worker trio after a crash (spawns: $(wc -l < "$tmp/fsm_spawns" 2>/dev/null || echo 0))"
  exit 1
fi
if ! grep -q "restarting snapshot/space/watchdog workers" "$tmp/log"
then
  echo "FAIL: supervisor restarted workers but never logged the restart"
  exit 1
fi

echo "camdisk supervisor restart test passed"
