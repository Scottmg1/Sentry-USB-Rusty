#!/bin/bash
# Fix networking on SentryUSB installs that used the old read-only setup,
# where /var/lib/NetworkManager and related dirs were symlinked to /mutable.
# That caused WiFi/AP to fail after reboot when the USB drive wasn't ready.
# Run as root after: /root/bin/remountfs_rw
# Then run this script (e.g. via setup-sentryusb fix_networking). Reboot after.

set -e

function log_progress () {
  if declare -F setup_progress &> /dev/null; then
    setup_progress "fix-readonly-networking: $1"
  else
    echo "fix-readonly-networking: $1"
  fi
}

if [ "$(id -u)" -ne 0 ]; then
  echo "Run as root (e.g. sudo -i)"
  exit 1
fi

# ---- Check if the old broken state is present; skip if already fixed ----
_needs_fix=false
[ -L /var/lib/NetworkManager ] && _needs_fix=true
[ -L /etc/NetworkManager/system-connections ] && _needs_fix=true
[ -L /var/lib/dhcp ] || [ -L /var/lib/dhcpcd ] && _needs_fix=true
readlink -f /etc/resolv.conf 2>/dev/null | grep -q /mutable && _needs_fix=true
readlink -f /etc/resolv.conf 2>/dev/null | grep -q /run/systemd/resolve && _needs_fix=true
systemctl is-active --quiet systemd-resolved 2>/dev/null && _needs_fix=true
grep -w -q "/var/lib/NetworkManager" /etc/fstab || _needs_fix=true
grep -q "LABEL=mutable" /etc/fstab && ! grep "LABEL=mutable" /etc/fstab | grep -q "nofail" && _needs_fix=true
grep -q "LABEL=backingfiles" /etc/fstab && ! grep "LABEL=backingfiles" /etc/fstab | grep -q "nofail" && _needs_fix=true
[ ! -e /etc/tmpfiles.d/resolv-fallback.conf ] && _needs_fix=true
grep -w -q "/var/lib/systemd/rfkill" /etc/fstab || _needs_fix=true

if [ "$_needs_fix" = false ]; then
  log_progress "No fix needed: networking is already using tmpfs / root (not symlinks to /mutable)."
  exit 0
fi

log_progress "Applying networking fix for read-only root..."

# Ensure /mutable is mounted so we can copy from it if needed
if ! findmnt --mountpoint /mutable &> /dev/null; then
  if grep -q 'LABEL=mutable' /etc/fstab; then
    mount /mutable || log_progress "Warning: could not mount /mutable, will create empty dirs where needed"
  fi
fi

# ---- /var/lib/NetworkManager: must be a real dir so tmpfs can mount over it ----
if [ -L /var/lib/NetworkManager ]; then
  log_progress "Replacing /var/lib/NetworkManager symlink with directory"
  rm /var/lib/NetworkManager
  mkdir -p /var/lib/NetworkManager
fi

# ---- NM connection profiles: restore to root so they exist before /mutable mounts ----
if [ -L /etc/NetworkManager/system-connections ]; then
  log_progress "Restoring NetworkManager connection profiles to root FS"
  rm /etc/NetworkManager/system-connections
  if [ -d /mutable/etc/NetworkManager/system-connections ]; then
    cp -a /mutable/etc/NetworkManager/system-connections /etc/NetworkManager/
  else
    mkdir -p /etc/NetworkManager/system-connections
  fi
fi

# ---- DHCP lease dirs: real dirs for tmpfs ----
for d in /var/lib/dhcp /var/lib/dhcpcd; do
  if [ -L "$d" ]; then
    log_progress "Replacing $d symlink with directory"
    rm "$d"
    mkdir -p "$d"
  fi
done

