#!/bin/bash -eu

if [ "${BASH_SOURCE[0]}" != "$0" ]
then
  echo "${BASH_SOURCE[0]} must be executed, not sourced"
  return 1 # shouldn't use exit when sourced
fi

function log_progress () {
  if declare -F setup_progress > /dev/null
  then
    setup_progress "configure: $1"
    return
  fi
  echo "configure: $1"
}

if [ "${FLOCKED:-}" != "$0" ]
then
  PARENT="$(ps -o comm= $PPID)"
  if [ "$PARENT" != "setup-sentryusb" ]
  then
    log_progress "STOP: $0 must be called from setup-sentryusb: $PARENT"
    exit 1
  fi

  if FLOCKED="$0" flock -en -E 99 "$0" "$0" "$@" || case "$?" in
  99) echo already running
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

ARCHIVE_SYSTEM=${ARCHIVE_SYSTEM:-none}

function check_variable () {
    local var_name="$1"
    if [ -z "${!var_name+x}" ]
    then
        log_progress "STOP: Define the variable $var_name like this: export $var_name=value"
        exit 1
    fi
}

# as of March 2021, Raspberry Pi OS still includes a 3 year old version of
# rsync, which has a bug (https://bugzilla.samba.org/show_bug.cgi?id=10494)
# that breaks archiving from snapshots.
# Check that the default rsync works correctly, and install a newer version
# if needed.
function check_default_rsync {
  if ! hash rsync
  then
    apt install rsync
  fi

  rm -rf /tmp/rsynctest
  mkdir -p /tmp/rsynctest/src /tmp/rsynctest/dst
  echo testfile > /tmp/testfile.dat
  echo testfile.dat > /tmp/filelist
  ln -s /tmp/testfile.dat /tmp/rsynctest/src/
  if rsync -avhRL --remove-source-files --no-perms --omit-dir-times --files-from=/tmp/filelist /tmp/rsynctest/src/ /tmp/rsynctest/dst
  then
    if [ -s /tmp/rsynctest/dst/testfile.dat ] && ! [ -e /tmp/rsynctest/src/testfile.dat ]
    then
      rm -rf /tmp/rsynctest
      return 0
    fi
  fi
  return 1
}

function install_prebuilt_rsync {
  local arch="$(uname -m)"
  if [ "$arch" = "aarch64" ]
  then
    curl -L --fail -o /usr/local/bin/rsync https://github.com/marcone/rsync/releases/download/v3.2.3-arm64/rsync
  elif [[ $arch =~ arm* ]]
  then
    curl -L --fail -o /usr/local/bin/rsync https://github.com/marcone/rsync/releases/download/v3.2.3-rpi/rsync
  else
    log_progress "No prebuilt rsync for '$arch'"
    return 1
  fi
}

function check_rsync {
  if check_default_rsync
  then
    log_progress "rsync seems to work OK"
    return 0
  fi

  log_progress "default rsync doesn't work, installing prebuilt 3.2.3"
  if install_prebuilt_rsync
  then
    chmod a+x /usr/local/bin/rsync
    apt install -y libxxhash0 libssl-dev
    if check_default_rsync
    then
      log_progress "rsync works OK now"
      return 0
    fi
  fi

  log_progress "STOP: rsync doesn't work correctly"
  log_progress "(using '$(which rsync)')"
  exit 1
}

