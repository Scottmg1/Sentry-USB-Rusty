#!/bin/bash -eu

mutable_device=$(findmnt -n -o SOURCE /mutable)
backingfiles_device=$(findmnt -n -o SOURCE /backingfiles)
num_backingfiles_sectors=$(blockdev --getsz "$backingfiles_device")
num_wanted_mutable_inodes=$((num_backingfiles_sectors / 20000))
num_mutable_inodes=$(df --output=itotal /mutable | sed 1d)

if [[ "$num_mutable_inodes" -ge "$num_wanted_mutable_inodes" ]]
then
  echo "/mutable already has sufficient inodes ($num_mutable_inodes > $num_wanted_mutable_inodes)"
  exit 0
fi

echo "Want $num_wanted_mutable_inodes, have $num_mutable_inodes: reformatting."

systemctl stop sentryusb-archive || true
systemctl stop dnsmasq  || true
systemctl stop smbd || true
systemctl stop nmbd || true

tar -C /mutable --create --file /backingfiles/mutable.tgz . &> /tmp/tar.out

umount /mutable

mkfs.ext4 -F -N "$num_wanted_mutable_inodes" -L mutable "$mutable_device"

mount /mutable

tar -C /mutable --extract --file /backingfiles/mutable.tgz .
rm /backingfiles/mutable.tgz

echo "/mutable updated, please reboot now"
