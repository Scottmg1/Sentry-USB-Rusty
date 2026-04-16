#!/bin/bash -eu

VERS_OPT=
SEC_OPT=

function log_progress () {
  if declare -F setup_progress > /dev/null
  then
    setup_progress "verify-and-configure-archive: $*"
    return
  fi
  echo "verify-and-configure-archive: $1"
}

function check_archive_server_reachable () {
  log_progress "Verifying that the archive server $ARCHIVE_SERVER is reachable..."
  local serverunreachable=false
  local default_interface
  default_interface=$(route | grep "^default" | awk '{print $NF}')
  hping3 -c 1 -S -p 445 "$ARCHIVE_SERVER" 1>/dev/null 2>&1 ||
    hping3 -c 1 -S -p 445 -I "$default_interface" "$ARCHIVE_SERVER" 1>/dev/null 2>&1 ||
    serverunreachable=true

  if [ "$serverunreachable" = true ]
  then
    log_progress "STOP: The archive server $ARCHIVE_SERVER is unreachable. Try specifying its IP address instead."
    exit 1
  fi

  log_progress "The archive server is reachable."
}

function write_archive_configs_to {
  (
    echo "username=$SHARE_USER"
    echo "password=$SHARE_PASSWORD"
    if [ -n "${SHARE_DOMAIN+x}" ]
    then
      echo "domain=$SHARE_DOMAIN"
    fi
  ) > "$1"
}

function check_archive_mountable () {
  local test_mount_location="/tmp/archivetestmount"

  log_progress "Verifying that the archive share is mountable..."

  if [ ! -e "$test_mount_location" ]
  then
    mkdir "$test_mount_location"
  fi

  local tmp_credentials_file_path="/tmp/teslaCamArchiveCredentials"
  write_archive_configs_to "$tmp_credentials_file_path"

  local mounted=false
  local try_versions="${CIFS_VERSION:-default 3.1.1 3.0 2.1 2.0 @@}"
  local try_secs="${CIFS_SEC:-@@ ntlmssp ntlmv2 ntlm}"

  echo "Trying all combinations of vers=($try_versions) and sec=($try_secs)"
  for vers in $try_versions
  do
    for sec in $try_secs
    do
      versopt=""
      secopt=""
      if [ "$vers" != "@@" ]
      then
        versopt="vers=$vers"
      fi
      if [ "$sec" != "@@" ]
      then
        secopt="sec=$sec"
      fi
      local commandline="mount -t cifs '//$1/$2' '$test_mount_location' -o '$3,credentials=${tmp_credentials_file_path},iocharset=utf8,file_mode=0777,dir_mode=0777,$versopt,$secopt'"
      log_progress "Trying mount command-line:"
      log_progress "$commandline"
      if eval "$commandline"
      then
        mounted=true
        break 2
      fi
    done
  done
  if [ "$mounted" = false ]
  then
    log_progress "STOP: no working combination of vers and sec mount options worked"
    exit 1
  else
    log_progress "The archive share is mountable using: $commandline"
    if [ "$3" = "rw" ]
    then
       if ! touch "$test_mount_location/testfile"
       then
         log_progress "STOP: archive share is not writeable. Check permissions."
         umount "$test_mount_location"
         exit 1
       fi
       rm "$test_mount_location/testfile"
    fi

    # the music archive must be mountable with the same mount options
    # so fix the options now
    export CIFS_VERSION=$vers
    export CIFS_SEC=$sec
    VERS_OPT=$versopt
    SEC_OPT=$secopt
  fi

  umount "$test_mount_location"
}

function install_required_packages () {
  log_progress "Installing/updating required packages if needed"
  apt-get -y --force-yes install hping3 cifs-utils
  if ! command -v nc > /dev/null
  then
    apt-get -y --force-yes install netcat || apt-get -y --force-yes install netcat-openbsd
  fi
  log_progress "Done"
}

install_required_packages

check_archive_server_reachable

if [ -e /backingfiles/cam_disk.bin ]
then
  check_archive_mountable "$ARCHIVE_SERVER" "$SHARE_NAME" rw
fi

if [ -n "${MUSIC_SHARE_NAME:+x}" ]
then
  if [ "$MUSIC_SIZE" = "0" ]
  then
    log_progress "STOP: MUSIC_SHARE_NAME specified but no music drive size specified"
    exit 1
  fi
  check_archive_mountable "$ARCHIVE_SERVER" "$MUSIC_SHARE_NAME" ro
fi

function configure_archive () {
  log_progress "Configuring the archive..."

  local archive_path="/mnt/archive"
  local music_archive_path="/mnt/musicarchive"

  if [ ! -e "$archive_path" ] && [ -e /backingfiles/cam_disk.bin ]
  then
    mkdir "$archive_path"
  fi

  local credentials_file_path="/root/.teslaCamArchiveCredentials"
  write_archive_configs_to "$credentials_file_path"

  sed -i "/^.*\.teslaCamArchiveCredentials.*$/ d" /etc/fstab


  if [ -e /backingfiles/cam_disk.bin ]
  then
    local sharenameforstab="${SHARE_NAME// /\\040}"
    echo "//$ARCHIVE_SERVER/$sharenameforstab $archive_path cifs rw,noauto,credentials=${credentials_file_path},iocharset=utf8,file_mode=0777,dir_mode=0777,$VERS_OPT,$SEC_OPT 0" >> /etc/fstab
  elif [ -d "$archive_path" ]
  then
    rmdir "$archive_path" || log_progress "failed to remove $archive_path"
  fi

  if [ -n "${MUSIC_SHARE_NAME:+x}" ]
  then
    if [ ! -e "$music_archive_path" ]
    then
      mkdir "$music_archive_path"
    fi
    local musicsharenameforstab="${MUSIC_SHARE_NAME// /\\040}"
    echo "//$ARCHIVE_SERVER/$musicsharenameforstab $music_archive_path cifs ro,noauto,credentials=${credentials_file_path},iocharset=utf8,file_mode=0777,dir_mode=0777,$VERS_OPT,$SEC_OPT 0" >> /etc/fstab
  elif [ -d "$music_archive_path" ]
  then
    rmdir "$music_archive_path" || log_progress "failed to remove $music_archive_path"
  fi
  log_progress "Configured the archive."
}

configure_archive