function check_archive_configs () {
    log_progress "Checking archive configs: "

    case "$ARCHIVE_SYSTEM" in
        rsync)
            check_variable "RSYNC_USER"
            check_variable "RSYNC_SERVER"
            check_variable "RSYNC_PATH"
            export ARCHIVE_SERVER="$RSYNC_SERVER"
            check_rsync
            ;;
        rclone)
            check_variable "RCLONE_DRIVE"
            check_variable "RCLONE_PATH"
            export ARCHIVE_SERVER="8.8.8.8" # since it's a cloud hosted drive we'll just set this to google dns
            ;;
        cifs)
            if [ -e /backingfiles/cam_disk.bin ]
            then
              check_variable "SHARE_NAME"
            fi
            check_variable "SHARE_USER"
            check_variable "SHARE_PASSWORD"
            check_variable "ARCHIVE_SERVER"
            check_rsync
            ;;
         nfs)
            if [ -e /backingfiles/cam_disk.bin ]
            then
              check_variable "SHARE_NAME"
            fi
            check_variable "ARCHIVE_SERVER"
            check_rsync
            ;;
        none)
            export ARCHIVE_SERVER=localhost
            ;;
        *)
            log_progress "STOP: Unrecognized archive system: $ARCHIVE_SYSTEM"
            exit 1
            ;;
    esac

    log_progress "done"

    # rsync is used unconditionally by archiveloop for wraps/LicensePlate
    # sync, so ensure it is present regardless of the archive system.
    if ! hash rsync 2>/dev/null
    then
      log_progress "Installing rsync (required for media sync)..."
      apt-get install -y rsync
    fi
}

function get_archive_module () {

    case "$ARCHIVE_SYSTEM" in
        rsync)
            echo "run/rsync_archive"
            ;;
        rclone)
            echo "run/rclone_archive"
            ;;
        cifs)
            echo "run/cifs_archive"
            ;;
         nfs)
            echo "run/nfs_archive"
            ;;
        none)
            echo "run/none_archive"
            ;;
        *)
            log_progress "Internal error: Attempting to configure unrecognized archive system: $ARCHIVE_SYSTEM"
            exit 1
            ;;
    esac
}

function pip3_install () {
  rm -f /usr/lib/$(py3versions -d)/EXTERNALLY-MANAGED
  pip3 install "$@"
}

function check_at_most_one_wake_api () {
  if [[ ( -n "${TESSIE_API_TOKEN:+x}" && -n "${TESLAFI_API_TOKEN:+x}" ) ||
        ( -n "${TESSIE_API_TOKEN:+x}" && -n "${TESLA_BLE_VIN:+x}" ) ||
        ( -n "${TESLAFI_API_TOKEN:+x}" && -n "${TESLA_BLE_VIN:+x}" ) ||
        ( -n "${KEEP_AWAKE_WEBHOOK_URL:+x}" && -n "${TESLAFI_API_TOKEN:+x}" ) ||
        ( -n "${KEEP_AWAKE_WEBHOOK_URL:+x}" && -n "${TESLA_BLE_VIN:+x}" ) ||
        ( -n "${KEEP_AWAKE_WEBHOOK_URL:+x}" && -n "${TESSIE_API_TOKEN:+x}" )]]
  then
    log_progress "STOP: You're trying to set up multiple control providers at the same time."
    log_progress "Only 1 can be enabled at a time."
    exit 1
  fi
  return 0
}

function check_teslafi_api () {
  if [[ ( -n "${TESLAFI_API_TOKEN:+x}" ) ]]
  then
    check_variable "SENTRY_CASE"
    if [[ "$SENTRY_CASE" != 1 && "$SENTRY_CASE" != 2 ]]; then
      log_progress "STOP: invalid SENTRY_CASE for Tesla API."
      exit 1
    fi

    if ! command -v jq &>/dev/null
      then
        log_progress "Installing required package for TeslaFi API: jq"
        DEBIAN_FRONTEND=noninteractive apt-get -y --force-yes install jq
    fi
    log_progress "TeslaFi API enabled."
  else
    log_progress "TeslaFi API not enabled because no TeslaFi credential was provided."
  fi
}

