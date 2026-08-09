#!/bin/bash -eu

ARCHIVE_HOST_NAME="$1"

# Optional non-standard SSH port (RSYNC_SSH_PORT in sentryusb.conf). Without
# it the ssh fallback probes port 22, so a reachable server on another port
# reads as dead whenever ICMP is filtered and archiveloop waits forever.
SSH_PORT_ARGS=()
if [ -n "${RSYNC_SSH_PORT:-}" ]; then
  SSH_PORT_ARGS=(-p "${RSYNC_SSH_PORT}")
fi

# Probe timeouts default to 1s (snappy, normal mode). Travel Mode raises them
# via env (exported by the watchdog in archive-clips.sh) so a slow/relayed VPN
# link isn't misread as "unreachable".
ping -q -w "${ARCHIVE_PING_TIMEOUT:-1}" -c 1 "$ARCHIVE_HOST_NAME" &> /dev/null \
  || ssh -q ${SSH_PORT_ARGS[@]+"${SSH_PORT_ARGS[@]}"} \
       -o ConnectTimeout="${ARCHIVE_SSH_TIMEOUT:-1}" "$RSYNC_USER"@"$ARCHIVE_HOST_NAME" exit