# ---- resolv.conf: point to /tmp (always writable) ----
# Also redirect away from systemd-resolved's stub (/run/systemd/resolve/...)
# because we configure NM with dns=none below and use a dispatcher script
# to populate resolv.conf; systemd-resolved would conflict with that.
_resolv_target=$(readlink -f /etc/resolv.conf 2>/dev/null || true)
if [ "$_resolv_target" != "/tmp/resolv.conf" ]; then
  log_progress "Redirecting resolv.conf to /tmp (was: ${_resolv_target:-empty})"
  # Seed /tmp/resolv.conf with current DNS so resolution keeps working.
  # Try multiple sources: nmcli (NetworkManager), existing resolv.conf, fallback.
  > /tmp/resolv.conf
  if command -v nmcli &>/dev/null; then
    nmcli --terse --fields IP4.DNS dev show 2>/dev/null \
      | sed -n 's/^IP4\.DNS\[.*\]:/nameserver /p' \
      | head -3 >> /tmp/resolv.conf || true
  fi
  if ! grep -q '^nameserver' /tmp/resolv.conf 2>/dev/null; then
    [ -f "$_resolv_target" ] && grep '^nameserver' "$_resolv_target" >> /tmp/resolv.conf 2>/dev/null || true
  fi
  if ! grep -q '^nameserver' /tmp/resolv.conf 2>/dev/null; then
    echo "nameserver 1.1.1.1" >> /tmp/resolv.conf
  fi
  rm -f /etc/resolv.conf 2>/dev/null || true
  ln -sf /tmp/resolv.conf /etc/resolv.conf
fi

# ---- tmpfiles.d: seed /tmp/resolv.conf on every boot ----
# /tmp is a tmpfs that is empty after reboot, so without this rule the
# resolv.conf symlink dangles and DNS breaks.
# Seed with a public DNS fallback so basic resolution works during early boot;
# dhcpcd / NetworkManager will overwrite with DHCP-provided servers (e.g. PiHole)
# once a lease is obtained.
log_progress "Installing tmpfiles.d rule for resolv.conf"
mkdir -p /etc/tmpfiles.d
echo 'f /tmp/resolv.conf 0644 root root - nameserver 1.1.1.1' > /etc/tmpfiles.d/resolv-fallback.conf

# ---- DHCP client hooks to populate /tmp/resolv.conf ----
# On a read-only root, /etc/resolv.conf is a symlink to /tmp/resolv.conf.
# Install hooks for whichever DHCP client is present so DNS gets populated
# when a lease is obtained.  Multiple hooks can coexist harmlessly.

# -- NetworkManager: dns=none + dispatcher --
if command -v nmcli &>/dev/null; then
  log_progress "Configuring NetworkManager DNS handling (dns=none + dispatcher)"
  mkdir -p /etc/NetworkManager/conf.d
  cat > /etc/NetworkManager/conf.d/sentryusb-dns.conf << 'EOF'
[main]
dns=none
EOF

  mkdir -p /etc/NetworkManager/dispatcher.d
  cat > /etc/NetworkManager/dispatcher.d/50-write-resolv-conf << 'DISPATCHER'
#!/bin/bash
# Populate /tmp/resolv.conf with DHCP-provided DNS servers.
case "$2" in
  up|dhcp4-change)
    _servers="${DHCP4_DOMAIN_NAME_SERVERS:-${IP4_NAMESERVERS:-}}"
    if [ -n "$_servers" ]; then
      {
        for _ns in $_servers; do
          echo "nameserver $_ns"
        done
        _domain="${DHCP4_DOMAIN_NAME:-}"
        [ -n "$_domain" ] && echo "search $_domain"
      } > /tmp/resolv.conf
    fi
    ;;
esac
DISPATCHER
  chmod 0755 /etc/NetworkManager/dispatcher.d/50-write-resolv-conf
fi

# -- dhcpcd: hook to write DHCP-provided DNS --
# DietPi and Raspberry Pi OS Lite use dhcpcd instead of NetworkManager.
if command -v dhcpcd &>/dev/null; then
  log_progress "Installing dhcpcd hook for resolv.conf"
  mkdir -p /lib/dhcpcd/dhcpcd-hooks
  cat > /lib/dhcpcd/dhcpcd-hooks/90-sentryusb-resolv << 'DHCPHOOK'
# Write DHCP-provided DNS servers to /tmp/resolv.conf.
# /etc/resolv.conf is a symlink to /tmp/resolv.conf on SentryUSB.
if [ -n "${new_domain_name_servers:-}" ]; then
  {
    for ns in $new_domain_name_servers; do
      echo "nameserver $ns"
    done
    [ -n "${new_domain_name:-}" ] && echo "search $new_domain_name"
  } > /tmp/resolv.conf
fi
DHCPHOOK
  chmod 0644 /lib/dhcpcd/dhcpcd-hooks/90-sentryusb-resolv
fi