function check_tessie_api () {
  if [[ ( -n "${TESSIE_API_TOKEN:+x}" ) ]]
  then
    check_variable "SENTRY_CASE"
    if [[ "$SENTRY_CASE" != 1 && "$SENTRY_CASE" != 2 && "$SENTRY_CASE" != 3 ]]; then
      log_progress "STOP: invalid SENTRY_CASE for Tessie API."
      exit 1
    fi
    
    if [[ ( -z "${TESSIE_VIN:+x}" ) ]]
    then
      log_progress "STOP: Tessie API requires the VIN number to be provided."
      log_progress "Please set TESSIE_VIN in the config file."
      exit 1
    else
      if ! command -v jq &>/dev/null
      then
        log_progress "Installing required package for Tessie API: jq"
        DEBIAN_FRONTEND=noninteractive apt-get -y --force-yes install jq
      fi

      log_progress "Tessie API enabled."
    fi
  else
    log_progress "Tessie API not enabled because no Tessie credential was provided."
  fi
}

function check_and_configure_tesla_ble () {
  local install_path="$1"
  if [[ ( -n "${TESLA_BLE_VIN:+x}" ) ]]
  then
    check_variable "SENTRY_CASE"
    if [[ "$SENTRY_CASE" != 1 && "$SENTRY_CASE" != 2 && "$SENTRY_CASE" != 3 ]]; then
      log_progress "STOP: invalid SENTRY_CASE for Tesla BLE API."
      exit 1
    fi
    
    if dpkg-query -W --showformat='${db:Status-Status}\n' "bluez" 2>/dev/null | grep -q '^installed$'
    then
      log_progress "Skipping required package for Tesla BLE API: bluez already installed."
    else
      log_progress "Installing required package for Tesla BLE API: bluez"
      DEBIAN_FRONTEND=noninteractive apt-get -y --force-yes install bluez
    fi

    if [[ -n "$(apt-cache search pi-bluetooth)" ]]
    then
        if dpkg-query -W --showformat='${db:Status-Status}\n' "pi-bluetooth" 2>/dev/null | grep -q '^installed$'
        then
          log_progress "Skipping required package for Tesla BLE API: pi-bluetooth already installed."
        else
          log_progress "Installing required package for Tesla BLE API: pi-bluetooth"
          DEBIAN_FRONTEND=noninteractive apt-get -y --force-yes install pi-bluetooth
        fi
    else
        log_progress "Skipping required package for Tesla BLE API: pi-bluetooth does not exist for this device."
    fi

    log_progress "Downloading Tesla BLE control binaries..."
    install_tesla_ble_packages "$install_path"

    local pairing_needed=true
    mkdir -p /root/.ble
    if [ ! -f /root/.ble/key_public.pem ] || [ ! -f /root/.ble/key_private.pem ]
    then
      "$install_path/tesla-keygen" -key-file /root/.ble/key_private.pem -output /root/.ble/key_public.pem create
      chmod 600 /root/.ble/key_private.pem
      chmod 644 /root/.ble/key_public.pem
      # Mark these as freshly generated so the web UI knows pairing hasn't been
      # confirmed yet and won't falsely show "BLE Paired" on a new install.
      touch /root/.ble/key_pending_pairing
      log_progress "Generated keys for Tesla BLE interface."
    elif timeout 30 "$install_path/tesla-control" -ble -vin "${TESLA_BLE_VIN^^}" body-controller-state; then
      if timeout 30 "$install_path/tesla-control" -ble -vin "${TESLA_BLE_VIN^^}" session-info /root/.ble/key_private.pem infotainment; then
        log_progress "Tesla BLE keys exist and are paired."
        pairing_needed=false
      else
        log_progress "WARNING: Tesla BLE keys already exist, but are not paired."
      fi
    else
      log_progress "WARNING: Tesla BLE keys exist, but the car is not reachable (timed out after 30s)."
      log_progress "If you are performing setup away from your car, this is expected."
      log_progress "Pairing can be completed later via the web UI when SentryUSB is near the car."
      pairing_needed=false
    fi

    if $pairing_needed
    then
      log_progress "Please visit the web UI after SentryUSB is in the car to pair the key."
    fi

    log_progress "Tesla BLE API enabled."
  else
    log_progress "Tesla BLE API not enabled because no Tesla BLE VIN was provided."
  fi
}

