#!/bin/bash

function log_progress () {
  if declare -F setup_progress > /dev/null
  then
    setup_progress "configure-rtc: $1"
    return
  fi
  echo "configure-rtc: $1"
}

# Write system time → RTC via /dev/rtc0 ioctl (hwclock is not available on minimal images,
# and /sys/class/rtc/rtc0/since_epoch is read-only on rpi-rtc)
rtc_sync_systohc() {
  if [ ! -e /dev/rtc0 ]; then
    log_progress "Warning: /dev/rtc0 not found"
    return 1
  fi
  python3 -c "
import fcntl, struct, time
t = time.gmtime()
# struct rtc_time: sec, min, hour, mday, mon(0-based), year-1900, wday, yday, isdst
data = struct.pack('9i', t.tm_sec, t.tm_min, t.tm_hour, t.tm_mday, t.tm_mon-1, t.tm_year-1900, t.tm_wday, t.tm_yday, -1)
with open('/dev/rtc0', 'wb') as f:
    fcntl.ioctl(f.fileno(), 0x4024700a, data)  # RTC_SET_TIME
" 2>/dev/null && log_progress "Synced system time to RTC" || {
    log_progress "Warning: failed to sync time to RTC"
    return 1
  }
}

# Read RTC → set system time via /dev/rtc0 ioctl
rtc_sync_hctosys() {
  if [ ! -e /dev/rtc0 ]; then
    return 1
  fi
  local epoch
  epoch=$(python3 -c "
import fcntl, struct, calendar, time
with open('/dev/rtc0', 'rb') as f:
    data = fcntl.ioctl(f.fileno(), 0x80247009, b'\x00' * 36)  # RTC_RD_TIME
vals = struct.unpack('9i', data)
t = time.struct_time((vals[5]+1900, vals[4]+1, vals[3], vals[2], vals[1], vals[0], vals[6], vals[7], vals[8]))
print(int(calendar.timegm(t)))
" 2>/dev/null)
  if [ -n "$epoch" ] && [ "$epoch" -gt 1704067200 ]; then
    # Only set if RTC has a sane date (after 2024-01-01)
    date -u -s "@$epoch" > /dev/null
  fi
}

RTC_BATTERY_ENABLED=${RTC_BATTERY_ENABLED:-false}
RTC_TRICKLE_CHARGE=${RTC_TRICKLE_CHARGE:-false}

# Only relevant on Pi 5
if ! grep -qi "Raspberry Pi 5" /proc/device-tree/model 2>/dev/null; then
  log_progress "Not a Pi 5, skipping RTC configuration"
  exit 0
fi

if [ "$RTC_BATTERY_ENABLED" = "true" ]; then
  log_progress "Enabling RTC battery support"

  # Disable fake-hwclock
  if systemctl is-enabled fake-hwclock.service 2>/dev/null | grep -q enabled; then
    log_progress "Disabling fake-hwclock"
    systemctl stop fake-hwclock.service || true
    systemctl disable fake-hwclock.service || true
  fi

  # Create RTC sync service using /dev/rtc0 ioctl
  # This service ONLY handles boot-time RTC→system sync.
  # Periodic system→RTC sync is handled by archiveloop's timesyncloop,
  # because the Pi never gracefully shuts down (car just loses power).
  log_progress "Creating sentryusb-hwclock.service"
  cat > /lib/systemd/system/sentryusb-hwclock.service << 'UNIT'
[Unit]
Description=SentryUSB hardware clock sync
DefaultDependencies=no
After=dev-rtc0.device
Before=time-sync.target sysinit.target

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/bin/bash -c '\
  epoch=$(python3 -c "\
import fcntl, struct, calendar, time;\
f=open(\"/dev/rtc0\",\"rb\");\
d=fcntl.ioctl(f.fileno(),0x80247009,b\"\\x00\"*36);\
f.close();\
v=struct.unpack(\"9i\",d);\
t=time.struct_time((v[5]+1900,v[4]+1,v[3],v[2],v[1],v[0],v[6],v[7],v[8]));\
print(int(calendar.timegm(t)))\
" 2>/dev/null);\
  if [ -n "$epoch" ] && [ "$epoch" -gt 1704067200 ]; then\
    date -u -s "@$epoch" > /dev/null;\
  fi'

[Install]
WantedBy=sysinit.target
UNIT

  systemctl daemon-reload
  systemctl enable sentryusb-hwclock.service

  # Sync current system time to RTC
  rtc_sync_systohc

  # Trickle charging for rechargeable batteries (ML-2020, ML-2032, LIR2032)
  if [ "$RTC_TRICKLE_CHARGE" = "true" ]; then
    log_progress "Enabling RTC trickle charging (3.0V)"
    if ! grep -q "dtparam=rtc_bbat_vchg" /boot/firmware/config.txt 2>/dev/null; then
      echo "dtparam=rtc_bbat_vchg=3000000" >> /boot/firmware/config.txt
    else
      sed -i 's/^#*dtparam=rtc_bbat_vchg.*/dtparam=rtc_bbat_vchg=3000000/' /boot/firmware/config.txt
    fi
  else
    if grep -q "^dtparam=rtc_bbat_vchg" /boot/firmware/config.txt 2>/dev/null; then
      log_progress "Disabling RTC trickle charging"
      sed -i '/^dtparam=rtc_bbat_vchg/d' /boot/firmware/config.txt
    fi
  fi

  log_progress "RTC battery support enabled"
else
  log_progress "RTC battery support disabled, ensuring fake-hwclock is active"

  # Remove hwclock service if it exists
  if [ -e /lib/systemd/system/sentryusb-hwclock.service ]; then
    systemctl stop sentryusb-hwclock.service 2>/dev/null || true
    systemctl disable sentryusb-hwclock.service 2>/dev/null || true
    rm -f /lib/systemd/system/sentryusb-hwclock.service
    systemctl daemon-reload
  fi

  # Remove trickle charging if it was enabled
  if grep -q "^dtparam=rtc_bbat_vchg" /boot/firmware/config.txt 2>/dev/null; then
    log_progress "Removing RTC trickle charging"
    sed -i '/^dtparam=rtc_bbat_vchg/d' /boot/firmware/config.txt
  fi

  # Re-enable fake-hwclock
  if systemctl is-enabled fake-hwclock.service 2>/dev/null | grep -q disabled; then
    log_progress "Re-enabling fake-hwclock"
    systemctl enable fake-hwclock.service || true
  fi

  log_progress "fake-hwclock restored"
fi
