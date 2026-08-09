#!/bin/bash -eu

if [ "${BASH_SOURCE[0]}" != "$0" ]
then
  echo "${BASH_SOURCE[0]} must be executed, not sourced"
  return 1 # shouldn't use exit when sourced
fi

if [ "${FLOCKED:-}" != "$0" ]
then
  mkdir -p /backingfiles/snapshots
  if FLOCKED="$0" flock -E 99 /backingfiles/snapshots "$0" "$@" || case "$?" in
  99) echo "failed to lock snapshots dir"
      exit 99
      ;;
  *)  exit $?
      ;;
  esac
  then
    # success
    exit 0
  fi
fi

# archiveloop exports its own `log` to children (export -f log), so the
# normal path inherits it. A HAND invocation does not, and every log
# call would then abort the script under `set -e` — right before the
# delete, so a troubleshooting operator got `log: command not found` and
# no cleanup. Define a stdout fallback only when nothing was inherited.
if ! declare -F log > /dev/null 2>&1
then
  function log () {
    echo "$( date ):" "$@"
  }
fi

function manage_free_space {
  # Try to make free space equal to 10 GB plus three percent of the total
  # available space. This should be enough to hold the next hour of
  # recordings without completely filling up the filesystem.
  # todo: this could be put in a background task and with a lower free
  # space requirement, to delete old snapshots just before running out
  # of space and thus make better use of space
  local reserve="$1"
  while true
  do
    local freespace
    freespace=$(eval "$(stat --file-system --format="echo \$((%f*%S))" /backingfiles/cam_disk.bin)")
    if [ "$freespace" -gt "$reserve" ]
    then
      exit 0
    fi
    # Candidates = snapshot dirs that actually hold a snap.bin, ordered by
    # the bin's mtime (TRUE age), name as tie-break. Slot numbers are not
    # time-monotonic in the field — a reflash can leave a stale
    # high-numbered snapshot above a restarted sequence — and the old
    # name-order pick (`sort | head -1`) deleted newer footage while
    # sparing the genuinely oldest snapshot. The same list feeds the
    # "fewer than two" guard so the count and the pick can't disagree.
    local candidates
    # `-regex` (not just snap-*) so a scratch dir like snap-backup/ is
    # never a deletion candidate — matches the numeric-only rule the
    # Rust scanners use.
    candidates=$(
      LC_ALL=C find /backingfiles/snapshots \
        -mindepth 2 -maxdepth 2 -type f \
        -regex '/backingfiles/snapshots/snap-[0-9]+/snap\.bin' -regextype posix-extended \
        -printf '%T@\t%h\n' 2>/dev/null |
      LC_ALL=C sort -t $'\t' -k1,1n -k2,2
    )
    if [ -z "$candidates" ]
    then
      log "Warning: low space for new snapshots, but no snapshots exist."
      log "Please use a larger storage medium or reduce CAM_SIZE"
      exit 1
    fi
    # if there's only one snapshot then we likely just took it, so don't immediately delete it
    if [ "$(printf '%s\n' "$candidates" | wc -l)" -lt 2 ]
    then
      # there's only one snapshot and yet we're low on space
      log "Warning: low space for new snapshots, but only one snapshot exists."
      log "Please use a larger storage medium or reduce CAM_SIZE"
      exit 1
    fi

    # Never delete the HIGHEST-numbered snapshot, even if its mtime says
    # it is the oldest. This board usually has no battery-backed RTC and
    # archiveloop starts freespacemanager BEFORE timesyncloop, so a boot
    # can run eviction while the clock still holds a fake-hwclock time in
    # the past — the snapshot just taken then sorts as oldest and would
    # be the first deleted. Slot numbers are allocated monotonically, so
    # the highest number is the most recent creation whatever the clock
    # said. Mirrors `releasable()` in crates/usb_gadget/src/space.rs.
    # Withhold BOTH the highest-numbered slot and the newest-by-mtime,
    # matching releasable() in space.rs. (Protecting only the highest
    # would still let a long low-space run delete the mtime-newest.)
    local highest newest
    highest=$(printf '%s\n' "$candidates" | cut -f2- | sed 's#.*/##' \
              | grep -E '^snap-[0-9]+$' | LC_ALL=C sort | tail -1)
    newest=$(printf '%s\n' "$candidates" | tail -1 | cut -f2- | sed 's#.*/##')
    oldest=$(printf '%s\n' "$candidates" | cut -f2- \
             | grep -v "/${highest}\$" | grep -v "/${newest}\$" | head -1)
    if [ -z "$oldest" ]
    then
      log "unable to select oldest snapshot (only protected snapshots remain)"
      exit 1
    fi
    log "low space, deleting $oldest (oldest by snap.bin mtime)"
    /root/bin/release_snapshot.sh "$oldest"
    rm -rf "$oldest"
  done
}

# Normally called by archiveloop's freespacemanager with "10G + 3% of
# total space". When invoked by hand with no argument, compute the SAME
# policy rather than falling back to a flat 20G: a size-blind default is
# harsher than the real policy on small drives and far weaker on
# multi-TB ones, which is exactly the vintage-dependent divergence this
# shares with crates/usb_gadget/src/space.rs (default_reserve_bytes).
reserve="${1:-}"
if [ -z "$reserve" ]
then
  reserve=$(eval "$(stat --file-system --format="echo \$((10737418240 + %b*%S/33))" /backingfiles/cam_disk.bin)")
fi
manage_free_space "$reserve"