function check_awake_webhook () {
  if [[ ( -n "${KEEP_AWAKE_WEBHOOK_URL:+x}" ) ]]
  then
    check_variable "SENTRY_CASE"
    if [[ "$SENTRY_CASE" != 1 && "$SENTRY_CASE" != 2 && "$SENTRY_CASE" != 3 ]]; then
      log_progress "STOP: invalid SENTRY_CASE for Webhook."
      exit 1
    fi

    log_progress "Awake Webhook enabled."
  else
    log_progress "Awake Webhook not enabled because no webhook URL was provided."
  fi
}

function check_and_install_temperature_monitor () {
  local install_path="$1"

  if [[ -n "${TEMPERATURE_WARNING:+x}" && ! "$TEMPERATURE_WARNING" =~ ^[+-]?[0-9]+$ ]]
  then
    log_progress "STOP: You're trying to set up a temperature Warning threshold that is not an integer."
    exit 1
  fi
  if [[ -n "${TEMPERATURE_CAUTION:+x}" && ! "$TEMPERATURE_CAUTION" =~ ^[+-]?[0-9]+$ ]]
    then
    log_progress "STOP: You're trying to set up a temperature Caution threshold that is not an integer."
    exit 1
  fi
  if [[ -n "${TEMPERATURE_INTERVAL:+x}" && ( ! "$TEMPERATURE_INTERVAL" =~ ^[0-9]+$ || "$TEMPERATURE_INTERVAL" -lt 1 ) ]]
  then
    log_progress "STOP: You're trying to set up a fixed-interval temperature logging time that is not a positive integer of at least 1 minute."
    exit 1
  fi

  log_progress 'Installing temperature monitor script'
  copy_script run/temperature_monitor "$install_path"

  return 0
}

function install_archive_scripts () {
  local install_path="$1"
  local archive_module="$2"

  log_progress "Installing base archive scripts into $install_path"
  copy_script setup/pi/envsetup.sh "$install_path"
  copy_script run/archiveloop "$install_path"
  copy_script run/waitforidle "$install_path"
  copy_script run/remountfs_rw "$install_path"
  copy_script run/awake_start "$install_path"
  copy_script run/awake_stop "$install_path"
  copy_script run/post-archive-process.sh "$install_path"
  log_progress "Installing archive module scripts"
  copy_script "$archive_module"/verify-and-configure-archive.sh /tmp
  copy_script "$archive_module"/archive-clips.sh "$install_path"
  copy_script "$archive_module"/connect-archive.sh "$install_path"
  copy_script "$archive_module"/disconnect-archive.sh "$install_path"
  copy_script "$archive_module"/archive-is-reachable.sh "$install_path"
  if [ -n "${MUSIC_SHARE_NAME:+x}" ] && grep -E "cifs|nfs|rsync" <<< "$archive_module"
  then
    copy_script "$archive_module"/copy-music.sh "$install_path"
  fi
}

function install_python3_pip () {
  if ! command -v pip3 &> /dev/null
  then
    setup_progress "Installing support for python packages..."
    apt-get --assume-yes install python3-pip
  fi
}

function install_sns_packages () {
  install_python3_pip
  setup_progress "Installing sns python packages..."
  pip3_install boto3
}

function install_matrix_packages () {
  install_python3_pip
  setup_progress "Installing matrix python packages..."
  pip3_install matrix-nio
}

function install_tesla_ble_packages () {
  local install_path="$1"
  local binary_dir=/tmp/binarydir

  umount "$binary_dir" &> /dev/null || true
  rm -rf "$binary_dir"
  mkdir -p "$binary_dir"
  mount -t tmpfs none "$binary_dir"
  (
    cd "$binary_dir"
    curlwrapper -L "https://github.com/MikeBishop/tesla-vehicle-command-arm-binaries/releases/latest/download/vehicle-command-binaries-linux-armv6.tar.gz" | tar zxf - --strip-components=1
  )

  for binary in tesla-control tesla-keygen; do
    cp "${binary_dir}/$binary" "$install_path/$binary"
    chmod +x "$install_path/$binary"
    setup_progress "Downloaded $install_path/$binary ..."
  done

  umount "$binary_dir" &> /dev/null || true
  rm -rf "$binary_dir"
}

