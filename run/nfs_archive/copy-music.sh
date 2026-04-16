#!/bin/bash -eu

ensure_music_file_is_mounted
/root/bin/copy-music.sh
trim_free_space "$MUSIC_MOUNT"
unmount_music_file
