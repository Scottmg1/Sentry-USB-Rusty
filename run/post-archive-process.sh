#!/bin/bash
# Post-archive hook: process newly archived dashcam clips for GPS/drive data.
# Called by archiveloop after archive_clips completes, before awake_stop.
# Only runs if DRIVE_MAP_ENABLED is set to true in the config.
#
# Error handling: -u catches typos in variable names; -o pipefail makes
# size-check pipelines (wc | tr) fail loudly instead of silently producing
# empty strings that the size-guard would then treat as "0 bytes = allow".
# We deliberately do NOT set -e: the script is structured to tolerate
# individual curl/grep failures and continue with fallbacks, and flipping
# that behavior would turn every transient API hiccup into a skipped sync.
set -uo pipefail

source /root/bin/envsetup.sh 2>/dev/null || true

# Default to enabled. Users who want manual control (testing, or to
# avoid coupling archive completion to drive-DB writes) can set
# DRIVE_MAP_ENABLED=false in /root/sentryusb.conf to opt out.
if [ "${DRIVE_MAP_ENABLED:-true}" != "true" ]; then
  exit 0
fi

LOG_FILE="${LOG_FILE:-/mutable/archiveloop.log}"

function log() {
  echo "$(date): [drive-map] $*" >> "$LOG_FILE"
}

# --- drive-data.json archive size-guard -------------------------------------
# Shared with the Rust server's SyncToArchive(): both update the same cache at
# DRIVE_DATA_SYNC_CACHE so the guard stays consistent across CIFS/NFS (Rust)
# and rsync/rclone (shell) archive backends.
#
# The cache holds "v2:<byte count>" of the last successful sync. Before
# each sync we refuse to overwrite if the new local file is less than 50%
# of that recorded size AND the recorded size is above 10 MB. This catches
# the failure mode where a corrupted/empty local drive-data.json would
# otherwise blow away a healthy archive backup.
#
# The v2: prefix versions the BASELINE FORMAT: pre-v2 baselines were bare
# integers measured against the pretty-printed export, roughly 3x larger
# than today's compact one — honoring them across the format change flagged
# every upgrader's first compact export as phantom corruption and blocked
# archive sync forever. Bare integers therefore read as "no baseline"
# (fail open, re-baseline on next success). Keep in lockstep with
# crates/drives/src/syncguard.rs.
DRIVE_DATA_SYNC_CACHE="/mutable/.drive-data-last-sync"
DRIVE_DATA_SYNC_MIN_THRESHOLD=$((10 * 1024 * 1024)) # 10 MB

# Local path of the regenerated drive-data.json mirror. Lives on /backingfiles
# alongside the SQLite DB so the 2 GB /mutable partition can't be filled by
# the export (which can reach hundreds of MB on a long-used Pi). Kept after
# upload so rsync's delta-transfer protocol only ships changed bytes each cycle.
DRIVE_DATA_JSON="/backingfiles/drive-data.json"

# drive_data_size_guard_ok <local_file> <destination_label>
# Returns 0 (allow) if the sync may proceed, 1 (refuse) otherwise.
# On refuse, logs the reason and sends a mobile notification.
function drive_data_size_guard_ok() {
  local local_file="$1"
  local dest_label="$2"

  if [ ! -f "$local_file" ]; then
    # Nothing to sync; caller will no-op. Let it through.
    return 0
  fi

  local new_size
  new_size=$(wc -c < "$local_file" 2>/dev/null | tr -d ' ')
  new_size=${new_size:-0}

  local last_size=0
  if [ -f "$DRIVE_DATA_SYNC_CACHE" ]; then
    last_size=$(cat "$DRIVE_DATA_SYNC_CACHE" 2>/dev/null | tr -d ' \n')
    # Require the v2 format — bare-integer (pre-v2) or corrupt baselines
    # fail open and re-baseline on the next successful sync.
    if [[ "$last_size" =~ ^v2:([0-9]+)$ ]]; then
      last_size=${BASH_REMATCH[1]}
    else
      last_size=0
    fi
  fi

  # No baseline → allow (first-ever sync, or cache corrupted/missing).
  if [ "$last_size" -le 0 ]; then
    return 0
  fi
  # Tiny baseline → allow (nothing big to protect yet).
  if [ "$last_size" -lt "$DRIVE_DATA_SYNC_MIN_THRESHOLD" ]; then
    return 0
  fi
  # new >= last/2 → allow (integer math; boundary matches Rust-side).
  local half=$((last_size / 2))
  if [ "$new_size" -ge "$half" ]; then
    return 0
  fi

  log "SIZE GUARD: refusing ${dest_label} sync — new=${new_size} bytes < 50% of last-good=${last_size} bytes. Local file may be corrupted; archive preserved."
  if [ -x /root/bin/send-push-message ]; then
    /root/bin/send-push-message "${NOTIFICATION_TITLE:-SentryUSB}:" \
      "Drive data sync blocked — local file shrunk to $((new_size / 1024 / 1024)) MB from $((last_size / 1024 / 1024)) MB. Archive backup preserved. Check ${DRIVE_DATA_JSON}." \
      warning drives > /dev/null 2>&1 || true
  fi
  return 1
}

