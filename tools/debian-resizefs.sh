#!/bin/bash
#
# Resize root filesystem during boot.
#
# VERSION       :1.0.1
# DATE          :2018-04-01
# URL           :https://github.com/szepeviktor/debian-server-tools
# AUTHOR        :Viktor Szépe <viktor@szepe.net>
# LICENSE       :The MIT License (MIT)
# BASH-VERSION  :4.2+
# ALTERNATIVE   :http://www.ivarch.com/blogs/oss/2007/01/resize-a-live-root-fs-a-howto.shtml

# Check current filesystem type
ROOT_FS_TYPE="$(sed -n -e 's|^/dev/\S\+ / \(ext4\) .*$|\1|p' /proc/mounts)"
test "$ROOT_FS_TYPE" == ext4 || exit 100

# Copy e2fsck, resize2fs, and mount to initrd
cat > /etc/initramfs-tools/hooks/resize2fs <<"EOF"
#!/bin/sh

PREREQ=""

prereqs() {
    echo "$PREREQ"
}

case "$1" in
    prereqs)
        prereqs
        exit 0
        ;;
esac

. /usr/share/initramfs-tools/hook-functions
copy_exec $(readlink -f /sbin/findfs) /sbin/findfs-full
copy_exec /sbin/e2fsck /sbin
copy_exec /sbin/resize2fs /sbin
copy_exec /bin/mount /bin
copy_exec /bin/umount /bin
EOF

chmod +x /etc/initramfs-tools/hooks/resize2fs

# Execute resize2fs before mounting root filesystem
cat > /etc/initramfs-tools/scripts/init-premount/resize <<EOF
#!/bin/sh

PREREQ=""

# New size of root filesystem
ROOT_SIZE=${1:-"8G"}

EOF
cat >> /etc/initramfs-tools/scripts/init-premount/resize <<"EOF"
prereqs() {
    echo "$PREREQ"
}

case "$1" in
    prereqs)
        prereqs
        exit 0
        ;;
esac

# Convert root from possible UUID to device name
echo
echo "root=${ROOT}  "
while [ ! -d /dev/disk/by-partuuid ]
do
  echo "waiting for /dev/disk/by-partuuid"
  sleep 1
done
ROOT_DEVICE="$(/sbin/findfs-full "$ROOT")"
echo "root device name is ${ROOT_DEVICE}  "
# Make sure LVM volumes are activated
if [ -x /sbin/vgchange ]; then
    /sbin/vgchange -a y || echo "vgchange: $?  "
fi
# Write a result marker to the root filesystem so userspace can verify
write_resize_marker() {
  mkdir -p /tmp/rootmnt
  if mount "$ROOT_DEVICE" /tmp/rootmnt 2>/dev/null; then
    echo "$1" > /tmp/rootmnt/root/RESIZE_RESULT
    umount /tmp/rootmnt 2>/dev/null || true
  fi
  rmdir /tmp/rootmnt 2>/dev/null || true
}

# Check root filesystem
if /sbin/e2fsck -y -v -f "$ROOT_DEVICE"; then
  # Resize
  # debug-flag 8 means debug moving the inode table
  # -f means ignore various checks, which is needed for devices with a bad clock.
  # This should be safe, because e2fsck just completed successfully.
  if /sbin/resize2fs -f -d 8 "$ROOT_DEVICE" "$ROOT_SIZE"; then
    echo "resize2fs completed successfully"
    write_resize_marker "success"
  else
    RC=$?
    echo "resize2fs failed with exit code $RC"
    write_resize_marker "fail:resize2fs:$RC"
  fi
else
  RC=$?
  echo "e2fsck $ROOT_DEVICE failed with exit code $RC"
  write_resize_marker "fail:e2fsck:$RC"
fi
EOF

chmod +x /etc/initramfs-tools/scripts/init-premount/resize

# Regenerate initrd
update-initramfs -v -u -k "$(uname -r)"

# On Bookworm the boot partition is /boot/firmware/, not /boot/.
# Copy the updated initramfs there so the bootloader can find it.
INITRD_NAME="initrd.img-$(uname -r)"
if [ -L /sentryusb ]; then
  BOOT_PART="$(readlink -f /sentryusb)"
  if [ "/boot" != "${BOOT_PART}" ] && [ -e "/boot/${INITRD_NAME}" ]; then
    cp "/boot/${INITRD_NAME}" "${BOOT_PART}/${INITRD_NAME}"
  fi
fi

# Remove files
rm -f /etc/initramfs-tools/hooks/resize2fs /etc/initramfs-tools/scripts/init-premount/resize

reboot

# List files in initrd
# lsinitramfs /boot/initrd.img-*-amd64

# Remove files from initrd after reboot
# update-initramfs -u
