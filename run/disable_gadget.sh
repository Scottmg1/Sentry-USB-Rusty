#!/bin/bash -eu

# g_mass_storage module may be loaded on a system that
# is being transitioned from module to configfs
modprobe -q -r g_mass_storage || true

if ! configfs_root=$(findmnt -o TARGET -n configfs)
then
  echo "error: configfs not found"
  exit 1
fi
readonly gadget_root="$configfs_root/usb_gadget/sentryusb"

if [ ! -d "$gadget_root" ]
then
  echo "already released"
  exit 0
fi

echo > "$gadget_root/UDC" || true
rmdir "$gadget_root"/configs/*/strings/* || true
rm -f "$gadget_root"/configs/*/mass_storage.0 || true
rmdir "$gadget_root"/functions/mass_storage.0/lun.1 &> /dev/null || true
rmdir "$gadget_root"/functions/mass_storage.0/lun.2 &> /dev/null || true
rmdir "$gadget_root"/functions/mass_storage.0/lun.3 &> /dev/null || true
rmdir "$gadget_root"/functions/mass_storage.0/lun.4 &> /dev/null || true
rmdir "$gadget_root"/functions/mass_storage.0 || true
rmdir "$gadget_root"/configs/* || true
rmdir "$gadget_root"/strings/* || true
rmdir "$gadget_root"

modprobe -r usb_f_mass_storage g_ether usb_f_ecm usb_f_rndis libcomposite || true