# update_drive_data_sync_cache <local_file>
# Record the byte size of local_file as the new baseline, atomically.
# Called after every successful sync (Rust-side writes the same file).
function update_drive_data_sync_cache() {
  local local_file="$1"
  if [ ! -f "$local_file" ]; then
    return 0
  fi
  local size
  size=$(wc -c < "$local_file" 2>/dev/null | tr -d ' ')
  size=${size:-0}
  local tmp="${DRIVE_DATA_SYNC_CACHE}.tmp"
  if echo -n "v2:$size" > "$tmp" 2>/dev/null; then
    mv -f "$tmp" "$DRIVE_DATA_SYNC_CACHE" 2>/dev/null || rm -f "$tmp"
  fi
}
# ----------------------------------------------------------------------------

# Find the SentryUSB API port
SENTRYUSB_PORT="${SENTRYUSB_PORT:-80}"
API_URL="http://127.0.0.1:${SENTRYUSB_PORT}"

# Wait for the SentryUSB API to become reachable (it may still be starting
# after a reboot, or briefly unavailable during an update).
API_READY=false
for i in 1 2 3 4 5 6; do
  if curl -sf "${API_URL}/api/drives/status" > /dev/null 2>&1; then
    API_READY=true
    break
  fi
  log "SentryUSB API not reachable (attempt $i/6), retrying in 5s..."
  sleep 5
done

if [ "$API_READY" != "true" ]; then
  log "SentryUSB API not reachable after 30s, skipping drive processing"
  exit 0
fi

# Clear archive status so the processing API doesn't think archiving is
# still in progress.  archive_clips is finished by the time this script
# runs, but the status file may not have been cleaned up yet.
rm -f /tmp/archive_status.json /tmp/archive_status.json.tmp