function check_signal_configuration () {
  if [ "${SIGNAL_ENABLED:-false}" = "true" ]
  then
    if [ -z "${SIGNAL_URL+x}" ] || [  "${SIGNALTO_NUM+x}" = "country_code_and_number_configured_with_signal" ] || [  "${SIGNAL_FROM_NUM+x}" = "country_code_and_number_configured_with_signal"  ]
    then
      log_progress "STOP: You're trying to setup Signal but didn't provide a URL."
      log_progress "Define the variables like this:"
      log_progress "export SIGNAL_URL=put_protocol_ip/hostname_portnumber"
      log_progress "export SIGNAL_TO_NUM=put_phone_number_associated_with_signal_including_country_code"
      log_progress "export SIGNAL_FROM_NUM=put_phone_number_associated_with_signal_including_country_code_to_send_to"
      exit 1
    elif [ "${SIGNAL_URL}" = "http://<url>:8080" ] || [  "${SIGNAL_TO_NUM}" = "country_code_and_number_configured_with_signal" ] || [  "${SIGNAL_FROM_NUM}" = "country_code_and_number_configured_with_signal"  ]
    then
      log_progress "STOP: You're trying to setup Signal, but didn't replace the default URL, to number, or from number"
      exit 1
    fi
  fi
}

function check_pushover_configuration () {
  if [ "${PUSHOVER_ENABLED:-false}" = "true" ]
  then
    if [ -z "${PUSHOVER_USER_KEY+x}" ] || [ -z "${PUSHOVER_APP_KEY+x}"  ]
    then
      log_progress "STOP: You're trying to setup Pushover but didn't provide your User and/or App key."
      log_progress "Define the variables like this:"
      log_progress "export PUSHOVER_USER_KEY=put_your_userkey_here"
      log_progress "export PUSHOVER_APP_KEY=put_your_appkey_here"
      exit 1
    elif [ "${PUSHOVER_USER_KEY}" = "put_your_userkey_here" ] || [  "${PUSHOVER_APP_KEY}" = "put_your_appkey_here" ]
    then
      log_progress "STOP: You're trying to setup Pushover, but didn't replace the default User and App key values."
      exit 1
    fi
  fi
}

function check_gotify_configuration () {
  if [ "${GOTIFY_ENABLED:-false}" = "true" ]
  then
    if [ -z "${GOTIFY_DOMAIN+x}" ] || [ -z "${GOTIFY_APP_TOKEN+x}" ] || [ -z "${GOTIFY_PRIORITY+x}" ]
    then
      log_progress "STOP: You're trying to setup Gotify but didn't provide your Domain, App token or priority."
      log_progress "Define the variables like this:"
      log_progress "export GOTIFY_DOMAIN=https://gotify.domain.com"
      log_progress "export GOTIFY_APP_TOKEN=put_your_token_here"
      log_progress "export GOTIFY_PRIORITY=5"
      exit 1
    elif [ "${GOTIFY_DOMAIN}" = "https://gotify.domain.com" ] || [  "${GOTIFY_APP_TOKEN}" = "put_your_token_here" ]
    then
      log_progress "STOP: You're trying to setup Gotify, but didn't replace the default Domain and/or App token values."
      exit 1
    fi
  fi
}

