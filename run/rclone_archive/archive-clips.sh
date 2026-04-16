#!/bin/bash -eu

# read the setup variables again because arrays, like RCLONE_FLAGS, don't export to subshells/child scripts
source /root/bin/envsetup.sh

flags=("-L" "--transfers=1")
if [[ -v RCLONE_FLAGS ]]
then
  flags+=("${RCLONE_FLAGS[@]}")
fi

while [ -n "${1+x}" ]
do
  rclone --config /root/.config/rclone/rclone.conf move "${flags[@]}" --files-from "$2" "$1" "$RCLONE_DRIVE:$RCLONE_PATH" >> "$LOG_FILE" 2>&1
  shift 2
done
