#!/bin/bash

# Repro case for the issue described here: https://lore.kernel.org/lkml/8bed44f2-273c-856e-0018-69f127ea4258@linux.ibm.com/
# When running this script on an affected kernel/userspace combo,
# such as Raspberry Pi OS with kernel 5.10.103 and losetup 2.33.1,
# you should see an occasional message about losetup having failed.

TESTIMG=/backingfiles/losetuptest.bin
while true
do
  echo -n "$(date) looping: "
  dd if=/dev/zero of=$TESTIMG bs=1M count=128 status=none
  if losetup --sector-size=4096 -fP $TESTIMG
  then
    LOOP=$(losetup -n -O NAME -j /backingfiles/losetuptest.bin)
    echo "$LOOP"
    losetup -d $LOOP
  else
    echo losetup failed:
    LOOP=$(losetup -n -O NAME -j /backingfiles/losetuptest.bin)
    if [ -z "$LOOP" ]
    then
      echo "No loop device exists"
    else
      echo "loop device exists: $LOOP"
      losetup -d $LOOP
    fi
    losetup -l
  fi
  rm $TESTIMG
done