# Process a single clips directory: trigger API, wait for completion
function process_clips_dir() {
  local clips_dir="$1"
  log "Starting drive processing on $clips_dir"

  # Retry up to 6 times (60s) if the server is busy (409 = already running or archiving)
  local max_retries=6
  local retry_wait=10
  local attempt=0
  local HTTP_CODE RESPONSE

  while [ $attempt -lt $max_retries ]; do
    HTTP_CODE=$(curl -s -o /tmp/drive_process_response.json -w "%{http_code}" \
      -X POST "${API_URL}/api/drives/process?post_archive=1" \
      -H "Content-Type: application/json" \
      -d "{\"clips_dir\": \"${clips_dir}\", \"throttle_ms\": 20}" 2>/dev/null)
    RESPONSE=$(cat /tmp/drive_process_response.json 2>/dev/null)

    if [ "$HTTP_CODE" = "200" ] || [ "$HTTP_CODE" = "202" ]; then
      break
    elif [ "$HTTP_CODE" = "409" ]; then
      attempt=$((attempt + 1))
      log "Processing busy (409), retrying in ${retry_wait}s (attempt $attempt/$max_retries): $RESPONSE"
      sleep $retry_wait
    else
      log "Failed to trigger processing for $clips_dir (HTTP $HTTP_CODE): $RESPONSE"
      return 1
    fi
  done

  if [ $attempt -ge $max_retries ]; then
    log "Failed to trigger processing for $clips_dir: still busy after ${max_retries} retries"
    return 1
  fi

  log "Processing triggered: $RESPONSE"

  local total_estimate
  total_estimate=$(echo "$RESPONSE" | grep -o '"total":[0-9]*' | cut -d: -f2 || echo "0")

  local timeout=1800
  local elapsed=0
  local poll_interval=10
  local last_progress_log=0

  while [ $elapsed -lt $timeout ]; do
    sleep $poll_interval
    elapsed=$((elapsed + poll_interval))

    STATUS=$(curl -sf "${API_URL}/api/drives/status" 2>/dev/null)
    if [ $? -ne 0 ]; then
      log "Failed to check status, continuing to wait..."
      continue
    fi

    RUNNING=$(echo "$STATUS" | grep -o '"running":true' || true)
    if [ -z "$RUNNING" ]; then
      ROUTES=$(echo "$STATUS" | grep -o '"routes_count":[0-9]*' | cut -d: -f2)
      PROCESSED=$(echo "$STATUS" | grep -o '"processed_count":[0-9]*' | cut -d: -f2)
      log "Processing complete for $clips_dir. Routes: ${ROUTES:-0}, Files processed: ${PROCESSED:-0}"
      return 0
    fi

    # Log progress every 15 seconds
    if [ $((elapsed - last_progress_log)) -ge 15 ]; then
      PROCESSED=$(echo "$STATUS" | grep -o '"processed_count":[0-9]*' | cut -d: -f2)
      ROUTES=$(echo "$STATUS" | grep -o '"routes_count":[0-9]*' | cut -d: -f2)
      log "Still processing $clips_dir... (${elapsed}s elapsed, ${PROCESSED:-?} files processed, ${ROUTES:-?} routes)"
      last_progress_log=$elapsed
    fi
  done

  log "Processing timed out for $clips_dir after ${timeout}s"
  return 1
}

# Process RecentClips from local snapshot storage (not NFS archive, not live cam)
CLIPS_DIR="/mutable/TeslaCam/RecentClips"
if [ ! -d "$CLIPS_DIR" ]; then
  log "RecentClips directory not found at $CLIPS_DIR, skipping"
  exit 0
fi

# Capture drive count before processing so we can detect new data
BEFORE_STATS=$(curl -sf "${API_URL}/api/drives/stats" 2>/dev/null)
DRIVES_BEFORE=$(echo "$BEFORE_STATS" | grep -o '"drives_count":[0-9]*' | cut -d: -f2)
DRIVES_BEFORE=${DRIVES_BEFORE:-0}

# Baseline from the end of the last reachable post-archive cycle. The delta
# between this baseline and DRIVES_BEFORE is what snapshotloop mapped silently
# while the archive was unreachable (DRIVE_MAP_WHILE_AWAY=true).
LAST_REACHABLE_STATS_FILE="/mutable/.last-reachable-drives-stats"
LAST_REACHABLE_EXISTS=false
LAST_DRIVES_COUNT=0
LAST_DIST_MI=0
LAST_DIST_KM=0
if [ -f "$LAST_REACHABLE_STATS_FILE" ]; then
  LAST_REACHABLE_EXISTS=true
  LAST_DRIVES_COUNT=$(grep -o '"drives_count":[0-9]*' "$LAST_REACHABLE_STATS_FILE" | cut -d: -f2)
  LAST_DRIVES_COUNT=${LAST_DRIVES_COUNT:-0}
  LAST_DIST_MI=$(grep -o '"total_distance_mi":[0-9.]*' "$LAST_REACHABLE_STATS_FILE" | cut -d: -f2)
  LAST_DIST_MI=${LAST_DIST_MI:-0}
  LAST_DIST_KM=$(grep -o '"total_distance_km":[0-9.]*' "$LAST_REACHABLE_STATS_FILE" | cut -d: -f2)
  LAST_DIST_KM=${LAST_DIST_KM:-0}
fi

process_clips_dir "$CLIPS_DIR"
PROCESSED=$?