function check_discord_configuration() {
  if [ "${DISCORD_ENABLED:-false}" = "true" ]
  then
    if [ -z "${DISCORD_WEBHOOK_URL+x}" ]
    then
      log_progress "STOP: You're trying to setup Discord but didn't provide your Webhook URL."
      log_progress "Define the variables like this:"
      log_progress "export DISCORD_WEBHOOK_URL=put_your_webhook_url_here"
      exit 1
    elif [ "${DISCORD_WEBHOOK_URL}" = "put_your_webhook_url_here" ]
    then
      log_progress "STOP: You're trying to setup Discord, but didn't replace the default Webhook URL"
      exit 1
    fi
  fi
}

function check_ifttt_configuration () {
  if [ "${IFTTT_ENABLED:-false}" = "true" ]
  then
    if [ -z "${IFTTT_EVENT_NAME+x}" ] || [ -z "${IFTTT_KEY+x}"  ]
    then
      log_progress "STOP: You're trying to setup IFTTT but didn't provide your Event Name and/or key."
      log_progress "Define the variables like this:"
      log_progress "export IFTTT_EVENT_NAME=put_your_event_name_here"
      log_progress "export IFTTT_KEY=put_your_key_here"
      exit 1
    elif [ "${IFTTT_EVENT_NAME}" = "put_your_event_name_here" ] || [  "${IFTTT_KEY}" = "put_your_key_here" ]
    then
      log_progress "STOP: You're trying to setup IFTTT, but didn't replace the default Event Name and/or key values."
      exit 1
    fi
  fi
}

function check_webhook_configuration () {
  if [ "${WEBHOOK_ENABLED:-false}" = "true" ]
  then
    if [ -z "${WEBHOOK_URL+x}"  ]
    then
      log_progress "STOP: You're trying to setup a Webhook but didn't provide your webhook url."
      log_progress "Define the variable like this:"
      log_progress "export WEBHOOK_URL=http://domain/path/"
      exit 1
    elif [ "${WEBHOOK_URL}" = "http://domain/path/" ]
    then
      log_progress "STOP: You're trying to setup a Webhook, but didn't replace the default url."
      exit 1
    fi
  fi
}

function check_slack_configuration () {
  if [ "${SLACK_ENABLED:-false}" = "true" ]
  then
    if [ -z "${SLACK_WEBHOOK_URL+x}"  ]
    then
      log_progress "STOP: You're trying to setup a Slack webhook but didn't provide your webhook url."
      log_progress "Define the variable like this:"
      log_progress "export SLACK_WEBHOOK_URL=http://domain/path/"
      exit 1
    elif [ "${SLACK_WEBHOOK_URL}" = "http://domain/path/" ]
    then
      log_progress "STOP: You're trying to setup a Slack webhook, but didn't replace the default url."
      exit 1
    fi
  fi
}

function check_matrix_configuration () {
  if [ "${MATRIX_ENABLED:-false}" = "true" ]
  then
      if [ -z "${MATRIX_SERVER_URL+x}"  ] || [ -z "${MATRIX_USERNAME+x}"  ] || [ -z "${MATRIX_PASSWORD+x}"  ] || [ -z "${MATRIX_ROOM+x}"  ]
      then
          log_progress "STOP: You're trying to setup Matrix but didn't provide your server URL, username, password or room."
          log_progress "Define the variable like this:"
          log_progress "export MATRIX_SERVER_URL=https://matrix.org"
          log_progress "export MATRIX_USERNAME=put_your_matrix_username_here"
          log_progress "export MATRIX_PASSWORD='put_your_matrix_password_here'"
          log_progress "export MATRIX_ROOM='put_the_matrix_target_room_id_here'"
          exit 1
      elif [ "${MATRIX_USERNAME}" = "put_your_matrix_username_here" ] || [ "${MATRIX_PASSWORD}" = "put_your_matrix_password_here" ] ||[ "${MATRIX_ROOM}" = "put_the_matrix_target_room_id_here" ]
      then
          log_progress "STOP: You're trying to setup Matrix, but didn't replace the default username, password or target room."
          exit 1
      fi
  fi
}

