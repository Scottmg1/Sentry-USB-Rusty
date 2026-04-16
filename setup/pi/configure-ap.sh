#!/bin/bash -eu

# based on https://blog.thewalr.us/2017/09/26/raspberry-pi-zero-w-simultaneous-ap-and-managed-mode-wifi/

function log_progress () {
  if declare -F setup_progress > /dev/null
  then
    setup_progress "configure-ap: $1"
  else
    echo "configure-ap: $1"
  fi
}

if [ -z "${AP_SSID+x}" ]
then
  log_progress "AP_SSID not set"
  exit 1
fi

if [ -z "${AP_PASS+x}" ] || [ "$AP_PASS" = "password" ] || (( ${#AP_PASS} < 8))
then
  log_progress "AP_PASS not set, not changed from default, or too short"
  exit 1
fi

function nm_get_wifi_client_device () {
  for i in {1..5}
  do
    WLAN="$(nmcli -t -f TYPE,DEVICE c show --active | grep 802-11-wireless | grep -v ":ap0$" | cut -c 17-)"
    if [ -n "$WLAN" ]
    then
      break;
    fi
    log_progress "Waiting for wifi interface to come back up"
    sleep 5
  done

  [ -n "$WLAN" ] && return 0

  log_progress "Couldn't determine wifi client device"
  nmcli c show
  return 1
}

function nm_add_ap () {
  nm_get_wifi_client_device || return 1

  if ! iw dev ap0 info &> /dev/null
  then
    # create additional virtual interface for the wifi device
    iw dev "$WLAN" interface add ap0 type __ap || return 1
  fi

  # turn off power savings for both interfaces since they use
  # the same underlying hardware, and we don't want one to go
  # into power save mode just because the other is idle
  iw "$WLAN" set power_save off || return 1
  iw ap0 set power_save off || return 1

  # set up access point on the virtual interface using networkmanager
  nmcli con delete SENTRYUSB_AP &> /dev/null || true
  nmcli con delete TESLAUSB_AP &> /dev/null || true
  nmcli con add type wifi ifname ap0 mode ap con-name SENTRYUSB_AP ssid "$AP_SSID" || return 1
  # don't set band and channel, because that is controlled by the $WLAN interface
  #nmcli con modify SENTRYUSB_AP 802-11-wireless.band bg
  #nmcli con modify SENTRYUSB_AP 802-11-wireless.channel 6
  nmcli con modify SENTRYUSB_AP 802-11-wireless-security.key-mgmt wpa-psk || return 1
  nmcli con modify SENTRYUSB_AP 802-11-wireless-security.psk "$AP_PASS" || return 1
  IP=${AP_IP:-"192.168.66.1"}
  nmcli con modify SENTRYUSB_AP ipv4.addr "$IP/24" || return 1
  nmcli con modify SENTRYUSB_AP ipv4.method shared || return 1
  nmcli con modify SENTRYUSB_AP ipv6.method disabled || return 1
  # Don't auto-start the AP — Away Mode controls when it comes up
  nmcli con modify SENTRYUSB_AP connection.autoconnect no || return 1
  # Remove stale if-up.d script from previous installs — it doesn't fire on
  # NetworkManager/netplan systems (e.g. Pi 5 with Debian Trixie).
  rm -f /etc/network/if-up.d/sentryusb-ap

  # Use a NetworkManager dispatcher script instead: NM calls scripts in
  # /etc/NetworkManager/dispatcher.d/ with $1=interface $2=action whenever
  # an interface changes state.
  # Use a NetworkManager dispatcher script that only recreates ap0 when
  # Away Mode is active (flag file exists).  During normal operation the AP
  # stays off so wlan0 can freely scan all channels.
  mkdir -p /etc/NetworkManager/dispatcher.d
  cat > /etc/NetworkManager/dispatcher.d/10-sentryusb-ap << EOF
#!/bin/bash
# Recreate ap0 virtual interface when the wifi client comes up,
# but ONLY if Away Mode is active (flag file exists).
# Created by SentryUSB configure-ap.sh

IFACE="\$1"
ACTION="\$2"

if [ "\$IFACE" = "$WLAN" ] && [ "\$ACTION" = "up" ]
then
  if [ -f /mutable/sentryusb_away_mode.json ]; then
    if ! iw dev ap0 info &> /dev/null; then
      iw dev $WLAN interface add ap0 type __ap || true
    fi
    iw $WLAN set power_save off 2>/dev/null || true
    iw ap0 set power_save off 2>/dev/null || true
    nmcli con up SENTRYUSB_AP 2>/dev/null || true
  fi
fi
EOF
  chmod 755 /etc/NetworkManager/dispatcher.d/10-sentryusb-ap || return 1
}


function nm_write_ap_file () {
  # Write the AP connection profile directly to disk, bypassing NM's
  # keyfile plugin.  This works even when NM was started on a read-only
  # root (the plugin refuses "nmcli con add", but the filesystem is
  # writable after remountfs_rw).  Uses "nmcli con reload" instead of a
  # full NM restart, so WiFi stays up and SSH sessions survive.
  log_progress "Writing AP connection file directly (NM keyfile workaround)"
  local _ssid="$AP_SSID"
  local _psk="$AP_PASS"
  local _ip="${AP_IP:-192.168.66.1}"
  local _file="/etc/NetworkManager/system-connections/SENTRYUSB_AP.nmconnection"

  nm_get_wifi_client_device || return 1

  if ! iw dev ap0 info &> /dev/null; then
    iw dev "$WLAN" interface add ap0 type __ap || return 1
  fi
  iw "$WLAN" set power_save off || return 1
  iw ap0 set power_save off || return 1

  nmcli con delete SENTRYUSB_AP &> /dev/null || true
  nmcli con delete TESLAUSB_AP &> /dev/null || true

  mkdir -p /etc/NetworkManager/system-connections
  cat > "$_file" << EOF
[connection]
id=SENTRYUSB_AP
type=wifi
interface-name=ap0
autoconnect=false

[wifi]
mode=ap
ssid=$_ssid

[wifi-security]
key-mgmt=wpa-psk
psk=$_psk

[ipv4]
address1=$_ip/24
method=shared

[ipv6]
method=disabled
EOF
  chmod 0600 "$_file"

  # Tell NM to pick up the new file without a full restart
  nmcli con reload 2>/dev/null || true

  # Install the dispatcher script for ap0 — only recreates ap0 when Away
  # Mode is active (flag file exists).
  rm -f /etc/network/if-up.d/sentryusb-ap
  mkdir -p /etc/NetworkManager/dispatcher.d
  cat > /etc/NetworkManager/dispatcher.d/10-sentryusb-ap << EOF2
#!/bin/bash
# Recreate ap0 virtual interface when the wifi client comes up,
# but ONLY if Away Mode is active (flag file exists).
# Created by SentryUSB configure-ap.sh

IFACE="\$1"
ACTION="\$2"

if [ "\$IFACE" = "$WLAN" ] && [ "\$ACTION" = "up" ]
then
  if [ -f /mutable/sentryusb_away_mode.json ]; then
    if ! iw dev ap0 info &> /dev/null; then
      iw dev $WLAN interface add ap0 type __ap || true
    fi
    iw $WLAN set power_save off 2>/dev/null || true
    iw ap0 set power_save off 2>/dev/null || true
    nmcli con up SENTRYUSB_AP 2>/dev/null || true
  fi
fi
EOF2
  chmod 755 /etc/NetworkManager/dispatcher.d/10-sentryusb-ap || return 1
}

if systemctl --quiet is-enabled NetworkManager.service
then
  # force-install iw because otherwise it will get autoremoved when
  # alsa-utils is removed later
  apt-get -y --force-yes install iw || exit 1
  if ! nm_add_ap
  then
    # NM won't allow adding connections when its keyfile plugin started
    # on a read-only root fs.  Instead of a full NM restart (which drops
    # WiFi and kills SSH sessions), write the connection file directly
    # and reload.
    if ! nm_write_ap_file
    then
      log_progress "STOP: Failed to configure AP"
      exit 1
    fi
  fi
  log_progress "AP configured"
  exit 0
fi


if [ ! -e /etc/wpa_supplicant/wpa_supplicant.conf ]
then
  log_progress "No wpa_supplicant, skipping AP setup."
  exit 0
fi

if ! grep -q id_str /etc/wpa_supplicant/wpa_supplicant.conf
then
  IP=${AP_IP:-"192.168.66.1"}
  NET=$(echo -n "$IP" | sed -e 's/\.[0-9]\{1,3\}$//')

  # install required packages
  log_progress "installing dnsmasq and hostapd"
  apt-get -y --force-yes install dnsmasq hostapd

  log_progress "configuring AP '$AP_SSID' with IP $IP"
  # create udev rule
  MAC="$(cat /sys/class/net/wlan0/address)"
  cat <<- EOF > /etc/udev/rules.d/70-persistent-net.rules
	SUBSYSTEM=="ieee80211", ACTION=="add|change", ATTR{macaddress}=="$MAC", KERNEL=="phy0", \
	RUN+="/sbin/iw phy phy0 interface add ap0 type __ap", \
	RUN+="/bin/ip link set ap0 address $MAC"
	EOF

  # configure dnsmasq
  cat <<- EOF > /etc/dnsmasq.conf
	interface=lo,ap0
	no-dhcp-interface=lo,wlan0
	bind-interfaces
	bogus-priv
	dhcp-range=${NET}.10,${NET}.254,12h
	# don't configure a default route, we're not a router
	dhcp-option=3
	EOF

  # configure hostapd
  cat <<- EOF > /etc/hostapd/hostapd.conf
	ctrl_interface=/var/run/hostapd
	ctrl_interface_group=0
	interface=ap0
	driver=nl80211
	ssid=${AP_SSID}
	hw_mode=g
	channel=11
	wmm_enabled=0
	macaddr_acl=0
	auth_algs=1
	wpa=2
	wpa_passphrase=${AP_PASS}
	wpa_key_mgmt=WPA-PSK
	wpa_pairwise=TKIP CCMP
	rsn_pairwise=CCMP
	EOF
  cat <<- EOF > /etc/default/hostapd
	DAEMON_CONF="/etc/hostapd/hostapd.conf"
	EOF

  # define network interfaces. Note use of 'AP1' name, defined in wpa_supplication.conf below
  cat <<- EOF > /etc/network/interfaces
	source-directory /etc/network/interfaces.d

	auto lo
	auto ap0
	auto wlan0
	iface lo inet loopback

	allow-hotplug ap0
	iface ap0 inet static
	    address ${IP}
	    netmask 255.255.255.0
	    hostapd /etc/hostapd/hostapd.conf

	allow-hotplug wlan0
	iface wlan0 inet manual
	    wpa-roam /etc/wpa_supplicant/wpa_supplicant.conf
	iface AP1 inet dhcp
	EOF

  # For bullseye it is apparently necessary to explicitly disable wpa_supplicant for the ap0 interface
  cat <<- EOF >> /etc/dhcpcd.conf
	# disable wpa_supplicant for the ap0 interface
	interface ap0
	nohook wpa_supplicant
	EOF

  if [ ! -L /var/lib/misc ]
  then
    if ! findmnt --mountpoint /mutable
    then
        mount /mutable
    fi
    mkdir -p /mutable/varlib
    mv /var/lib/misc /mutable/varlib
    ln -s /mutable/varlib/misc /var/lib/misc
  fi

  # update the host name to have the AP IP address, otherwise
  # clients connected to the IP will get 127.0.0.1 when looking
  # up the sentryusb host name
  sed -i -e "/^127.0.0.1\s*localhost/b; s/^127.0.0.1\(\s*.*\)/$IP\1/" /etc/hosts

  # add ID string to wpa_supplicant
  sed -i -e 's/}/  id_str="AP1"\n}/'  /etc/wpa_supplicant/wpa_supplicant.conf
else
  log_progress "AP mode already configured"
fi