log "Drive processing complete. $PROCESSED directories processed."

# Check if archive is still reachable before syncing drive data.
# If the user drove away during processing, skip the sync — it will
# be retried on the next archive cycle.
#
# Travel Mode: relax the probe timeouts (same values as the archive-clips.sh
# watchdog) so a slow VPN link isn't misread as "drove away", which would
# needlessly skip the drive-data sync every cycle. envsetup.sh (sourced at
# the top) provides TRAVEL_MODE_ENABLED fresh for each invocation.
case "${TRAVEL_MODE_ENABLED:-}" in
  [yY][eE][sS]|[tT][rR][uU][eE]|1|[oO][nN])
    export ARCHIVE_PING_TIMEOUT="${ARCHIVE_PING_TIMEOUT:-4}"
    export ARCHIVE_SSH_TIMEOUT="${ARCHIVE_SSH_TIMEOUT:-8}"
    ;;
esac
ARCHIVE_REACHABLE=true
if [ -x /root/bin/archive-is-reachable.sh ]; then
  ARCHIVE_SERVER="${ARCHIVE_SERVER:-}"
  # Derive ARCHIVE_SERVER from archive type if not set
  if [ -z "$ARCHIVE_SERVER" ]; then
    if [ -n "${RSYNC_SERVER:-}" ]; then
      ARCHIVE_SERVER="$RSYNC_SERVER"
    elif [ -n "${RCLONE_DRIVE:-}" ]; then
      ARCHIVE_SERVER="8.8.8.8"
    fi
  fi
  if [ -n "$ARCHIVE_SERVER" ]; then
    if ! /root/bin/archive-is-reachable.sh "$ARCHIVE_SERVER" 2>/dev/null; then
      ARCHIVE_REACHABLE=false
      log "Archive unreachable after drive processing, skipping drive-data.json sync (user likely drove away)"
    fi
  fi
fi

# No-op short-circuit: skip the JSON regen + remote sync when the live
# drives_count matches what the archive already has (the LAST_REACHABLE
# baseline written at the end of the previous successful sync). The export
# can take 3+ minutes on a well-used Pi (~848 MB on a year of dashcam data)
# and rsync would then ship zero bytes — pure waste when nothing changed.
#
# Only short-circuits when a baseline exists (LAST_REACHABLE_EXISTS=true);
# the first-ever post-archive run still regenerates + ships normally.
# Compares drives_count, which is the user-visible "drives" number; if a
# clip was processed but had no GPS, the processed_files list grows in the
# DB but the archive's view of processed_files stays stale by one cycle —
# harmless, since the route data itself is unchanged.
SKIP_REGEN_SYNC=false
if [ "$ARCHIVE_REACHABLE" = "true" ] && { [ -n "${RSYNC_SERVER:-}" ] || [ -n "${RCLONE_DRIVE:-}" ]; }; then
  POST_STATS=$(curl -sf "${API_URL}/api/drives/stats" 2>/dev/null)
  if [ $? -eq 0 ]; then
    DRIVES_NOW=$(echo "$POST_STATS" | grep -o '"drives_count":[0-9]*' | cut -d: -f2)
    DRIVES_NOW=${DRIVES_NOW:-0}
    if [ "$LAST_REACHABLE_EXISTS" = "true" ] && [ "$DRIVES_NOW" -eq "$LAST_DRIVES_COUNT" ]; then
      SKIP_REGEN_SYNC=true
      log "No new drives since last successful sync (drives_count=${DRIVES_NOW}); skipping drive-data.json regen + archive sync."
    fi
  fi
fi

