#!/bin/bash -eu

if [ "${BASH_SOURCE[0]}" = "$0" ]
then
  echo "$0 must be sourced, not executed"
  exit 1
fi

if [ ! -L /sentryusb ]
then
  mount / -o remount,rw
  rm -rf /sentryusb
  if [ -d /boot/firmware ] && findmnt --fstab /boot/firmware &> /dev/null
  then
    ln -s /boot/firmware /sentryusb
  else
    ln -s /boot /sentryusb
  fi
fi

function safesource {
  cat <<EOF > /tmp/checksetupconf
#!/bin/bash -eu
source '$1' &> /tmp/checksetupconf.out
EOF
  chmod +x /tmp/checksetupconf
  if ! /tmp/checksetupconf
  then
    if declare -F setup_progress > /dev/null
    then
      setup_progress "Error in $1:"
      setup_progress "$(cat /tmp/checksetupconf.out)"
    else
      echo "Error in $1:"
      cat /tmp/checksetupconf.out
    fi
    exit 1
  fi
  # shellcheck disable=SC1090
  source "$1"
}

function read_setup_variables {
  if [ -z "${setup_file+x}" ]
  then
    local -r setup_file=/root/sentryusb.conf
  fi
  if [ -e $setup_file ]
  then
    # "shellcheck" doesn't realize setup_file is effectively a constant
    # shellcheck disable=SC1090
    safesource $setup_file
  else
    echo "couldn't find $setup_file"
    return 1
  fi

  # TODO: change this "declare" to "local" when github updates
  # to a newer shellcheck.
  declare -A newnamefor

  newnamefor[archiveserver]=ARCHIVE_SERVER
  newnamefor[camsize]=CAM_SIZE
  newnamefor[musicsize]=MUSIC_SIZE
  newnamefor[sharename]=SHARE_NAME
  newnamefor[musicsharename]=MUSIC_SHARE_NAME
  newnamefor[shareuser]=SHARE_USER
  newnamefor[sharepassword]=SHARE_PASSWORD
  newnamefor[tesla_email]=TESLA_EMAIL
  newnamefor[tesla_password]=TESLA_PASSWORD
  newnamefor[tesla_vin]=TESLA_VIN
  newnamefor[timezone]=TIME_ZONE
  newnamefor[usb_drive]=DATA_DRIVE
  newnamefor[USB_DRIVE]=DATA_DRIVE
  newnamefor[archivedelay]=ARCHIVE_DELAY
  newnamefor[trigger_file_saved]=TRIGGER_FILE_SAVED
  newnamefor[trigger_file_sentry]=TRIGGER_FILE_SENTRY
  newnamefor[trigger_file_any]=TRIGGER_FILE_ANY
  newnamefor[pushover_enabled]=PUSHOVER_ENABLED
  newnamefor[pushover_user_key]=PUSHOVER_USER_KEY
  newnamefor[pushover_app_key]=PUSHOVER_APP_KEY
  newnamefor[gotify_enabled]=GOTIFY_ENABLED
  newnamefor[gotify_domain]=GOTIFY_DOMAIN
  newnamefor[gotify_app_token]=GOTIFY_APP_TOKEN
  newnamefor[gotify_priority]=GOTIFY_PRIORITY
  newnamefor[ifttt_enabled]=IFTTT_ENABLED
  newnamefor[ifttt_event_name]=IFTTT_EVENT_NAME
  newnamefor[ifttt_key]=IFTTT_KEY
  newnamefor[sns_enabled]=SNS_ENABLED
  newnamefor[aws_region]=AWS_REGION
  newnamefor[aws_access_key_id]=AWS_ACCESS_KEY_ID
  newnamefor[aws_secret_key]=AWS_SECRET_ACCESS_KEY
  newnamefor[aws_sns_topic_arn]=AWS_SNS_TOPIC_ARN

  local oldname
  for oldname in "${!newnamefor[@]}"
  do
    local newname=${newnamefor[$oldname]}
    if [[ -z ${!newname+x} ]] && [[ -n ${!oldname+x} ]]
    then
      local value=${!oldname}
      export $newname="$value"
      unset $oldname
    fi
  done

  # set defaults for things not set in the config
  REPO=${REPO:-Scottmg1}
  REPO_NAME=${REPO_NAME:-Sentry-USB}
  SNAPSHOTS_ENABLED=${SNAPSHOTS_ENABLED:-true}
  if [ "$SNAPSHOTS_ENABLED" != "true" ]
  then
    BRANCH="no-snapshots"
    if declare -F setup_progress > /dev/null
    then
      setup_progress "WARNING: using '$BRANCH' branch because SNAPSHOTS_ENABLED is not true"
    else
      echo "WARNING: using '$BRANCH' branch because SNAPSHOTS_ENABLED is not true"
    fi
  else
    BRANCH=${BRANCH:-main-dev}
  fi
  CONFIGURE_ARCHIVING=${CONFIGURE_ARCHIVING:-true}
  UPGRADE_PACKAGES=${UPGRADE_PACKAGES:-false}
  export SENTRYUSB_HOSTNAME=${SENTRYUSB_HOSTNAME:-sentryusb}
  export NOTIFICATION_TITLE=${NOTIFICATION_TITLE:-${SENTRYUSB_HOSTNAME}}
  SAMBA_ENABLED=${SAMBA_ENABLED:-false}
  SAMBA_GUEST=${SAMBA_GUEST:-false}
  INCREASE_ROOT_SIZE=${INCREASE_ROOT_SIZE:-0}
  export CAM_SIZE=${CAM_SIZE:-0}
  export MUSIC_SIZE=${MUSIC_SIZE:-0}
  export BOOMBOX_SIZE=${BOOMBOX_SIZE:-0}
  export LIGHTSHOW_SIZE=${LIGHTSHOW_SIZE:-0}
  export DATA_DRIVE=${DATA_DRIVE:-''}
  export USE_EXFAT=${USE_EXFAT:-false}
  export RTC_BATTERY_ENABLED=${RTC_BATTERY_ENABLED:-false}
  export RTC_TRICKLE_CHARGE=${RTC_TRICKLE_CHARGE:-false}
}

