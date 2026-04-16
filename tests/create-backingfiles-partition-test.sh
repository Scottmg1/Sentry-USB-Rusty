#!/bin/bash

if [[ $EUID -ne 0 ]]
then
  echo "STOP: Run this script as root."
  exit 1
fi

if [ "$(systemd-detect-virt)" == "none" ]
then
  echo "WARNING, it is recommended to run this script in a VM."
  echo "Press enter to continue, or ctrl-c to abort."
  read
fi

function checkenv {
  if [ -e /dev/disk/by-label/backingfiles ] || [ -e /dev/disk/by-label/mutable ] || grep -q LABEL=backingfiles /etc/fstab || grep -q LABEL=backingfiles /etc/fstab
  then
    echo "STOP: mutable or backingfiles already exist and/or are listed in fstab. Restart with clean environment."
    exit 1
  fi
}

function checknewpartitions {
  partprobe "$LOOP"
  sleep 2
  if [ ! -e /dev/disk/by-label/backingfiles ] || [ ! -e /dev/disk/by-label/mutable ]
  then
    echo "ERROR: mutable or backingfiles not created:"
    ls -l /dev/disk/by-label
    return 1
  fi
  if [ -n "$DATA_DRIVE" ]
  then
    lsblk -no NAME,LABEL "$DATA_DRIVE" | grep -q backingfiles || return 1
    lsblk -no NAME,LABEL "$DATA_DRIVE" | grep -q mutable || return 1
  else
    lsblk -no NAME,LABEL "$LOOP" | grep -q backingfiles || return 1
    lsblk -no NAME,LABEL "$LOOP" | grep -q mutable || return 1
  fi
  if ! grep -q LABEL=backingfiles /etc/fstab || ! grep -q LABEL=backingfiles /etc/fstab
  then
    echo "ERROR: fstab not correctly updated"
    cat /etc/fstab
    return 1
  fi
}

# Standard Raspberry Pi OS image with boot and root partition
function makestandardpiimage {
  local img="$1"
  truncate -s $((64*1024*1024*1024)) "$img"

  sfdisk "$img" << EOF
,+500M
,+2G
EOF

  fdisk -l "$img"

  LOOP=$(sudo losetup --find --partscan --show "$img")

  mkfs.vfat "${LOOP}p1" &>> "$LOG"
  mkfs.ext4 "${LOOP}p2" &>> "$LOG"

  export BOOT_DISK="$LOOP"
  export DATA_DRIVE=""
  export CMDLINE_PATH="/dev/null"
  export BOOT_DEVICE_PARTITION_PREFIX="${LOOP}p"

  echo "BOOT_DISK: $BOOT_DISK"
}

# image with dos partition layout and three partitions
# on it already. This should prevent backingfiles and
# mutable from being created
function maketriplepartdosimage {
  local img="$1"
  truncate -s $((64*1024*1024*1024)) "$img"

  sfdisk "$img" << EOF
,+500M
,+2G
,+2G
EOF

  fdisk -l "$img"

  LOOP=$(sudo losetup --find --partscan --show "$img")

  mkfs.vfat "${LOOP}p1" &>> "$LOG"
  mkfs.ext4 "${LOOP}p2" &>> "$LOG"
  mkfs.ext4 "${LOOP}p3" &>> "$LOG"

  export BOOT_DISK="$LOOP"
  export DATA_DRIVE=""
  export CMDLINE_PATH="/dev/null"
  export BOOT_DEVICE_PARTITION_PREFIX="${LOOP}p"

  echo "BOOT_DISK: $BOOT_DISK"
}

# image with gpt partition layout and three partitions
# on it already. Since gpt supports more than 4 primary
# partitions, backingfiles and mutable can still be
# created
function maketriplepartgptimage {
  local img="$1"
  truncate -s $((64*1024*1024*1024)) "$img"

  sfdisk -X gpt "$img" << EOF
,+500M
,+2G
,+2G
EOF

  fdisk -l "$img"

  LOOP=$(sudo losetup --find --partscan --show "$img")

  mkfs.vfat "${LOOP}p1" &>> "$LOG"
  mkfs.ext4 "${LOOP}p2" &>> "$LOG"
  mkfs.ext4 "${LOOP}p3" &>> "$LOG"

  export BOOT_DISK="$LOOP"
  export DATA_DRIVE=""
  export CMDLINE_PATH="/dev/null"
  export BOOT_DEVICE_PARTITION_PREFIX="${LOOP}p"

  echo "BOOT_DISK: $BOOT_DISK"
}

