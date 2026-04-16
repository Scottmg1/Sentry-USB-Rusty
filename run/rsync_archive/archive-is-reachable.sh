#!/bin/bash -eu

ARCHIVE_HOST_NAME="$1"

ping -q -w 1 -c 1 "$ARCHIVE_HOST_NAME" &> /dev/null || ssh -q -o ConnectTimeout=1 "$RSYNC_USER"@"$ARCHIVE_HOST_NAME" exit
