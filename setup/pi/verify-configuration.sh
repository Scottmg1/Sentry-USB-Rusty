#!/bin/bash -eu

function check_variable () {
  local var_name="$1"
  if [ -z "${!var_name+x}" ]
  then
    setup_progress "STOP: Define the variable $var_name like this: export $var_name=value"
    exit 1
  fi
}

function check_supported_hardware () {
  if ! grep -q  'Raspberry Pi' /sys/firmware/devicetree/base/model
  then
    return
  fi
  if grep -q 'Raspberry Pi Zero W' /sys/firmware/devicetree/base/model
  then
    return
  fi
  if grep -q 'Raspberry Pi Zero 2' /sys/firmware/devicetree/base/model
  then
    return
  fi
  if grep -q 'Raspberry Pi 3' /sys/firmware/devicetree/base/model
  then
    return
  fi
  if grep -q 'Raspberry Pi 4' /sys/firmware/devicetree/base/model
  then
    return
  fi
  if grep -q 'Raspberry Pi 5' /sys/firmware/devicetree/base/model
  then
    return
  fi
  setup_progress "STOP: unsupported hardware: '$(cat /sys/firmware/devicetree/base/model)'"
  setup_progress "(only Pi Zero W, Pi Zero 2 W, Pi 3, Pi 4, and Pi 5 have the necessary hardware to run SentryUSB)"
  exit 1
}

function check_udc () {
  local udc
  udc=$(find /sys/class/udc -type l -prune | wc -l)
  if [ "$udc" = "0" ]
  then
    setup_progress "STOP: this device ($(cat /sys/firmware/devicetree/base/model)) does not have a UDC driver"
    setup_progress "(check that dtoverlay=dwc2 is in the correct section of config.txt for your Pi model)"
    exit 1
  fi
}

function check_xfs () {
  setup_progress "Checking XFS support"
  # install XFS tools if needed
  if ! hash mkfs.xfs
  then
    apt-get -y --force-yes install xfsprogs
  fi
  truncate -s 1GB /tmp/xfs.img
  mkfs.xfs -m reflink=1 -f /tmp/xfs.img > /dev/null
  mkdir -p /tmp/xfsmnt
  if ! mount /tmp/xfs.img /tmp/xfsmnt
  then
    setup_progress "STOP: xfs does not support required features"
    exit 1
  fi

  umount /tmp/xfsmnt
  rm -rf /tmp/xfs.img /tmp/xfsmnt
  setup_progress "XFS supported"
}

function check_available_space () {
    if [ -z "$DATA_DRIVE" ]
    then
      setup_progress "DATA_DRIVE is not set. SD card will be used."
      check_available_space_sd
    else
      if [ -e "$DATA_DRIVE" ]
      then
        setup_progress "DATA_DRIVE is set to $DATA_DRIVE. This will be used for /mutable and /backingfiles."
        check_available_space_usb
      else
        setup_progress "STOP: DATA_DRIVE is set to $DATA_DRIVE, which does not exist."
        exit 1
      fi
    fi
}

function check_available_space_sd () {
  setup_progress "Verifying that there is sufficient space available on the MicroSD card..."

  # Minimum usable space: 8 GiB for backingfiles partition, or 8 GiB unpartitioned.
  # Old 32 GiB threshold blocked cards smaller than ~38 GB even after root shrink.
  local min_space=$(( (1<<30) * 8 ))

  # check if backingfiles and mutable already exist
  if [ -e /dev/disk/by-label/backingfiles ] && [ -e /dev/disk/by-label/mutable ]
  then
    backingfiles_size=$(blockdev --getsize64 /dev/disk/by-label/backingfiles)
    if [ "$backingfiles_size" -lt "$min_space" ]
    then
      setup_progress "STOP: Existing backingfiles partition is too small ($(( backingfiles_size / 1024 / 1024 / 1024 ))GB, need at least 8GB)"
      exit 1
    fi
  else
    # The following assumes that all the partitions are at the start
    # of the disk, and that all the free space is at the end.

    local available_space

    # query unpartitioned space
    available_space=$(sfdisk -F "$BOOT_DISK" | grep -o '[0-9]* bytes' | head -1 | awk '{print $1}')

    if [ "$available_space" -lt "$min_space" ]
    then
      setup_progress "STOP: The MicroSD card is too small: $(( available_space / 1024 / 1024 / 1024 ))GB available, need at least 8GB."
      setup_progress "$(parted "${BOOT_DISK}" print)"
      exit 1
    fi
  fi

  setup_progress "There is sufficient space available."
}

function check_available_space_usb () {
  setup_progress "Verifying that there is sufficient space available on the USB drive ..."

  # Verify that the disk has been provided and not a partition.
  # Use timeout to avoid hanging indefinitely on unresponsive drives
  # (e.g. USB drive in sleep mode, I/O errors after interrupted setup).
  local drive_type
  drive_type=$(timeout 30 lsblk -pno TYPE "$DATA_DRIVE" 2>/dev/null | head -n 1) || {
    setup_progress "STOP: Could not read $DATA_DRIVE (drive may be unresponsive or disconnected). Try unplugging and reconnecting it."
    exit 1
  }

  if [ "$drive_type" != "disk" ]
  then
    setup_progress "STOP: The specified drive ($DATA_DRIVE) is not a disk (TYPE=$drive_type). Please specify path to the disk."
    exit 1
  fi

  # This verifies only the total size of the USB Drive.
  # All existing partitions on the drive will be erased if backingfiles are to be created or changed.
  # EXISTING DATA ON THE DATA_DRIVE WILL BE REMOVED.

  local drive_size
  drive_size=$(timeout 30 blockdev --getsize64 "$DATA_DRIVE") || {
    setup_progress "STOP: Could not read size of $DATA_DRIVE (drive may be unresponsive). Try unplugging and reconnecting it."
    exit 1
  }

  # Require at least 64GB drive size, or 59 GiB.
  if [ "$drive_size" -lt  $(( (1<<30) * 59)) ]
  then
    setup_progress "STOP: The USB drive is too small: $(( drive_size / 1024 / 1024 / 1024 ))GB available. Expected at least 64GB"
    setup_progress "$(parted "$DATA_DRIVE" print)"
    exit 1
  fi

  setup_progress "There is sufficient space available."
}

function check_setup_sentryusb () {
  if [ ! -e /root/bin/setup-sentryusb ]
  then
    setup_progress "STOP: setup-sentryusb is not in /root/bin"
    exit 1
  fi

  local parent
  parent="$(ps -o comm= $PPID)"
  if [ "$parent" != "setup-sentryusb" ]
  then
    setup_progress "STOP: $0 must be called from setup-sentryusb: $parent"
    exit 1
  fi
}

check_supported_hardware

check_udc

check_xfs

check_setup_sentryusb

check_variable "CAM_SIZE"

check_available_space