# Regenerate the drive-data.json mirror from the SQLite store before any
# remote sync. The canonical live store is /backingfiles/drive-data.db; the
# JSON file at $DRIVE_DATA_JSON (/backingfiles/drive-data.json) is rebuilt
# on demand for archive consumers — Sentry Studio reads the archive-side
# JSON copy, not the Pi-local one.
#
# On older binaries that don't yet expose /api/drives/data/export-for-sync
# this is a no-op (curl returns non-zero) and the rsync/rclone blocks
# below ship whatever JSON is on disk, same as before.
if [ "$SKIP_REGEN_SYNC" != "true" ] && [ "$ARCHIVE_REACHABLE" = "true" ] && { [ -n "${RSYNC_SERVER:-}" ] || [ -n "${RCLONE_DRIVE:-}" ]; }; then
  log "Regenerating drive-data.json mirror for archive sync..."
  EXPORT_RESULT=$(curl -sf -X POST "${API_URL}/api/drives/data/export-for-sync" 2>/dev/null)
  if [ $? -eq 0 ]; then
    EXPORT_BYTES=$(echo "$EXPORT_RESULT" | grep -o '"bytes":[0-9]*' | cut -d: -f2)
    log "Regenerated drive-data.json mirror (${EXPORT_BYTES:-?} bytes)."
  else
    log "Note: export-for-sync endpoint unavailable; shipping existing ${DRIVE_DATA_JSON} (pre-SQLite binary?)."
  fi
fi

# Sync drive-data.json to the rsync archive server.
# For CIFS/NFS archive types, the Rust server's SyncToArchive() handles this
# while /mnt/archive is still mounted.  For rsync archive there is no local
# mount, so SyncToArchive() silently skips — we handle it here instead.
#
# Size-guard: refuse if local file is dramatically smaller than the last
# successful sync (see drive_data_size_guard_ok above).
if [ "$SKIP_REGEN_SYNC" != "true" ] && [ "$ARCHIVE_REACHABLE" = "true" ] && [ -n "${RSYNC_SERVER:-}" ] && [ -n "${RSYNC_USER:-}" ] && [ -f "$DRIVE_DATA_JSON" ]; then
  if drive_data_size_guard_ok "$DRIVE_DATA_JSON" "rsync archive"; then
    log "Syncing drive-data.json to rsync archive..."
    # Optional non-standard SSH port, via rsync's remote-shell override
    # (see rsync_archive/archive-clips.sh). Unset means the SSH default.
    RSYNC_SSH_ARGS=()
    if [ -n "${RSYNC_SSH_PORT:-}" ]; then
      RSYNC_SSH_ARGS=(-e "ssh -p ${RSYNC_SSH_PORT}")
    fi
    if rsync -avh --no-perms --omit-dir-times --timeout=60 \
        ${RSYNC_SSH_ARGS[@]+"${RSYNC_SSH_ARGS[@]}"} \
        "$DRIVE_DATA_JSON" \
        "$RSYNC_USER@$RSYNC_SERVER:${RSYNC_PATH}/drive-data.json" > /dev/null 2>&1; then
      log "Synced drive-data.json to archive ($(wc -c < "$DRIVE_DATA_JSON") bytes)."
      update_drive_data_sync_cache "$DRIVE_DATA_JSON"
    else
      log "Warning: failed to sync drive-data.json to rsync archive."
    fi
  fi
fi

# For rclone archive (no local mount; rclone pushes directly to cloud storage).
if [ "$SKIP_REGEN_SYNC" != "true" ] && [ "$ARCHIVE_REACHABLE" = "true" ] && [ -n "${RCLONE_DRIVE:-}" ] && [ -f "$DRIVE_DATA_JSON" ]; then
  if drive_data_size_guard_ok "$DRIVE_DATA_JSON" "rclone archive"; then
    log "Syncing drive-data.json to rclone archive..."
    if rclone --config /root/.config/rclone/rclone.conf copy \
        "$DRIVE_DATA_JSON" "$RCLONE_DRIVE:${RCLONE_PATH}/drive-data.json" > /dev/null 2>&1; then
      log "Synced drive-data.json to rclone archive ($(wc -c < "$DRIVE_DATA_JSON") bytes)."
      update_drive_data_sync_cache "$DRIVE_DATA_JSON"
    else
      log "Warning: failed to sync drive-data.json to rclone archive."
    fi
  fi
fi

# Backup config after successful archive
if [ "$ARCHIVE_REACHABLE" = "true" ]; then
  log "Creating config backup..."
  BACKUP_RESULT=$(curl -sf -X POST "${API_URL}/api/system/backup" 2>/dev/null)
  if [ $? -eq 0 ]; then
    log "Config backup created successfully."
  else
    log "Warning: config backup failed."
  fi