function check_sns_configuration () {
  if [ "${SNS_ENABLED:-false}" = "true" ]
  then
    if [ -z "${AWS_ACCESS_KEY_ID:+x}" ] || [ -z "${AWS_SECRET_ACCESS_KEY:+x}" ] || [ -z "${AWS_SNS_TOPIC_ARN:+x}" ]
    then
      echo "STOP: You're trying to setup AWS SNS but didn't provide your User and/or App key and/or topic ARN."
      echo "Define the variables like this:"
      echo "export AWS_ACCESS_KEY_ID=put_your_accesskeyid_here"
      echo "export AWS_SECRET_ACCESS_KEY=put_your_secretkey_here"
      echo "export AWS_SNS_TOPIC_ARN=put_your_sns_topicarn_here"
      exit 1
    elif [ "${AWS_ACCESS_KEY_ID}" = "put_your_accesskeyid_here" ] || [ "${AWS_SECRET_ACCESS_KEY}" = "put_your_secretkey_here" ] || [ "${AWS_SNS_TOPIC_ARN}" = "put_your_sns_topicarn_here" ]
    then
      echo "STOP: You're trying to setup SNS, but didn't replace the default values."
      exit 1
    fi
  fi
}

function check_telegram_configuration () {
  if [ "${TELEGRAM_ENABLED:-false}" = "true" ]
  then
    if [ -z "${TELEGRAM_BOT_TOKEN+x}"  ] || [ -z "${TELEGRAM_CHAT_ID:+x}" ]
    then
      log_progress "STOP: You're trying to setup Telegram but didn't provide your Bot Token or Chat id."
      echo "Define the variables in config file like this:"
      echo "export TELEGRAM_CHAT_ID=123456789"
      echo "export TELEGRAM_BOT_TOKEN=bot123456789:abcdefghijklmnopqrstuvqxyz987654321"
      exit 1
    elif [ "${TELEGRAM_BOT_TOKEN}" = "bot123456789:abcdefghijklmnopqrstuvqxyz987654321" ] || [ "${TELEGRAM_CHAT_ID}" = "123456789" ]
    then
      log_progress "STOP: You're trying to setup Telegram, but didn't replace the default values."
      exit 1
    fi
  fi
}

function configure_pushover () {
  # remove legacy file
  rm -f /root/.teslaCamPushoverCredentials

  if [ "${PUSHOVER_ENABLED:-false}" = "true" ]
  then
    log_progress "Pushover enabled"
  else
    log_progress "Pushover not enabled."
  fi
}

function configure_gotify () {
  # remove legacy file
  rm -f /root/.teslaCamGotifySettings

  if [ "${GOTIFY_ENABLED:-false}" = "true" ]
  then
    log_progress "Gotify enabled."
  else
    log_progress "Gotify not enabled."
  fi
}

function configure_discord () {
  if [ "${DISCORD_ENABLED:-false}" = "true" ]
  then
    log_progress "Discord enabled."
  else
    log_progress "Discord not enabled."
  fi
}

function configure_ifttt () {
  # remove legacy file
  rm -f /root/.teslaCamIftttSettings

  if [ "${IFTTT_ENABLED:-false}" = "true" ]
  then
    log_progress "IFTTT enabled."
  else
    log_progress "IFTTT not enabled."
  fi
}

function configure_telegram () {
  if [ "${TELEGRAM_ENABLED:-false}" = "true" ]
  then
    log_progress "Telegram enabled."
  else
    log_progress "Telegram not enabled."
  fi
}

function configure_webhook () {
  # remove legacy file
  rm -f /root/.teslaCamWebhookSettings

  if [ "${WEBHOOK_ENABLED:-false}" = "true" ]
  then
    log_progress "Webhook enabled."
  else
    log_progress "Webhook not enabled."
  fi
}

function configure_slack () {
  if [ "${SLACK_ENABLED:-false}" = "true" ]
  then
    log_progress "Slack enabled."
  else
    log_progress "Slack not enabled."
  fi
}

