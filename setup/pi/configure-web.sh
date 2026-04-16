#!/bin/bash -eu

setup_progress "configuring web (SentryUSB mode)"

# Install only the packages we actually need (no nginx — SentryUSB serves the web UI)
apt-get -y --force-yes install fuse libfuse-dev g++ net-tools wireless-tools ethtool

# Stop and disable nginx if it was previously installed — SentryUSB owns port 80
if systemctl is-active --quiet nginx 2>/dev/null; then
  systemctl stop nginx || true
fi
if systemctl is-enabled --quiet nginx 2>/dev/null; then
  systemctl disable nginx || true
fi

# install the fuse layer needed to work around an incompatibility
# between Chrome and Tesla's recordings
g++ -o /root/cttseraser -D_FILE_OFFSET_BITS=64 "$SOURCE_DIR/fuse/cttseraser.cpp" -lstdc++ -lfuse

cat > /sbin/mount.ctts << EOF
#!/bin/bash -eu
/root/cttseraser "\$@" -o allow_other
EOF
chmod +x /sbin/mount.ctts

# Set up TeslaCam FUSE mount
mkdir -p /var/www/html/TeslaCam
sed -i '/mount.ctts/d' /etc/fstab
echo "mount.ctts#/mutable/TeslaCam /var/www/html/TeslaCam fuse defaults,nofail,x-systemd.requires=/mutable 0 0" >> /etc/fstab
mkdir -p /mutable/TeslaCam

sed -i 's/#user_allow_other/user_allow_other/' /etc/fuse.conf

if [ -e /backingfiles/music_disk.bin ] || [ -e /backingfiles/lightshow_disk.bin ] || [ -e /backingfiles/boombox_disk.bin ]
then
  mkdir -p /var/www/html/fs
  copy_script run/auto.www /root/bin
  echo "/var/www/html/fs  /root/bin/auto.www --timeout=0" > /etc/auto.master.d/www.autofs
  apt-get -y --force-yes install zip
fi

setup_progress "done configuring web"
