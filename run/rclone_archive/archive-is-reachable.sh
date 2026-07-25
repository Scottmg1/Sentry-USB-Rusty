#!/bin/bash -eu

# $1 is a pingable liveness IP (ARCHIVE_SERVER, default 8.8.8.8 for cloud
# remotes). ARCHIVE_PING_TIMEOUT is exported by archive-clips.sh in Travel
# Mode for slow mobile links; default matches the original 1s probe.
ping -q -w "${ARCHIVE_PING_TIMEOUT:-1}" -c 1 "$1" > /dev/null 2>&1