function configure_matrix () {
  if [ "${MATRIX_ENABLED:-false}" = "true" ]
  then
    log_progress "Enabling Matrix"
    install_matrix_packages
  else
    log_progress "Matrix not configured."
  fi
}

function configure_sns () {
  # remove legacy file
  rm -f /root/.teslaCamSNSTopicARN

  if [ "${SNS_ENABLED:-false}" = "true" ]
  then
    log_progress "Enabling SNS"
    mkdir -p /root/.aws

    rm -f /root/.aws/credentials

    echo "[default]" > /root/.aws/config
    echo "region = $AWS_REGION" >> /root/.aws/config

    install_sns_packages
  else
    log_progress "SNS not configured."
  fi
}

function check_and_configure_pushover () {
  check_pushover_configuration

  configure_pushover
}

function check_and_configure_gotify () {
  check_gotify_configuration

  configure_gotify
}

function check_and_configure_discord () {
  check_discord_configuration

  configure_discord
}

function check_and_configure_ifttt () {
  check_ifttt_configuration

  configure_ifttt
}

function check_and_configure_webhook () {
  check_webhook_configuration

  configure_webhook
}

function check_and_configure_slack () {
  check_slack_configuration

  configure_slack
}

function check_and_configure_matrix () {
  check_matrix_configuration

  configure_matrix
}

function check_and_configure_telegram () {
  check_telegram_configuration

  configure_telegram
}

function check_and_configure_sns () {
  check_sns_configuration

  configure_sns
}

function install_push_message_scripts() {
  local install_path="$1"
  copy_script run/send-push-message "$install_path"
  copy_script run/send_sns.py "$install_path"
  copy_script run/send_matrix.py "$install_path"
}

if [[ $EUID -ne 0 ]]
then
    log_progress "STOP: Run sudo -i."
    exit 1
fi

mkdir -p /root/bin

if check_at_most_one_wake_api
then
  check_teslafi_api
  check_tessie_api
  check_and_configure_tesla_ble /root/bin
  check_awake_webhook
fi
check_and_install_temperature_monitor /root/bin
check_and_configure_pushover
check_and_configure_gotify
check_and_configure_ifttt
check_and_configure_discord
check_and_configure_webhook
check_and_configure_slack
check_and_configure_matrix
check_and_configure_telegram
check_and_configure_sns
install_push_message_scripts /root/bin

check_archive_configs

rm -f /root/teslausb.conf
# Only clean TeslaCam symlinks and archive tracking when the cam disk was
# actually recreated (fresh install or size change).  create-backingfiles.sh
# already handles this in setup-sentryusb; doing it unconditionally here
# breaks the WebUI (symlinks gone → no clips visible) and forces the archive
# to re-copy every file (tracking file gone → nothing marked as archived).
if [ ! -e /backingfiles/cam_disk.bin ]
then
  rm -rf /mutable/TeslaCam/RecentClips /mutable/TeslaCam/SavedClips /mutable/TeslaCam/SentryClips /mutable/TeslaCam/TeslaTrackMode
  rm -f /mutable/sentry_files_archived
else
  # Existing install re-running setup. Flag a one-time symlink rebuild so
  # make_snapshot.sh restores any snapshot symlinks that may have been lost
  # by earlier versions of this script or other edge cases.
  touch /mutable/.rebuild_snapshot_symlinks
fi

archive_module="$( get_archive_module )"
log_progress "Using archive module: $archive_module"

install_archive_scripts /root/bin "$archive_module"
/tmp/verify-and-configure-archive.sh

systemctl disable teslausb.service || true
systemctl disable sentryusb-archive.service || true

cat << EOF > /lib/systemd/system/sentryusb-archive.service
[Unit]
Description=SentryUSB archiveloop service
DefaultDependencies=no
After=mutable.mount backingfiles.mount

[Service]
Type=simple
ExecStart=/bin/bash /root/bin/archiveloop
Restart=always

[Install]
WantedBy=backingfiles.mount
EOF

systemctl enable sentryusb-archive.service
