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

# Free inodes on /mutable, or "" if it can't be read (dev containers).
# Releasing a snapshot also deletes its /mutable/TeslaCam symlinks, so
# snapshot eviction is what relieves inode pressure there too.
function mutable_free_inodes {
  stat --file-system --format=%d /mutable 2>/dev/null || true
}

# Inode-driven eviction requires /mutable mounted READ-WRITE, not just
# mounted: on an ext4-error ro remount, release_snapshot.sh deletes the
# snapshot first and then cannot remove its symlinks — permanent footage
# loss with zero inode recovery. (A write-probe would be wrong here: an
# inode-FULL filesystem also fails writes, and that is exactly the state
# eviction is meant to fix — so check the mount option, not a write.)
function mutable_rw_mounted {
  local opts
  opts=$(findmnt -no OPTIONS --mountpoint /mutable 2>/dev/null) || return 1
  case ",$opts," in
    *,rw,*) return 0 ;;
    *) return 1 ;;
  esac
}

# Written when snapshot release stopped relieving inode pressure (the
# stall guard below); holds the free-inode count at the moment of the
# stall. tmpfs on purpose: /mutable itself may be inode-full and the
# root filesystem is read-only. Cleared once free inodes rise above the
# recorded value, i.e. something actually got freed. Without this
# latch, archiveloop's 30-second freespacemanager retry would re-enter
# and delete three more snapshots per attempt — draining the entire
# snapshot store against pressure that snapshots cannot relieve.
INODE_STALL_LATCH=/run/sentryusb_inode_stall

# Give up on inode-driven eviction and stop the retry treadmill.
#
# Called from the branches that cannot release anything. When the BLOCK
# target is already met, the only thing driving eviction is inode
# pressure, and no further attempt can help: latch the verdict so
# archiveloop's 30s retry stops re-entering (shipped v3.20.x logged the
# same warning every 60 seconds indefinitely) and so the health check
# can surface it. Also keeps the advice honest: "use a larger storage
# medium or reduce CAM_SIZE" is wrong for inode pressure — neither adds
# inodes to an existing filesystem.
function halt_inode_eviction () {
  local ifree_now
  ifree_now=$(mutable_free_inodes)
  echo "${ifree_now:-0}" > "$INODE_STALL_LATCH" 2>/dev/null || true
  log "Clip index (/mutable inodes) is low but no snapshot can be released to relieve it."
  log "Snapshot cleanup cannot add inodes to an existing filesystem; reformatting /mutable is the only way to enlarge the clip index. Not retrying automatically."
}