fi

# Send notification with split counts: drives mapped silently while the archive
# was unreachable (snapshotloop) vs drives mapped in this post-archive cycle.
# Baseline (LAST_*) for the "while unreachable" bucket is loaded near the top.
if [ -x /root/bin/send-push-message ]; then
  STATS=$(curl -sf "${API_URL}/api/drives/stats" 2>/dev/null)
  if [ $? -eq 0 ]; then
    DRIVES_AFTER=$(echo "$STATS" | grep -o '"drives_count":[0-9]*' | cut -d: -f2)
    DRIVES_AFTER=${DRIVES_AFTER:-0}

    # Check user unit preference (mi or km) from setup config (DRIVE_MAP_UNIT)
    UNIT_PREF=$(curl -sf "${API_URL}/api/setup/config" 2>/dev/null | grep -o '"DRIVE_MAP_UNIT":{[^}]*}' | grep -o '"value":"[^"]*"' | cut -d'"' -f4)
    if [ "$UNIT_PREF" = "km" ]; then
      DIST_AFTER=$(echo "$STATS" | grep -o '"total_distance_km":[0-9.]*' | cut -d: -f2)
      DIST_BEFORE=$(echo "$BEFORE_STATS" | grep -o '"total_distance_km":[0-9.]*' | cut -d: -f2)
      DIST_LAST=$LAST_DIST_KM
      DIST_LABEL="km"
    else
      DIST_AFTER=$(echo "$STATS" | grep -o '"total_distance_mi":[0-9.]*' | cut -d: -f2)
      DIST_BEFORE=$(echo "$BEFORE_STATS" | grep -o '"total_distance_mi":[0-9.]*' | cut -d: -f2)
      DIST_LAST=$LAST_DIST_MI
      DIST_LABEL="miles"
    fi
    DIST_BEFORE=${DIST_BEFORE:-0}
    DIST_AFTER=${DIST_AFTER:-0}

    # Bucket 1: drives added by snapshotloop while archive was unreachable.
    # Skip on the first-ever run (no baseline) — otherwise we'd attribute every
    # historical drive to "while away."
    if [ "$LAST_REACHABLE_EXISTS" = "true" ]; then
      AWAY_DRIVES=$((DRIVES_BEFORE - LAST_DRIVES_COUNT))
      [ "$AWAY_DRIVES" -lt 0 ] && AWAY_DRIVES=0
      AWAY_DIST=$(awk "BEGIN { d = ${DIST_BEFORE} - ${DIST_LAST}; if (d < 0) d = 0; printf \"%.2f\", d }")
    else
      AWAY_DRIVES=0
      AWAY_DIST="0.00"
    fi

    # Bucket 2: drives just added by this post-archive cycle.
    NOW_DRIVES=$((DRIVES_AFTER - DRIVES_BEFORE))
    [ "$NOW_DRIVES" -lt 0 ] && NOW_DRIVES=0
    NOW_DIST=$(awk "BEGIN { d = ${DIST_AFTER} - ${DIST_BEFORE}; if (d < 0) d = 0; printf \"%.2f\", d }")

    word() { [ "$1" -eq 1 ] && echo drive || echo drives; }

    MSG=""
    if [ "$AWAY_DRIVES" -gt 0 ] && [ "$NOW_DRIVES" -gt 0 ]; then
      MSG="${AWAY_DRIVES} new $(word $AWAY_DRIVES) mapped while archive was unreachable (${AWAY_DIST} ${DIST_LABEL}). ${NOW_DRIVES} new $(word $NOW_DRIVES) mapped now (${NOW_DIST} ${DIST_LABEL})."
    elif [ "$AWAY_DRIVES" -gt 0 ]; then
      MSG="${AWAY_DRIVES} new $(word $AWAY_DRIVES) mapped while archive was unreachable (${AWAY_DIST} ${DIST_LABEL})."
    elif [ "$NOW_DRIVES" -gt 0 ]; then
      MSG="${NOW_DRIVES} new $(word $NOW_DRIVES) mapped (${NOW_DIST} ${DIST_LABEL})."
    fi

    if [ -n "$MSG" ]; then
      /root/bin/send-push-message "${NOTIFICATION_TITLE:-SentryUSB}:" "$MSG" info drives \
        || log "Failed to send notification"
    else
      log "No new drives found, skipping drive stats notification."
    fi

    # Update baseline for next cycle (always — even when no notification fired —
    # so AWAY_DRIVES on the next run reflects only deltas added after this point).
    DIST_AFTER_MI=$(echo "$STATS" | grep -o '"total_distance_mi":[0-9.]*' | cut -d: -f2)
    DIST_AFTER_KM=$(echo "$STATS" | grep -o '"total_distance_km":[0-9.]*' | cut -d: -f2)
    DIST_AFTER_MI=${DIST_AFTER_MI:-0}
    DIST_AFTER_KM=${DIST_AFTER_KM:-0}
    NOW_TS=$(date +%s)
    TMP_STATS="${LAST_REACHABLE_STATS_FILE}.tmp"
    if printf '{"drives_count":%d,"total_distance_mi":%s,"total_distance_km":%s,"updated_at":%d}\n' \
       "$DRIVES_AFTER" "$DIST_AFTER_MI" "$DIST_AFTER_KM" "$NOW_TS" > "$TMP_STATS" 2>/dev/null; then
      mv -f "$TMP_STATS" "$LAST_REACHABLE_STATS_FILE" 2>/dev/null || rm -f "$TMP_STATS"
    fi
  fi
