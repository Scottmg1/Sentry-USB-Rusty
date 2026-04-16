#!/bin/bash -eu

function usage {
  echo "usage: $0 <size> <image>"
}

function dehumanize () {
  echo $(($(echo "$1" | sed 's/GB/G/;s/MB/M/;s/KB/K/;s/G/*1024M/;s/M/*1024K/;s/K/*1024/')))
}

function closeenough () {
  DIFF=$(($1-$2))
  if [ $DIFF -ge 0 ] && [ $DIFF -lt 1048576 ]
  then
    true
    return
  elif [ $DIFF -lt 0 ] && [ $DIFF -gt -1048576 ]
  then
    true
    return
  fi
  false
}

if [[ $# -ne 2 ]]
then
  usage
  exit 1
fi

NEWSIZE=$(dehumanize "$1")
FILE=$2

if [ ! -e "$FILE" ]
then
  echo "No such file: $FILE"
  usage
  exit 1
fi

if findmnt /mnt/cam > /dev/null
then
  echo "cam drive is mounted. Please ensure no archiving operation is in progress"
  exit 1
fi

if findmnt /mnt/music > /dev/null
then
  echo "music drive is mounted. Please ensure no music sync operation is in progress"
  exit 1
fi

# remove device from any attached host
/root/bin/disable_gadget.sh

# fsck the image, since we may have just yanked it out from under the host.
# Use -p repair arg. It works with vfat and exfat.
DEVLOOP=$(losetup --show -P -f "$FILE")
udevadm settle --timeout=5 2>/dev/null || true
PARTLOOP=${DEVLOOP}p1
fsck "$PARTLOOP" -- -p > /dev/null || true

# Detect filesystem type on the partition
FS_TYPE=$(blkid -s TYPE -o value "$PARTLOOP" 2>/dev/null || echo "unknown")
IS_EXFAT=false
if [ "$FS_TYPE" = "exfat" ]
then
  IS_EXFAT=true
fi

# install fatresize if needed (only useful for FAT16/FAT32)
if [ "$IS_EXFAT" = false ] && ! hash fatresize &> /dev/null
then
  /root/bin/remountfs_rw
  apt install -y fatresize
fi

# get size of the image file and the partition within
CURRENT_PARTITION_SIZE=$(($(partx -o SECTORS -g -n 1 "$FILE") * 512 + 512))
PARTITION_OFFSET=$(($(partx -o START -g -n 1 "$FILE") * 512))

# fatresize doesn't seem to like extending partitions to the very end of the file
# and sometimes segfault in that case, so add some padding
PARTITION_PADDING=65536

ORIGINAL_FILE_SIZE=$(stat --printf="%s" "$FILE")
RESIZE_OK=true

if closeenough $CURRENT_PARTITION_SIZE "$NEWSIZE"
then
  echo "no sizing needed"
elif [ "$IS_EXFAT" = true ]
then
  echo "exFAT filesystem detected -- fatresize does not support exFAT"
  echo "resize not supported for exFAT images, image must be recreated"
  RESIZE_OK=false
elif [ $CURRENT_PARTITION_SIZE -lt "$NEWSIZE" ]
then
  echo "growing"
  fallocate -l $((PARTITION_OFFSET + NEWSIZE + PARTITION_PADDING)) "$FILE"
  if ! fatresize -s "$NEWSIZE" "$FILE" > /dev/null
  then
    echo "fatresize failed during grow, rolling back file to original size"
    truncate -s "$ORIGINAL_FILE_SIZE" "$FILE"
    RESIZE_OK=false
  fi
else
  echo "shrinking"
  if fatresize -s "$NEWSIZE" "$FILE" > /dev/null
  then
    PARTITION_END=$(($(partx -o END -g -n 1 "$FILE") * 512 + 512))
    truncate -s $((PARTITION_END + PARTITION_PADDING)) "$FILE"
  else
    echo "fatresize failed during shrink, image left at current size"
    RESIZE_OK=false
  fi
fi

losetup -d "$DEVLOOP"

if [ "$RESIZE_OK" = false ]
then
  exit 1
fi