# -- ifupdown: hook for systems using /etc/network/interfaces + dhclient --
# dhclient normally writes /etc/resolv.conf directly (following the symlink).
# Install a hook as a safety net in case resolvconf intercepts that write.
if [ -d /etc/network ] && ! command -v nmcli &>/dev/null && ! command -v dhcpcd &>/dev/null; then
  log_progress "Installing ifupdown hook for resolv.conf"
  mkdir -p /etc/dhcp/dhclient-exit-hooks.d
  cat > /etc/dhcp/dhclient-exit-hooks.d/sentryusb-resolv << 'DHCLIENTHOOK'
# Write DHCP-provided DNS to /tmp/resolv.conf (SentryUSB read-only root).
if [ -n "${new_domain_name_servers:-}" ]; then
  {
    for ns in $new_domain_name_servers; do
      echo "nameserver $ns"
    done
    [ -n "${new_domain_name:-}" ] && echo "search $new_domain_name"
  } > /tmp/resolv.conf
fi
DHCLIENTHOOK
  chmod 0755 /etc/dhcp/dhclient-exit-hooks.d/sentryusb-resolv
fi

# ---- Disable systemd-resolved (conflicts with our resolv.conf management) ----
if systemctl is-active --quiet systemd-resolved 2>/dev/null; then
  log_progress "Disabling systemd-resolved (dispatcher handles DNS directly)"
  systemctl stop systemd-resolved 2>/dev/null || true
  systemctl disable systemd-resolved 2>/dev/null || true
fi

# ---- Unblock Bluetooth + install boot service ----
rfkill unblock bluetooth 2>/dev/null || true

# Install a systemd service that unblocks Bluetooth at every boot.
# On RPi the BT radio starts soft-blocked by default; on a read-only root
# the block is never cleared, breaking BLE (Tesla BLE key).
if [ ! -e /etc/systemd/system/rfkill-unblock-bluetooth.service ]; then
  log_progress "Installing Bluetooth rfkill-unblock boot service"
  cat > /etc/systemd/system/rfkill-unblock-bluetooth.service << 'BTUNIT'
[Unit]
Description=Unblock Bluetooth RF-kill
DefaultDependencies=no
Before=bluetooth.service hciuart.service
After=sysinit.target

[Service]
Type=oneshot
ExecStart=/usr/sbin/rfkill unblock bluetooth

[Install]
WantedBy=multi-user.target
BTUNIT
  systemctl enable rfkill-unblock-bluetooth.service 2>/dev/null || true
fi

# ---- Reload NM config (dns=none + dispatcher) without dropping WiFi ----
# A full restart would kill SSH sessions.  The reboot that follows will
# fully apply the new configuration.
if systemctl is-active --quiet NetworkManager 2>/dev/null; then
  log_progress "Reloading NetworkManager configuration"
  nmcli general reload 2>/dev/null || true
fi

# ---- fstab: tmpfs entries for networking + rfkill (idempotent) ----
for spec in \
  "/var/lib/NetworkManager:nodev,nosuid,mode=0700" \
  "/var/lib/dhcp:nodev,nosuid" \
  "/var/lib/dhcpcd:nodev,nosuid" \
  "/var/lib/systemd/rfkill:nodev,nosuid"
do
  _mountpoint="${spec%%:*}"
  _opts="${spec#*:}"
  if ! grep -w -q "$_mountpoint" /etc/fstab; then
    log_progress "Adding tmpfs fstab entry for $_mountpoint"
    mkdir -p "$_mountpoint"
    echo "tmpfs $_mountpoint tmpfs $_opts 0 0" >> /etc/fstab
  fi
done

# ---- fstab: add nofail to mutable and backingfiles so boot doesn't hang if USB is missing ----
for label in mutable backingfiles; do
  if grep -q "LABEL=$label" /etc/fstab && ! grep "LABEL=$label" /etc/fstab | grep -q "nofail"; then
    log_progress "Adding nofail to LABEL=$label in fstab"
    sed -i "/LABEL=$label/ s/auto,rw/auto,rw,nofail/" /etc/fstab
    sed -i "/LABEL=$label/ s/auto,rw,noatime/auto,rw,noatime,nofail/" /etc/fstab
  fi
done

touch -t 197001010000 /etc/fstab 2>/dev/null || true

log_progress "Done. Reboot for changes to take effect."
exit 0
