#!/bin/bash -eu

if [ "${BASH_SOURCE[0]}" != "$0" ]
then
  echo "${BASH_SOURCE[0]} must be executed, not sourced"
  return 1 # shouldn't use exit when sourced
fi

if [ "${FLOCKED:-}" != "$0" ]
then
  mkdir -p /backingfiles/snapshots
  if FLOCKED="$0" flock -E 99 /backingfiles/snapshots "$0" "$@" || case "$?" in
  99) echo "failed to lock snapshots dir"
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

function log_progress () {
  if declare -F setup_progress > /dev/null
  then
    setup_progress "configure-automount: $1"
    return
  fi
  echo "configure-automount: $1"
}

apt-get -y --force-yes install autofs
# the Raspbian Stretch autofs package does not include the /etc/auto.master.d folder
if [ ! -d /etc/auto.master.d ]
then
  mkdir /etc/auto.master.d
fi
copy_script run/auto.sentryusb /root/bin
echo "/tmp/snapshots  /root/bin/auto.sentryusb" > /etc/auto.master.d/sentryusb.autofs
rm -f /root/bin/mount_image.sh
log_progress "converting snapshot mountpoints to links"
for snapdir in /backingfiles/snapshots/snap-*/
do
  if [ ! -L "${snapdir}/mnt" ] && [ -d "${snapdir}/mnt" ]
  then
    umount "${snapdir}/mnt" || true
    rmdir "${snapdir}/mnt"
    ln -s "/tmp/snapshots/$(basename "$snapdir")" "${snapdir}/mnt"
  fi
done
log_progress "configured automount"