read_setup_variables

# Load mobile push notification credentials from the JSON file managed by the
# Go server.  These are NOT stored in sentryusb.conf — the JSON file is the
# single source of truth so we avoid conf-write race conditions.
NOTIFICATION_CREDENTIALS_JSON="/root/.sentryusb/notification-credentials.json"
if [ "${MOBILE_PUSH_ENABLED:-false}" = "true" ] && [ -f "$NOTIFICATION_CREDENTIALS_JSON" ]; then
  MOBILE_PUSH_DEVICE_ID=$(sed -n 's/.*"device_id" *: *"\([^"]*\)".*/\1/p' "$NOTIFICATION_CREDENTIALS_JSON")
  MOBILE_PUSH_SECRET=$(sed -n 's/.*"device_secret" *: *"\([^"]*\)".*/\1/p' "$NOTIFICATION_CREDENTIALS_JSON")
  export MOBILE_PUSH_DEVICE_ID MOBILE_PUSH_SECRET
fi

if [ -t 0 ]
then
  if ! declare -F log > /dev/null 
  then
    function log { echo "$@"; }
    export -f log
  fi
  complete -W "diagnose upgrade install" setup-sentryusb
fi

function isRaspberryPi {
  grep -q "Raspberry Pi" /sys/firmware/devicetree/base/model
}

function isPi5 {
  grep -q "Raspberry Pi 5" /sys/firmware/devicetree/base/model
}
export -f isPi5

function isPi4 {
  grep -q "Raspberry Pi 4" /sys/firmware/devicetree/base/model
}
export -f isPi4

function isPi2 {
  grep -q "Raspberry Pi Zero 2" /sys/firmware/devicetree/base/model
}
export -f isPi2

function isPi3 {
  grep -q "Raspberry Pi 3" /sys/firmware/devicetree/base/model
}
export -f isPi3

function isRockPi4 {
  grep -q "ROCK Pi 4" /sys/firmware/devicetree/base/model
}
export -f isRockPi4

function isRadxaZero {
  grep -q "Radxa Zero" /sys/firmware/devicetree/base/model
}
export -f isRadxaZero

STATUSLED=/tmp/fakeled

while read -r led
do
  case "$led" in
    *status | */led0 | */ACT | */user-led2 | */radxa-zero:green)
      STATUSLED="$led"
      break;
      ;;
    *)
      ;;
    esac
done < <(find /sys/class/leds -type l)

if [ ! -d "$STATUSLED" ]
then
  mkdir -p "$STATUSLED"
fi

if [ -f /sentryusb/cmdline.txt ]
then
  export CMDLINE_PATH=/sentryusb/cmdline.txt
else
  export CMDLINE_PATH=/dev/null
fi

if [ -f /sentryusb/config.txt ]
then
  export PICONFIG_PATH=/sentryusb/config.txt
else
  export PICONFIG_PATH=/dev/null
fi

# losetup sometimes fails because of a mismatch between kernel and user land
# (https://lore.kernel.org/lkml/8bed44f2-273c-856e-0018-69f127ea4258@linux.ibm.com/)
# but even when it fails like that, testing shows the loop device gets created anyway
function losetup_find_show {
  local lastarg="${@:$#}"
  local loop=$(losetup -n -O NAME -j "$lastarg")
  if losetup -f --show "$@"
  then
    return
  fi
  if [ -n "$loop" ]
  then
    # losetup failed, and there was already a previous loop device for the
    # given file.
    # Rather than trying to determine if a new loop device was created, just return
    # an error.
    return 1
  fi
  local newloop=$(losetup -n -O NAME -j "$lastarg")
  if [ -z "$newloop" ]
  then
    # losetup truly failed and didn't even create a loop device
    return 1
  fi
  echo "$newloop"
}

export -f losetup_find_show