fi

# Check for updates automatically (if not disabled)
AUTO_UPDATE_CHECK=$(curl -sf "${API_URL}/api/config/preference?key=auto_update_check" 2>/dev/null | grep -o '"value":"[^"]*"' | cut -d'"' -f4)
if [ "$AUTO_UPDATE_CHECK" != "disabled" ]; then
  log "Checking for SentryUSB updates..."
  UPDATE_RESULT=$(curl -sf -X POST "${API_URL}/api/system/check-update" 2>/dev/null)
  if [ $? -eq 0 ]; then
    # Determine which version to notify about (stable or prerelease)
    NOTIFY_VER=""
    UPDATE_AVAILABLE=$(echo "$UPDATE_RESULT" | grep -o '"update_available":true')
    if [ -n "$UPDATE_AVAILABLE" ]; then
      NOTIFY_VER=$(echo "$UPDATE_RESULT" | grep -o '"latest_version":"[^"]*"' | cut -d'"' -f4)
    fi
    # If user is on prerelease channel, also check for prerelease updates
    UPDATE_CHANNEL=$(curl -sf "${API_URL}/api/config/preference?key=update_channel" 2>/dev/null | grep -o '"value":"[^"]*"' | cut -d'"' -f4)
    if [ "$UPDATE_CHANNEL" = "prerelease" ] && [ -z "$NOTIFY_VER" ]; then
      PRE_AVAILABLE=$(echo "$UPDATE_RESULT" | grep -o '"prerelease":{[^}]*"available":true')
      if [ -n "$PRE_AVAILABLE" ]; then
        NOTIFY_VER=$(echo "$UPDATE_RESULT" | grep -o '"prerelease":{[^}]*"version":"[^"]*"' | grep -o '"version":"[^"]*"' | cut -d'"' -f4)
      fi
    fi

    if [ -n "$NOTIFY_VER" ]; then
      # Only send notification once per version (check marker file)
      NOTIFIED_FILE="/tmp/sentryusb-update-notified-${NOTIFY_VER}"
      if [ ! -f "$NOTIFIED_FILE" ] && [ -x /root/bin/send-push-message ]; then
        /root/bin/send-push-message "${NOTIFICATION_TITLE:-SentryUSB}:" \
          "Update available: ${NOTIFY_VER}. Open Settings to install." \
          info update || log "Failed to send update notification"
        touch "$NOTIFIED_FILE"
      fi
      log "Update available: ${NOTIFY_VER}"
    else
      log "SentryUSB is up to date."
    fi
  else
    log "Could not check for updates (no internet?)."
  fi
fi

exit 0
