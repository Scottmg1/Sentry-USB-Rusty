#!/bin/bash -eu

function log_progress () {
  if declare -F setup_progress > /dev/null
  then
    setup_progress "verify-and-configure-archive: $*"
    return
  fi
  echo "verify-and-configure-archive: $1"
}

function verify_configuration () {
    log_progress "Verifying rclone configuration..."
    if ! [ -e "/root/.config/rclone/rclone.conf" ]
    then
        log_progress "STOP: rclone config was not found. did you configure rclone correctly?"
        exit 1
    fi

    if ! rclone lsd "$RCLONE_DRIVE:$RCLONE_PATH" > /dev/null
    then
        log_progress "STOP: Could not find the $RCLONE_DRIVE:$RCLONE_PATH"
        exit 1
    fi
}

verify_configuration

function configure_archive () {
  log_progress "Configuring rclone archive..."

  # Ensure that /root/.config/rclone is a directory not a symlink
  if [ ! -L "/root/.config/rclone" ] && [ -d "/root/.config/rclone" ]
  then
    log_progress "Moving rclone configs into /mutable"
    # make sure that /mutable is mounted prior to moving rclone configuration
    if ! findmnt --mountpoint /mutable
    then
      mount /mutable
    fi
    # Creating only configs dir so we can move the rclone dir into it
    mkdir -p /mutable/configs
    # Moving the directory itself to ensure the link creation works correctly
    mv /root/.config/rclone /mutable/configs/
    # Creating link, this requires the directory /root/.config/rclone to be nonexistent
    ln -s /mutable/configs/rclone /root/.config/rclone
  fi

  log_progress "Done"
}

configure_archive