function manage_free_space {
  # Try to make free space equal to 10 GB plus three percent of the total
  # available space. This should be enough to hold the next hour of
  # recordings without completely filling up the filesystem.
  # todo: this could be put in a background task and with a lower free
  # space requirement, to delete old snapshots just before running out
  # of space and thus make better use of space
  local reserve="$1"

  # /mutable inode headroom target: max(20000, table/20). Every
  # retained clip costs one symlink inode in /mutable/TeslaCam, and the
  # table (~121k at mkfs defaults on the stock partition) fills long
  # before a multi-TB /backingfiles feels block-space pressure — the
  # 2026-08-19 field failure ran 100% inode-full for a day: ln/state
  # writes all ENOSPC'd while df -h showed 71% free. At the observed
  # ~2.9k links/day, 20k free inodes is about a week of recovery
  # headroom; the /20 term tracks that rather than growing with denser
  # tables. rw-mount check: an unmounted /mutable would make stat
  # measure the root filesystem, and a ro-remounted one turns eviction
  # into pure footage loss (see mutable_rw_mounted). Matches
  # archiveloop's freespacemanager and crates/usb_gadget/src/space.rs.
  local ireserve=0
  local ifree itotal
  if mutable_rw_mounted
  then
    itotal=$(stat --file-system --format=%c /mutable 2>/dev/null || echo 0)
    if [ "$itotal" -gt 0 ]
    then
      ireserve=$((itotal / 20))
      if [ "$ireserve" -lt 20000 ]
      then
        ireserve=20000
      fi
      # Cap the target at a quarter of the table so it is always
      # REACHABLE. Single-disk installs size /mutable's inode table from
      # the data area (backingfiles_sectors/20000), so a 128GB card has
      # ~11.8k inodes TOTAL — less than the 20k floor. Shipped
      # v3.20.0-v3.20.8 demanded 20k free from that table, which no
      # amount of eviction can reach: cleanup deleted every releasable
      # snapshot and then failed on a 60-second loop forever. Two users
      # lost 23 and 45 snapshots of real footage. Leaves the 120,960 and
      # 472,000 tables byte-identical to the value that fixed the
      # original 2026-08-19 incident.
      if [ "$ireserve" -gt $((itotal / 4)) ]
      then
        ireserve=$((itotal / 4))
      fi
      # Defence in depth: never evict toward a target the filesystem
      # cannot satisfy, whatever a future edit to the formula does.
      if [ "$ireserve" -ge "$itotal" ]
      then
        log "inode reserve $ireserve >= /mutable inode table $itotal — target unreachable, disabling inode-driven eviction"
        ireserve=0
      fi
    fi
  fi

  # Honor a previous stall verdict: resume inode-driven eviction only
  # after free inodes actually rose above the latched value.
  if [ "$ireserve" -gt 0 ] && [ -e "$INODE_STALL_LATCH" ]
  then
    local latched
    latched=$(cat "$INODE_STALL_LATCH" 2>/dev/null || echo 0)
    ifree=$(mutable_free_inodes)
    if [ -n "$ifree" ] && [ "$ifree" -gt "$latched" ]
    then
      rm -f "$INODE_STALL_LATCH"
    else
      log "inode-driven eviction suspended (stalled at $latched free; see $INODE_STALL_LATCH)"
      ireserve=0
    fi
  fi

  # Consecutive releases that freed no /mutable inodes while the block
  # target was already met. Snapshot eviction only helps inode pressure
  # when the snapshot still owns symlinks; if something else ate the
  # table, deleting more footage cannot fix it — stop instead.
  local -i stale_releases=0

  while true
  do
    local freespace
    freespace=$(eval "$(stat --file-system --format="echo \$((%f*%S))" /backingfiles/cam_disk.bin)")
    ifree=$(mutable_free_inodes)
    # Done when the byte target is met AND the inode target is met (or
    # the inode policy is off: not mounted, unreadable, or latched).
    if [ "$freespace" -gt "$reserve" ] && \
       { [ "$ireserve" -eq 0 ] || [ -z "$ifree" ] || [ "$ifree" -gt "$ireserve" ]; }
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
        -regextype posix-extended -regex '/backingfiles/snapshots/snap-[0-9]+/snap\.bin' \
        -printf '%T@\t%h\n' 2>/dev/null |
      LC_ALL=C sort -t $'\t' -k1,1n -k2,2
    )
    if [ -z "$candidates" ]
    then
      if [ "$freespace" -gt "$reserve" ]
      then
        halt_inode_eviction
        exit 1
      fi
      log "Warning: low space for new snapshots, but no snapshots exist."
      log "Please use a larger storage medium or reduce CAM_SIZE"
      exit 1
    fi
    # if there's only one snapshot then we likely just took it, so don't immediately delete it
    if [ "$(printf '%s\n' "$candidates" | wc -l)" -lt 2 ]
    then
      if [ "$freespace" -gt "$reserve" ]
      then
        halt_inode_eviction
        exit 1
      fi
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
      if [ "$freespace" -gt "$reserve" ]
      then
        halt_inode_eviction
        exit 1
      fi
      log "unable to select oldest snapshot (only protected snapshots remain)"
      exit 1
    fi
    log "low space, deleting $oldest (oldest by snap.bin mtime)"
    local ifree_before="$ifree"
    /root/bin/release_snapshot.sh "$oldest"
    rm -rf "$oldest"

    # Stall guard: only meaningful when block space is already satisfied
    # and we are evicting purely for /mutable inodes. Three snapshots in
    # a row yielding zero freed inodes means the pressure isn't from
    # clip symlinks; latch that verdict (so retrying callers don't drain
    # the snapshot store three at a time) and bail.
    if [ "$ireserve" -gt 0 ] && [ "$freespace" -gt "$reserve" ] && [ -n "$ifree_before" ]
    then
      ifree=$(mutable_free_inodes)
      if [ -n "$ifree" ] && [ "$ifree" -le "$ifree_before" ]
      then
        stale_releases+=1
        if [ "$stale_releases" -ge 3 ]
        then
          echo "$ifree" > "$INODE_STALL_LATCH" 2>/dev/null || true
          log "inode pressure on /mutable not relieved by snapshot release ($ifree free of $itotal); something other than clip symlinks is consuming inodes"
          exit 1
        fi
      else
        stale_releases=0
      fi
    fi
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