# image with gpt partition layout and three partitions
# on it already, but with the on-disk order not matching
# the partition table order.
function makeubuntulikeimage {
  local img="$1"
  truncate -s $((64*1024*1024*1024)) "$img"

  sfdisk -X gpt "$img" << EOF
2560M,3G
12M,500M
512M,2G
EOF

  fdisk -l "$img"

  LOOP=$(sudo losetup --find --partscan --show "$img")

  mkfs.ext4 "${LOOP}p1" &>> "$LOG"
  mkfs.vfat "${LOOP}p2" &>> "$LOG"
  mkfs.ext4 "${LOOP}p3" &>> "$LOG"

  export BOOT_DISK="$LOOP"
  export DATA_DRIVE=""
  export CMDLINE_PATH="/dev/null"
  export BOOT_DEVICE_PARTITION_PREFIX="${LOOP}p"

  echo "BOOT_DISK: $BOOT_DISK"
}


SUCCESS=true

function checksuccess {
  printf 'Starting "%s"\n' "$1"
  cp /etc/fstab /etc/fstab.org
  if ../setup/pi/create-backingfiles-partition.sh /backingfiles /mutable && checknewpartitions
  then
    printf '%-45s %s\n' "$1" OK
    printf '%-45s %s\n' "$1" OK > /dev/tty
  else
    printf '%-45s %s\n' "$1" FAIL
    printf '%-45s %s\n' "$1" FAIL > /dev/tty
    SUCCESS=false
  fi
  mv /etc/fstab.org /etc/fstab
}

function checkfailure {
  printf 'Starting "%s"\n' "$1"
  if ! ../setup/pi/create-backingfiles-partition.sh
  then
    printf '%-45s %s\n' "$1" OK
    printf '%-45s %s\n' "$1" OK > /dev/tty
  else
    printf '%-45s %s\n' "$1" FAIL
    printf '%-45s %s\n' "$1" FAIL > /dev/tty
    SUCCESS=false
  fi
}

function deleteimage {
  if [ -e "$LOOP" ]
  then
    losetup -d "$LOOP"
  fi
  if [ -e "$DATA_DRIVE" ]
  then
    losetup -d "$DATA_DRIVE"
  fi
  rm -f "$ROOT_IMAGE"
  rm -f "$EXT_IMAGE"
}

# single partition disk like Armbian uses
function makearmbianlikeimage {
  local img="$1"

  truncate -s $((64*1024*1024*1024)) "$img"
  sfdisk "$ROOT_IMAGE" << EOF
,+2G
EOF

  fdisk -l "$img"

  LOOP=$(sudo losetup --find --partscan --show "$img")
  mkfs.ext4 "${LOOP}p1"

  export BOOT_DISK="$LOOP"
  export DATA_DRIVE=""
  export CMDLINE_PATH="/dev/null"
  export BOOT_DEVICE_PARTITION_PREFIX="${LOOP}p"

  echo "BOOT_DISK: $BOOT_DISK"
}

function makeexternaldriveimage {
  local img="$1"
  truncate -s $((64*1024*1024*1024)) "$img"

  export DATA_DRIVE=$(sudo losetup --find --partscan --show "$img")
}

ROOT_IMAGE=/tmp/createbackingfilepartitiontest$$.img
EXT_IMAGE=/tmp/createbackingfilepartitiontestext$$.img
LOG=/tmp/createbackingfilepartitiontest$$.log

checkenv

{
  makestandardpiimage "$ROOT_IMAGE"
  checksuccess "two partitions"
  checksuccess "two partitions repeat"
  deleteimage

  makearmbianlikeimage "$ROOT_IMAGE"
  checksuccess "one partition"
  checksuccess "one partition repeat"
  deleteimage

  makeubuntulikeimage "$ROOT_IMAGE"
  checksuccess "out of order partitions"
  checksuccess "out of order partitions repeat"
  deleteimage

  maketriplepartdosimage "$ROOT_IMAGE"
  checkfailure "three partitions dos"
  checkfailure "three partitions dos repeat"
  deleteimage

  maketriplepartgptimage "$ROOT_IMAGE"
  checksuccess "three partitions gpt"
  checksuccess "three partitions gpt repeat"
  deleteimage

  makestandardpiimage "$ROOT_IMAGE"
  makeexternaldriveimage "$EXT_IMAGE"
  checksuccess "two partitions, external data"
  checksuccess "two partitions, external data repeat"
  deleteimage

  maketriplepartdosimage "$ROOT_IMAGE"
  makeexternaldriveimage "$EXT_IMAGE"
  checksuccess "three partitions dos, external data"
  checksuccess "three partitions dos, external data repeat"
  deleteimage
} &>> "$LOG"

if [ "$SUCCESS" = "true" ]
then
  rm "$LOG"
else
  echo "One or more tests failed, see $LOG for details."
  exit 1
fi
