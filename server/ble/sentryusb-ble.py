#!/usr/bin/env python3
"""
SentryUSB BLE Peripheral Daemon

Exposes a GATT server over Bluetooth LE so the SentryUSB iOS app can:
  1. Discover the Pi and perform WiFi setup without prior network
  2. Proxy API requests for on-the-go management (dashboard, logs, settings)

Uses BlueZ D-Bus API — requires bluez >= 5.50 and python3-dbus.

Run as: python3 sentryusb-ble.py
Or via systemd: sentryusb-ble.service
"""

import dbus
import dbus.exceptions
import dbus.mainloop.glib
import dbus.service
import json
import subprocess
import os
import sys
import signal
import logging
import urllib.request
import urllib.error
import threading

try:
    from gi.repository import GLib
except ImportError:
    import glib as GLib

logging.basicConfig(level=logging.INFO, format='[BLE] %(levelname)s %(message)s')
log = logging.getLogger('sentryusb-ble')

# ============================================================
# D-Bus policy self-healing
# ============================================================

_DBUS_POLICY_PATH = '/etc/dbus-1/system.d/com.sentryusb.ble.conf'
_DBUS_POLICY_XML = """\
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <!-- Allow the SentryUSB BLE daemon (running as root) to own its bus name
       and expose GATT objects so BlueZ can call GetManagedObjects on them.
       Required on Pi 5 / Bookworm where D-Bus policies are stricter. -->

  <policy user="root">
    <allow own="com.sentryusb.ble"/>
    <allow send_destination="com.sentryusb.ble"/>
    <allow send_interface="org.freedesktop.DBus.ObjectManager"/>
    <allow send_interface="org.freedesktop.DBus.Properties"/>
    <allow send_interface="org.bluez.GattService1"/>
    <allow send_interface="org.bluez.GattCharacteristic1"/>
    <allow send_interface="org.bluez.GattDescriptor1"/>
    <allow send_interface="org.bluez.LEAdvertisement1"/>
  </policy>

  <policy context="default">
    <allow send_destination="com.sentryusb.ble"/>
  </policy>
</busconfig>
"""


def maybe_install_dbus_policy():
    """Auto-install the D-Bus policy file if it is missing, then re-exec.

    Without this file, strict D-Bus systems (Pi 5 / Bookworm) block the daemon
    from owning 'com.sentryusb.ble', so BlueZ falls back to PipeWire's GATT
    services (audio volume/stream UUIDs) — causing iOS to cache the wrong GATT
    and fail BLE pairing indefinitely until the iOS Bluetooth cache is cleared.

    Called before the D-Bus bus is connected so the re-exec takes full effect.
    """
    if os.path.exists(_DBUS_POLICY_PATH):
        return
    log.warning('D-Bus policy missing — auto-installing and restarting daemon')
    try:
        os.makedirs('/etc/dbus-1/system.d', exist_ok=True)
        with open(_DBUS_POLICY_PATH, 'w') as f:
            f.write(_DBUS_POLICY_XML)
        subprocess.run(['systemctl', 'reload', 'dbus'], timeout=10, check=False)
        import time; time.sleep(1)  # give dbus-daemon a moment to re-read config
        log.info('D-Bus policy installed — re-execing to claim com.sentryusb.ble')
        os.execv(sys.executable, [sys.executable] + sys.argv)
    except Exception as e:
        log.error(f'Could not auto-install D-Bus policy: {e} — GATT may not be served correctly')

# BlueZ D-Bus constants
BLUEZ_SERVICE = 'org.bluez'
LE_ADVERTISING_MANAGER_IFACE = 'org.bluez.LEAdvertisingManager1'
LE_ADVERTISEMENT_IFACE = 'org.bluez.LEAdvertisement1'
GATT_MANAGER_IFACE = 'org.bluez.GattManager1'
GATT_SERVICE_IFACE = 'org.bluez.GattService1'
GATT_CHRC_IFACE = 'org.bluez.GattCharacteristic1'
GATT_DESC_IFACE = 'org.bluez.GattDescriptor1'
DBUS_OM_IFACE = 'org.freedesktop.DBus.ObjectManager'
DBUS_PROP_IFACE = 'org.freedesktop.DBus.Properties'

# SentryUSB BLE UUIDs (matching iOS app Constants.swift)
WIFI_SERVICE_UUID        = '6e400001-b5a3-f393-e0a9-e50e24dcca9e'
WIFI_SCAN_UUID           = '6e400002-b5a3-f393-e0a9-e50e24dcca9e'
WIFI_CONFIG_UUID         = '6e400003-b5a3-f393-e0a9-e50e24dcca9e'
WIFI_STATUS_UUID         = '6e400004-b5a3-f393-e0a9-e50e24dcca9e'
DEVICE_INFO_UUID         = '6e400005-b5a3-f393-e0a9-e50e24dcca9e'

AUTH_UUID                = '6e400006-b5a3-f393-e0a9-e50e24dcca9e'

API_SERVICE_UUID         = '6e400010-b5a3-f393-e0a9-e50e24dcca9e'
API_REQUEST_UUID         = '6e400011-b5a3-f393-e0a9-e50e24dcca9e'
API_RESPONSE_UUID        = '6e400012-b5a3-f393-e0a9-e50e24dcca9e'

# Auto-detected at startup — production uses port 80, dev uses 8788
API_BASE = None

PIN_FILE = '/root/.sentryusb/ble-pin'
BOOT_PIN_FILE = '/boot/firmware/BLE_PIN'

# Track authenticated BLE peers (by D-Bus device path)
authenticated_peers = set()

mainloop = None


def detect_api_base():
    """Detect which port the Go server is listening on.
    Production runs on port 80, dev on 8788."""
    for port in (80, 8788):
        try:
            url = f'http://127.0.0.1:{port}/api/system/version'
            resp = urllib.request.urlopen(url, timeout=3)
            resp.read()
            base = f'http://127.0.0.1:{port}/api'
            log.info(f'API server detected on port {port}: {base}')
            return base
        except Exception:
            continue
    # Default to port 80 (production) even if not yet reachable
    log.warning('API server not yet reachable on port 80 or 8788, defaulting to port 80')
    return 'http://127.0.0.1:80/api'


def load_pin():
    """Load the stored BLE passcode, or None if not yet claimed."""
    try:
        with open(PIN_FILE, 'r') as f:
            return f.read().strip()
    except FileNotFoundError:
        return None


def save_pin(pin):
    """Save a new BLE passcode."""
    # Root filesystem is read-only at runtime — remount rw before writing.
    subprocess.run(['/root/bin/remountfs_rw'], capture_output=True, timeout=5)
    os.makedirs(os.path.dirname(PIN_FILE), exist_ok=True)
    with open(PIN_FILE, 'w') as f:
        f.write(pin)
    # Also write to boot partition for easy reset
    try:
        with open(BOOT_PIN_FILE, 'w') as f:
            f.write(pin)
    except Exception:
        pass
    log.info(f'BLE passcode set (length={len(pin)})')


def is_claimed():
    """Check if a passcode has been set (device is claimed)."""
    return load_pin() is not None


def check_pin(pin):
    """Verify a passcode against the stored one."""
    stored = load_pin()
    return stored is not None and stored == pin


def is_authenticated(options):
    """Check if the connected peer is authenticated."""
    device = options.get('device', '')
    if not device:
        # If we can't determine the device, check if unclaimed (allow all)
        return not is_claimed()
    return str(device) in authenticated_peers


def mark_authenticated(options):
    """Mark the connected peer as authenticated."""
    device = options.get('device', '')
    if device:
        authenticated_peers.add(str(device))
        log.info(f'Peer authenticated: {device}')


# ============================================================
# Helper: get hostname, version, and unique device suffix
# ============================================================

def get_hostname():
    try:
        return subprocess.check_output(['hostname'], text=True).strip()
    except Exception:
        return 'sentryusb'

def get_device_suffix():
    """Return a stable 4-character uppercase hex suffix unique to this Pi,
    derived from /etc/machine-id. Used for display names like SentryUSB-A3F1."""
    try:
        with open('/etc/machine-id', 'r') as f:
            machine_id = f.read().strip()
        # Use last 4 hex chars — unique enough for a handful of Pis
        return machine_id[-4:].upper()
    except Exception:
        # Fallback: derive from Bluetooth adapter MAC
        try:
            mac = subprocess.check_output(
                ['hciconfig', 'hci0'], text=True)
            for line in mac.splitlines():
                if 'BD Address' in line:
                    addr = line.split('BD Address:')[1].split()[0]
                    return addr.replace(':', '')[-4:].upper()
        except Exception:
            pass
    return '0000'

def get_version():
    try:
        resp = urllib.request.urlopen(f'{API_BASE}/system/version', timeout=3)
        data = json.loads(resp.read())
        return data.get('version', 'unknown')
    except Exception:
        return 'unknown'

def is_setup_finished():
    paths = [
        '/sentryusb/SENTRYUSB_SETUP_FINISHED',
        '/boot/firmware/SENTRYUSB_SETUP_FINISHED',
        '/boot/SENTRYUSB_SETUP_FINISHED',
    ]
    return any(os.path.exists(p) for p in paths)

AVAHI_SERVICE_PATH = '/etc/avahi/services/sentryusb.service'

def update_avahi_service_name(name):
    """Rewrite the Avahi mDNS service file to include the device suffix as a
    TXT record. The service name stays as %h (hostname) so .local resolution
    keeps working. The iOS app reads the 'suffix' TXT record to build a
    display name like 'SentryUSB-EC92'."""
    suffix = get_device_suffix()
    service_xml = f'''<?xml version="1.0" standalone='no'?>
<!DOCTYPE service-group SYSTEM "avahi-service.dtd">
<service-group>
  <name replace-wildcards="yes">%h</name>
  <service>
    <type>_sentryusb._tcp</type>
    <port>80</port>
    <txt-record>version=1.0.0</txt-record>
    <txt-record>path=/api</txt-record>
    <txt-record>suffix={suffix}</txt-record>
  </service>
</service-group>
'''
    try:
        # Only rewrite if the suffix record is missing or different
        if os.path.exists(AVAHI_SERVICE_PATH):
            with open(AVAHI_SERVICE_PATH, 'r') as f:
                current = f.read()
            if f'suffix={suffix}' in current:
                log.info(f'Avahi service already has suffix: {suffix}')
                return
        with open(AVAHI_SERVICE_PATH, 'w') as f:
            f.write(service_xml)
        # Restart avahi to pick up the change
        subprocess.run(['systemctl', 'restart', 'avahi-daemon'],
                       capture_output=True, timeout=10)
        log.info(f'Avahi mDNS service updated with suffix={suffix}')
    except Exception as e:
        log.warning(f'Failed to update Avahi service: {e}')


# ============================================================
# WiFi scanning and configuration
# ============================================================

def scan_wifi_networks():
    """Scan for visible WiFi networks using nmcli."""
    try:
        # Ensure WiFi radio is unblocked
        subprocess.run(['rfkill', 'unblock', 'wifi'],
                       capture_output=True, timeout=3)
        # Trigger a fresh scan
        subprocess.run(['nmcli', 'device', 'wifi', 'rescan'],
                       capture_output=True, timeout=10)
        # Wait for the scan to complete before listing results —
        # nmcli rescan returns immediately but the actual scan takes a few seconds.
        # Without this delay, wifi list returns stale/empty cached results.
        import time
        time.sleep(3)
        output = subprocess.check_output(
            ['nmcli', '-t', '-f', 'SSID,SIGNAL,SECURITY', 'device', 'wifi', 'list'],
            text=True, timeout=10, stderr=subprocess.DEVNULL
        )
        seen = {}
        for line in output.strip().split('\n'):
            if not line.strip():
                continue
            parts = line.split(':')
            if len(parts) < 3:
                continue
            ssid = parts[0].strip()
            if not ssid:
                continue
            try:
                signal = int(parts[1])
            except (ValueError, IndexError):
                signal = 0
            security = parts[2].strip() if len(parts) > 2 else ''
            encrypted = security != '' and security != '--'
            # Keep strongest signal per SSID
            if ssid not in seen or signal > seen[ssid].get('signal', 0):
                seen[ssid] = {'ssid': ssid, 'signal': signal, 'encrypted': encrypted}
        return list(seen.values())
    except Exception as e:
        log.error(f'WiFi scan failed: {e}')
        return []

def configure_wifi(ssid, password, hostname=None):
    """Configure WiFi via NetworkManager and optionally set hostname."""
    result = {'connected': False, 'ip': '', 'error': ''}
    try:
        # Set hostname if provided (needs rw filesystem)
        if hostname:
            subprocess.run(['/root/bin/remountfs_rw'], capture_output=True, timeout=5)
            subprocess.run(['hostnamectl', 'set-hostname', hostname], capture_output=True, timeout=5)

        # Delete any stale connection profile for this SSID — avoids
        # "802-11-wireless-security.key-mgmt: property is missing" errors
        # when a previous connection attempt left a broken profile.
        subprocess.run(['nmcli', 'connection', 'delete', ssid],
                       capture_output=True, timeout=5)

        # Connect via NetworkManager
        cmd = ['nmcli', 'device', 'wifi', 'connect', ssid]
        if password:
            cmd += ['password', password]
        connect_result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=30
        )
        if connect_result.returncode != 0:
            err_msg = connect_result.stderr.strip() or connect_result.stdout.strip()
            result['error'] = err_msg or 'nmcli connect failed'
            log.error(f'WiFi connect failed: {err_msg}')
            return result

        # Wait for IP address (up to 15 seconds)
        import time
        for _ in range(15):
            time.sleep(1)
            try:
                ip_output = subprocess.check_output(
                    ['ip', '-4', 'addr', 'show', 'wlan0'], text=True, timeout=3
                )
                for line in ip_output.split('\n'):
                    if 'inet ' in line:
                        ip = line.strip().split(' ')[1].split('/')[0]
                        result['connected'] = True
                        result['ip'] = ip
                        log.info(f'WiFi connected: {ssid} -> {ip}')
                        return result
            except Exception:
                pass

        result['error'] = 'Connection timed out'
    except Exception as e:
        result['error'] = str(e)
        log.error(f'WiFi configure failed: {e}')
    return result


# ============================================================
# API proxy: forward requests to the local Go server
# ============================================================

def proxy_api_request(method, path, body=None, retries=2, retry_delay=1.5):
    """Forward an API request to the local Go server.
    Retries on connection errors (e.g. server still starting up)."""
    import time
    url = f'{API_BASE}{path}'
    last_error = None
    for attempt in range(1 + retries):
        try:
            data = json.dumps(body).encode() if body else None
            req = urllib.request.Request(url, data=data, method=method)
            req.add_header('Content-Type', 'application/json')
            resp = urllib.request.urlopen(req, timeout=15)
            response_body = resp.read()
            try:
                parsed = json.loads(response_body)
            except (json.JSONDecodeError, ValueError):
                parsed = response_body.decode('utf-8', errors='replace')
            return {'status': resp.getcode(), 'body': parsed}
        except urllib.error.HTTPError as e:
            body_text = e.read().decode() if e.fp else ''
            return {'status': e.code, 'body': body_text}
        except (urllib.error.URLError, ConnectionRefusedError, OSError) as e:
            last_error = e
            if attempt < retries:
                log.warning(f'API proxy: {method} {path} attempt {attempt+1} failed ({e}), retrying in {retry_delay}s...')
                time.sleep(retry_delay)
            else:
                log.error(f'API proxy: {method} {path} failed after {retries+1} attempts: {e}')
        except Exception as e:
            return {'status': 500, 'body': {'error': str(e)}}
    return {'status': 503, 'body': {'error': f'Local server unavailable: {last_error}'}}


# ============================================================
# D-Bus / BlueZ GATT Application
# ============================================================

class InvalidArgsException(dbus.exceptions.DBusException):
    _dbus_error_name = 'org.freedesktop.DBus.Error.InvalidArgs'

class NotSupportedException(dbus.exceptions.DBusException):
    _dbus_error_name = 'org.bluez.Error.NotSupported'

class NotPermittedException(dbus.exceptions.DBusException):
    _dbus_error_name = 'org.bluez.Error.NotPermitted'


class Application(dbus.service.Object):
    """BlueZ GATT Application."""

    def __init__(self, bus):
        self.path = '/'
        self.services = []
        dbus.service.Object.__init__(self, bus, self.path)
        self.add_service(WifiSetupService(bus, 0))
        self.add_service(APIProxyService(bus, 1))

    def get_path(self):
        return dbus.ObjectPath(self.path)

    def add_service(self, service):
        self.services.append(service)

    @dbus.service.method(DBUS_OM_IFACE, out_signature='a{oa{sa{sv}}}')
    def GetManagedObjects(self):
        response = {}
        for service in self.services:
            response[service.get_path()] = service.get_properties()
            chrcs = service.get_characteristics()
            for chrc in chrcs:
                response[chrc.get_path()] = chrc.get_properties()
                descs = chrc.get_descriptors()
                for desc in descs:
                    response[desc.get_path()] = desc.get_properties()
        log.info(f'GetManagedObjects called — returning {len(response)} objects: {list(response.keys())}')
        return response


class Service(dbus.service.Object):
    PATH_BASE = '/org/bluez/sentryusb/service'

    def __init__(self, bus, index, uuid, primary):
        self.path = self.PATH_BASE + str(index)
        self.bus = bus
        self.uuid = uuid
        self.primary = primary
        self.characteristics = []
        dbus.service.Object.__init__(self, bus, self.path)

    def get_properties(self):
        return {
            GATT_SERVICE_IFACE: {
                'UUID': dbus.String(self.uuid),
                'Primary': dbus.Boolean(self.primary),
                'Characteristics': dbus.Array(
                    self.get_characteristic_paths(), signature='o')
            }
        }

    def get_path(self):
        return dbus.ObjectPath(self.path)

    def add_characteristic(self, characteristic):
        self.characteristics.append(characteristic)

    def get_characteristic_paths(self):
        return [chrc.get_path() for chrc in self.characteristics]

    def get_characteristics(self):
        return self.characteristics

    @dbus.service.method(DBUS_PROP_IFACE, in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        if interface != GATT_SERVICE_IFACE:
            raise InvalidArgsException()
        return self.get_properties()[GATT_SERVICE_IFACE]


class Characteristic(dbus.service.Object):

    def __init__(self, bus, index, uuid, flags, service):
        self.path = service.path + '/char' + str(index)
        self.bus = bus
        self.uuid = uuid
        self.service = service
        self.flags = flags
        self.descriptors = []
        self.value = []
        self.notifying = False
        dbus.service.Object.__init__(self, bus, self.path)

    def get_properties(self):
        return {
            GATT_CHRC_IFACE: {
                'Service': self.service.get_path(),
                'UUID': dbus.String(self.uuid),
                'Flags': dbus.Array(self.flags, signature='s'),
                'Descriptors': dbus.Array(
                    self.get_descriptor_paths(), signature='o')
            }
        }

    def get_path(self):
        return dbus.ObjectPath(self.path)

    def add_descriptor(self, descriptor):
        self.descriptors.append(descriptor)

    def get_descriptor_paths(self):
        return [desc.get_path() for desc in self.descriptors]

    def get_descriptors(self):
        return self.descriptors

    @dbus.service.method(DBUS_PROP_IFACE, in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        if interface != GATT_CHRC_IFACE:
            raise InvalidArgsException()
        return self.get_properties()[GATT_CHRC_IFACE]

    @dbus.service.method(GATT_CHRC_IFACE, in_signature='a{sv}', out_signature='ay')
    def ReadValue(self, options):
        return self.value

    @dbus.service.method(GATT_CHRC_IFACE, in_signature='aya{sv}')
    def WriteValue(self, value, options):
        pass

    @dbus.service.method(GATT_CHRC_IFACE)
    def StartNotify(self):
        self.notifying = True

    @dbus.service.method(GATT_CHRC_IFACE)
    def StopNotify(self):
        self.notifying = False

    @dbus.service.signal(DBUS_PROP_IFACE, signature='sa{sv}as')
    def PropertiesChanged(self, interface, changed, invalidated):
        pass

    def send_notification(self, value):
        if not self.notifying:
            return
        self.value = value
        self.PropertiesChanged(
            GATT_CHRC_IFACE, {'Value': value}, [])


# ============================================================
# WiFi Setup Service
# ============================================================

class WifiSetupService(Service):
    def __init__(self, bus, index):
        Service.__init__(self, bus, index, WIFI_SERVICE_UUID, True)
        self.add_characteristic(WifiScanCharacteristic(bus, 0, self))
        self.add_characteristic(WifiConfigCharacteristic(bus, 1, self))
        self.add_characteristic(WifiStatusCharacteristic(bus, 2, self))
        self.add_characteristic(DeviceInfoCharacteristic(bus, 3, self))
        self.add_characteristic(AuthCharacteristic(bus, 4, self))


class WifiScanCharacteristic(Characteristic):
    """Returns JSON array of visible WiFi networks.

    ReadValue triggers an async scan and returns {"scanning": true}.
    When scan completes, a small {"wifi_results": true, "count": N}
    notification is sent (fits within any BLE MTU).  The client then
    does a second ReadValue to fetch the full cached results.

    BlueZ handles the ATT Read Blob procedure automatically — when the
    response exceeds the negotiated MTU, it issues follow-up reads with
    increasing offsets.  We keep the serialized bytes in _read_blob_data
    so subsequent calls with offset > 0 return the correct slice.
    This works reliably across all boards regardless of BLE hardware.
    """

    def __init__(self, bus, index, service):
        Characteristic.__init__(self, bus, index, WIFI_SCAN_UUID,
                                ['read', 'notify'], service)
        self._cached_networks = None
        self._scanning = False
        self._read_blob_data = None  # bytes kept for Read Blob continuation

    def ReadValue(self, options):
        offset = int(options.get('offset', 0))

        # Read Blob continuation — return remaining bytes from previous read
        if offset > 0 and self._read_blob_data is not None:
            log.debug(f'WiFi scan Read Blob offset={offset}/{len(self._read_blob_data)}')
            return dbus.Array(
                [dbus.Byte(b) for b in self._read_blob_data[offset:]],
                signature='y')

        if not is_authenticated(options):
            data = json.dumps({'error': 'not_authenticated'}).encode()
            return dbus.Array([dbus.Byte(b) for b in data], signature='y')

        # If cached results exist from a completed scan, return them
        if self._cached_networks is not None:
            networks = self._cached_networks
            self._cached_networks = None
            data = json.dumps(networks).encode()
            # Keep serialized bytes for Read Blob continuation
            self._read_blob_data = data
            log.info(f'WiFi scan: returning {len(networks)} cached networks ({len(data)} bytes)')
            return dbus.Array([dbus.Byte(b) for b in data], signature='y')

        # No cached results — trigger async scan (if not already running)
        if not self._scanning:
            self._scanning = True
            log.info('WiFi scan requested — scanning async, will notify when ready')
            GLib.idle_add(lambda: (GLib.timeout_add(100, self._do_scan), False)[-1])

        data = json.dumps({'scanning': True}).encode()
        self._read_blob_data = None  # No blob data for short responses
        return dbus.Array([dbus.Byte(b) for b in data], signature='y')

    def _do_scan(self):
        """Run WiFi scan, cache results, and deliver via two parallel paths.

        Path 1 (Read Blob): Cache results and send a "ready" notification.
                The client reads the characteristic; BlueZ handles ATT Read Blob
                to transfer data larger than the MTU.  Works on Pi 5, Pi Zero 2 W.
        Path 2 (Chunked notifications): Also send the data as chunked
                notifications with generous stagger, starting after a 1-second
                delay.  This is the fallback for boards where BlueZ does not
                handle Read Blob correctly (e.g. Pi 4B).
        The client accepts whichever path delivers complete data first.
        """
        networks = scan_wifi_networks()
        self._scanning = False

        data_str = json.dumps(networks)
        total_bytes = len(data_str.encode())
        log.info(f'WiFi scan complete: {len(networks)} networks ({total_bytes} bytes)')

        # --- Path 1: Cache for Read Blob ---
        self._cached_networks = networks
        self._read_blob_data = None  # will be set on next ReadValue

        # Send "ready" notification with total byte count so client can
        # detect truncation if Read Blob fails.
        ready_msg = json.dumps({
            'wifi_results': True,
            'count': len(networks),
            'total_bytes': total_bytes,
        }).encode()
        def send_ready():
            self.send_notification(
                dbus.Array([dbus.Byte(b) for b in ready_msg], signature='y'))
            return False
        GLib.idle_add(send_ready)

        # --- Path 2: Chunked notifications as fallback ---
        # Use a conservative chunk size that fits in any MTU (min BLE MTU is 23,
        # but iOS typically negotiates >= 185).  We use 180 bytes of data per
        # chunk — after JSON wrapping this stays well under 250 bytes.
        CHUNK_DATA_SIZE = 180
        chunks = []
        remaining = data_str
        while remaining:
            chunks.append(remaining[:CHUNK_DATA_SIZE])
            remaining = remaining[CHUNK_DATA_SIZE:]

        total = len(chunks)
        log.info(f'WiFi scan: also sending {total} notification chunks as fallback')
        for idx, chunk_data in enumerate(chunks):
            chunk_msg = json.dumps({
                'wifi_chunks': True,
                'chunks': total,
                'chunk': idx,
                'data': chunk_data,
            }).encode()
            def send_chunk(msg=chunk_msg):
                self.send_notification(
                    dbus.Array([dbus.Byte(b) for b in msg], signature='y'))
                return False
            # Start after 1s delay (give Read Blob time), 200ms stagger between chunks
            GLib.timeout_add(1000 + 200 * idx, send_chunk)

        return False  # Don't repeat the timeout


class WifiConfigCharacteristic(Characteristic):
    """Receives WiFi credentials as JSON {ssid, password, hostname?}."""

    def __init__(self, bus, index, service):
        Characteristic.__init__(self, bus, index, WIFI_CONFIG_UUID,
                                ['write'], service)
        self.write_buffer = bytearray()

    def WriteValue(self, value, options):
        self.write_buffer.extend(bytes(value))
        # Try to parse as JSON — if incomplete, wait for more chunks
        try:
            config = json.loads(self.write_buffer.decode())
            self.write_buffer = bytearray()
        except (json.JSONDecodeError, UnicodeDecodeError):
            return

        if not is_authenticated(options):
            log.warning('WiFi config rejected: not authenticated')
            return

        ssid = config.get('ssid', '')
        password = config.get('password', '')
        hostname = config.get('hostname')

        if not ssid or not password:
            log.warning('WiFi config missing ssid or password')
            return

        log.info(f'Configuring WiFi: ssid={ssid}, hostname={hostname}')

        # Find the WifiStatusCharacteristic to send notifications
        status_chrc = None
        for chrc in self.service.get_characteristics():
            if chrc.uuid == WIFI_STATUS_UUID:
                status_chrc = chrc
                break

        # Send "connecting" status
        if status_chrc:
            status_data = json.dumps({'connected': False, 'ip': '', 'error': ''}).encode()
            status_chrc.send_notification(
                dbus.Array([dbus.Byte(b) for b in status_data], signature='y'))

        # Configure WiFi in background
        def do_configure():
            result = configure_wifi(ssid, password, hostname)
            if status_chrc:
                status_data = json.dumps(result).encode()
                status_chrc.send_notification(
                    dbus.Array([dbus.Byte(b) for b in status_data], signature='y'))

        GLib.idle_add(lambda: (GLib.timeout_add(100, do_configure), False)[-1])


class WifiStatusCharacteristic(Characteristic):
    """Notifies WiFi connection result {connected, ip, error}."""

    def __init__(self, bus, index, service):
        Characteristic.__init__(self, bus, index, WIFI_STATUS_UUID,
                                ['read', 'notify'], service)

    def ReadValue(self, options):
        status = {'connected': False, 'ip': '', 'error': ''}
        try:
            ip_output = subprocess.check_output(
                ['ip', '-4', 'addr', 'show', 'wlan0'], text=True, timeout=3)
            for line in ip_output.split('\n'):
                if 'inet ' in line:
                    ip = line.strip().split(' ')[1].split('/')[0]
                    status['connected'] = True
                    status['ip'] = ip
                    break
        except Exception:
            pass
        data = json.dumps(status).encode()
        return dbus.Array([dbus.Byte(b) for b in data], signature='y')


class DeviceInfoCharacteristic(Characteristic):
    """Returns device info: hostname, version, setup_finished, device_suffix."""

    def __init__(self, bus, index, service):
        Characteristic.__init__(self, bus, index, DEVICE_INFO_UUID,
                                ['read'], service)

    def ReadValue(self, options):
        info = {
            'hostname': get_hostname(),
            'version': get_version(),
            'setup_finished': is_setup_finished(),
            'device_suffix': get_device_suffix(),
        }
        data = json.dumps(info).encode()
        return dbus.Array([dbus.Byte(b) for b in data], signature='y')


class AuthCharacteristic(Characteristic):
    """BLE authentication. Read returns {claimed, authenticated}.
    Write accepts {action: 'set_pin'|'authenticate', pin: '...'}."""

    def __init__(self, bus, index, service):
        Characteristic.__init__(self, bus, index, AUTH_UUID,
                                ['read', 'write', 'notify'], service)
        self.write_buffer = bytearray()

    def ReadValue(self, options):
        device = options.get('device', '')
        authed = str(device) in authenticated_peers if device else not is_claimed()
        info = {
            'claimed': is_claimed(),
            'authenticated': authed,
        }
        data = json.dumps(info).encode()
        return dbus.Array([dbus.Byte(b) for b in data], signature='y')

    def WriteValue(self, value, options):
        self.write_buffer.extend(bytes(value))
        try:
            msg = json.loads(self.write_buffer.decode())
            self.write_buffer = bytearray()
        except (json.JSONDecodeError, UnicodeDecodeError):
            return

        action = msg.get('action', '')
        pin = msg.get('pin', '')

        if action == 'set_pin':
            if is_claimed():
                result = {'success': False, 'error': 'already_claimed'}
                log.warning('set_pin rejected: device already claimed')
            elif len(pin) < 4 or len(pin) > 6:
                result = {'success': False, 'error': 'pin_must_be_4_to_6_digits'}
            else:
                try:
                    save_pin(pin)
                    mark_authenticated(options)
                    result = {'success': True}
                except Exception as e:
                    log.error(f'Failed to save PIN: {e}')
                    result = {'success': False, 'error': 'save_failed'}
        elif action == 'authenticate':
            if not is_claimed():
                result = {'success': False, 'error': 'not_claimed'}
            elif check_pin(pin):
                mark_authenticated(options)
                result = {'success': True}
            else:
                result = {'success': False, 'error': 'wrong_pin'}
                log.warning('Authentication failed: wrong pin')
        else:
            result = {'success': False, 'error': 'unknown_action'}

        data = json.dumps(result).encode()
        self.send_notification(
            dbus.Array([dbus.Byte(b) for b in data], signature='y'))


# ============================================================
# API Proxy Service
# ============================================================

class APIProxyService(Service):
    def __init__(self, bus, index):
        Service.__init__(self, bus, index, API_SERVICE_UUID, True)
        self.response_chrc = APIResponseCharacteristic(bus, 1, self)
        self.add_characteristic(APIRequestCharacteristic(bus, 0, self, self.response_chrc))
        self.add_characteristic(self.response_chrc)


class APIRequestCharacteristic(Characteristic):
    """Receives API requests as JSON {id, method, path, body?}.
    Forwards to local Go API and sends response via APIResponseCharacteristic."""

    def __init__(self, bus, index, service, response_chrc):
        Characteristic.__init__(self, bus, index, API_REQUEST_UUID,
                                ['write'], service)
        self.response_chrc = response_chrc
        self.write_buffer = bytearray()
        self._client_mtu = 185  # conservative default; updated from WriteValue options

    def WriteValue(self, value, options):
        # Capture negotiated MTU from BlueZ for response chunking
        if 'mtu' in options:
            self._client_mtu = int(options['mtu'])
        self.write_buffer.extend(bytes(value))
        try:
            request = json.loads(self.write_buffer.decode())
            self.write_buffer = bytearray()
        except (json.JSONDecodeError, UnicodeDecodeError):
            return

        if not is_authenticated(options):
            request_id = request.get('id', 0)
            err_response = json.dumps({'id': request_id, 'status': 403, 'body': {'error': 'not_authenticated'}}).encode()
            self.response_chrc.send_notification(
                dbus.Array([dbus.Byte(b) for b in err_response], signature='y'))
            return

        request_id = request.get('id', 0)
        method = request.get('method', 'GET')
        path = request.get('path', '/status')
        body = request.get('body')

        log.info(f'API proxy: {method} {path} (id={request_id})')

        def do_request():
            result = proxy_api_request(method, path, body)
            response = {
                'id': request_id,
                'status': result['status'],
                'body': result['body'],
            }
            response_json = json.dumps(response).encode()

            # Send response, chunking if it exceeds the negotiated BLE MTU.
            # ATT notification overhead is 3 bytes.
            max_msg = self._client_mtu - 3
            if len(response_json) <= max_msg:
                def send_single():
                    self.response_chrc.send_notification(
                        dbus.Array([dbus.Byte(b) for b in response_json], signature='y'))
                    return False
                GLib.idle_add(send_single)
            else:
                # Binary-search split: find largest data slices whose
                # JSON-wrapped chunk messages fit within max_msg.
                remaining = response_json.decode('utf-8', errors='replace')
                chunks = []
                while remaining:
                    test_msg = json.dumps({'id': request_id, 'chunks': 0, 'chunk': 0, 'data': remaining})
                    if len(test_msg.encode()) <= max_msg:
                        chunks.append(remaining)
                        break
                    lo, hi = 1, len(remaining)
                    while lo < hi:
                        mid = (lo + hi + 1) // 2
                        test_msg = json.dumps({'id': request_id, 'chunks': 0, 'chunk': 0, 'data': remaining[:mid]})
                        if len(test_msg.encode()) <= max_msg:
                            lo = mid
                        else:
                            hi = mid - 1
                    chunks.append(remaining[:lo])
                    remaining = remaining[lo:]
                total = len(chunks)
                for idx, chunk_data in enumerate(chunks):
                    chunk_msg = json.dumps({
                        'id': request_id,
                        'chunks': total,
                        'chunk': idx,
                        'data': chunk_data,
                    }).encode()
                    def send_chunk(msg=chunk_msg):
                        self.response_chrc.send_notification(
                            dbus.Array([dbus.Byte(b) for b in msg], signature='y'))
                        return False
                    # Stagger chunk notifications by 50ms to prevent BlueZ drops
                    GLib.timeout_add(50 * idx, send_chunk)

        # Run the blocking HTTP proxy call in a background thread
        # so the GLib main loop stays responsive for BLE operations
        threading.Thread(target=do_request, daemon=True).start()


class APIResponseCharacteristic(Characteristic):
    """Sends API responses as notifications."""

    def __init__(self, bus, index, service):
        Characteristic.__init__(self, bus, index, API_RESPONSE_UUID,
                                ['notify'], service)


# ============================================================
# BLE Advertisement
# ============================================================

class Advertisement(dbus.service.Object):
    PATH_BASE = '/org/bluez/sentryusb/advertisement'

    def __init__(self, bus, index, advertising_type, ad_manager,
                 service_manager=None, app=None, local_name=None):
        self.path = self.PATH_BASE + str(index)
        self.bus = bus
        self.ad_type = advertising_type
        self.ad_manager = ad_manager
        self.service_manager = service_manager
        self.app = app
        self.local_name = local_name
        # Only advertise primary service UUID.
        # A 31-byte LE advertisement payload cannot fit two 128-bit UUIDs
        # (2+16+16=34 bytes) plus a local name and flags — doing so causes
        # BlueZ / the HCI controller to return "Invalid Parameters (0x0d)".
        # The iOS app scans by WIFI_SERVICE_UUID only, so one UUID is enough.
        # The LocalName is placed in the scan response by BlueZ automatically.
        self.service_uuids = [WIFI_SERVICE_UUID]
        dbus.service.Object.__init__(self, bus, self.path)

    def get_properties(self):
        props = {
            'Type': self.ad_type,
            'ServiceUUIDs': dbus.Array(self.service_uuids, signature='s'),
        }
        if self.local_name:
            props['LocalName'] = dbus.String(self.local_name)
        properties = {
            LE_ADVERTISEMENT_IFACE: props
        }
        return properties

    def get_path(self):
        return dbus.ObjectPath(self.path)

    @dbus.service.method(DBUS_PROP_IFACE, in_signature='s', out_signature='a{sv}')
    def GetAll(self, interface):
        if interface != LE_ADVERTISEMENT_IFACE:
            raise InvalidArgsException()
        return self.get_properties()[LE_ADVERTISEMENT_IFACE]

    @dbus.service.method(LE_ADVERTISEMENT_IFACE, in_signature='', out_signature='')
    def Release(self):
        log.info(f'Advertisement released: {self.path}')
        # BlueZ released the advertisement (happens after a connection or internal
        # timeout).  Schedule a re-registration so the Pi stays discoverable.
        GLib.timeout_add(2000, self._reregister)

    def _reregister(self, retry_count=0):
        max_retries = 5
        log.info(f'Re-registering advertisement... (attempt {retry_count + 1})')

        def on_success():
            log.info('Advertisement registered')
            if not (self.service_manager and self.app):
                return
            # Always attempt GATT re-registration after advertisement Release.
            # The adapter internally resets on every iOS connect/disconnect
            # (visible as "Destroy Adv Monitor Manager" in bluetoothd logs),
            # silently dropping all GATT applications with no signal to the
            # daemon.  The reset can be fast enough (~1s) that the first
            # advertisement re-registration attempt succeeds without errors,
            # so we cannot rely on error detection to know GATT was dropped.
            # Re-registering unconditionally is safe: BlueZ returns AlreadyExists
            # if GATT is still registered (no disruption), and re-registers
            # cleanly if it was dropped.
            def on_gatt_ok():
                log.info('GATT application re-registered')
            def on_gatt_err(error):
                if 'AlreadyExists' in str(error):
                    pass  # GATT was still registered — no action needed
                else:
                    log.error(f'Failed to re-register GATT after advertisement release: {error}')
            self.service_manager.RegisterApplication(
                self.app.get_path(), {},
                reply_handler=on_gatt_ok,
                error_handler=on_gatt_err)

        def on_reregister_error(error):
            if retry_count < max_retries:
                delay = min(2000 * (2 ** retry_count), 30000)  # exponential backoff, max 30s
                log.warning(f'Advertisement re-registration failed (attempt {retry_count + 1}/{max_retries + 1}): {error} — retrying in {delay}ms')
                GLib.timeout_add(delay, self._reregister, retry_count + 1)
            else:
                log.error(f'Advertisement re-registration failed after {max_retries + 1} attempts: {error} — GATT server still running but Pi is not discoverable')

        self.ad_manager.RegisterAdvertisement(
            self.get_path(), {},
            reply_handler=on_success,
            error_handler=on_reregister_error)
        return False  # don't repeat


# ============================================================
# Main
# ============================================================

def find_adapter(bus):
    """Find the first Bluetooth adapter that supports LE."""
    remote_om = dbus.Interface(
        bus.get_object(BLUEZ_SERVICE, '/'), DBUS_OM_IFACE)
    objects = remote_om.GetManagedObjects()
    for path, interfaces in objects.items():
        if LE_ADVERTISING_MANAGER_IFACE in interfaces:
            return path
    return None

def register_ad_cb():
    log.info('Advertisement registered')

def register_ad_error_cb(error):
    log.error(f'Failed to register advertisement: {error}')
    sys.exit(1)  # non-zero so systemd Restart=on-failure triggers

def register_app_cb():
    log.info('GATT application registered')

def register_app_error_cb(error):
    log.error(f'Failed to register GATT application: {error}')
    sys.exit(1)  # non-zero so systemd Restart=on-failure triggers


def verify_gatt_objects(app):
    """Self-test: verify GATT objects are properly structured after registration."""
    try:
        objects = app.GetManagedObjects()
        service_uuids = []
        chrc_uuids = []
        for path, ifaces in objects.items():
            if GATT_SERVICE_IFACE in ifaces:
                service_uuids.append(ifaces[GATT_SERVICE_IFACE].get('UUID', '?'))
            if GATT_CHRC_IFACE in ifaces:
                chrc_uuids.append(ifaces[GATT_CHRC_IFACE].get('UUID', '?'))
        log.info(f'GATT self-test: {len(objects)} objects, '
                 f'{len(service_uuids)} services {service_uuids}, '
                 f'{len(chrc_uuids)} characteristics')
        if len(service_uuids) < 2:
            log.error(f'GATT self-test FAILED: expected 2 services, got {len(service_uuids)} — exiting for systemd restart')
            mainloop.quit()
            sys.exit(1)
        if len(chrc_uuids) < 7:
            log.error(f'GATT self-test FAILED: expected 7+ characteristics, got {len(chrc_uuids)} — exiting for systemd restart')
            mainloop.quit()
            sys.exit(1)
    except Exception as e:
        log.error(f'GATT self-test exception: {e}')
    return False  # don't repeat GLib timeout


def setup_connection_monitoring(bus, adapter_path):
    """Subscribe to BlueZ D-Bus signals to log BLE central connect/disconnect
    events.  Without this, the daemon has no visibility into whether iOS
    actually established a connection — making pairing failures hard to debug.

    Also catches cases where iOS connects but GATT service discovery fails
    (seen as a connection log with no subsequent characteristic read logs).
    """
    def on_properties_changed(interface, changed, invalidated, path=None):
        if interface != 'org.bluez.Device1':
            return
        if path is None or not str(path).startswith(adapter_path):
            return
        if 'Connected' not in changed:
            return
        if changed['Connected']:
            log.info(f'BLE central connected: {path}')
        else:
            log.info(f'BLE central disconnected: {path}')

    bus.add_signal_receiver(
        on_properties_changed,
        dbus_interface=DBUS_PROP_IFACE,
        signal_name='PropertiesChanged',
        path_keyword='path',
        bus_name=BLUEZ_SERVICE)


def setup_bluez_restart_detection(bus):
    """Exit when BlueZ (org.bluez) disappears or restarts on D-Bus.

    When bluetoothd crashes or is restarted by systemd, all registered GATT
    applications and advertisements are dropped silently.  The daemon's mainloop
    stays alive but BlueZ no longer serves our custom GATT — it falls back to
    PipeWire audio services (0x1844, 0x184D, 0x184F), causing iOS to cache the
    wrong GATT and fail BLE reconnection until Bluetooth is toggled.

    Exiting here lets systemd's Restart=always relaunch the daemon fresh, which
    re-registers GATT with the newly started bluetoothd.  Combined with
    BindsTo=bluetooth.service in the service unit, this ensures the daemon is
    always in sync with bluetoothd's lifecycle.
    """
    def on_name_owner_changed(name, old_owner, new_owner):
        if name != BLUEZ_SERVICE:
            return
        if old_owner and not new_owner:
            log.warning('BlueZ (org.bluez) disappeared — GATT registration lost, exiting for systemd restart')
            mainloop.quit()
            sys.exit(1)
        if not old_owner and new_owner:
            # BlueZ came back after being absent; our GATT registration is still
            # gone so exit to let systemd restart us cleanly against the new instance.
            log.info('BlueZ (org.bluez) reappeared — exiting for clean GATT re-registration')
            mainloop.quit()
            sys.exit(1)

    bus.add_signal_receiver(
        on_name_owner_changed,
        dbus_interface='org.freedesktop.DBus',
        signal_name='NameOwnerChanged',
        bus_name='org.freedesktop.DBus')


# ============================================================
# Android BLE pairing agent
# ============================================================
# Android's BLE stack triggers a BlueZ pairing request on first connect.
# Without a registered agent, BlueZ rejects it and the connection times out.
# iOS uses CoreBluetooth internally and is unaffected.
# Registering a NoInputNoOutput agent makes BlueZ auto-accept just-works
# pairing, so Android clients pair silently on first connection.

AGENT_PATH = "/com/sentryusb/PairingAgent"


class AutoPairAgent(dbus.service.Object):
    @dbus.service.method("org.bluez.Agent1", in_signature="o", out_signature="u")
    def RequestPasskey(self, device):
        return dbus.UInt32(0)

    @dbus.service.method("org.bluez.Agent1", in_signature="ou", out_signature="")
    def DisplayPasskey(self, device, passkey):
        pass

    @dbus.service.method("org.bluez.Agent1", in_signature="ou", out_signature="")
    def RequestConfirmation(self, device, passkey):
        pass

    @dbus.service.method("org.bluez.Agent1", in_signature="o", out_signature="")
    def RequestAuthorization(self, device):
        pass

    @dbus.service.method("org.bluez.Agent1", in_signature="os", out_signature="")
    def AuthorizeService(self, device, uuid):
        pass

    @dbus.service.method("org.bluez.Agent1", in_signature="", out_signature="")
    def Cancel(self):
        pass


def register_android_pairing_agent(bus):
    agent = AutoPairAgent(bus, AGENT_PATH)
    mgr = dbus.Interface(
        bus.get_object("org.bluez", "/org/bluez"),
        "org.bluez.AgentManager1")
    mgr.RegisterAgent(AGENT_PATH, "NoInputNoOutput")
    mgr.RequestDefaultAgent(AGENT_PATH)
    log.info("Android pairing agent registered (NoInputNoOutput)")
    return agent


def main():
    maybe_install_dbus_policy()
    global mainloop, API_BASE
    dbus.mainloop.glib.DBusGMainLoop(set_as_default=True)
    bus = dbus.SystemBus()

    # Claim a well-known bus name so the D-Bus policy file takes effect.
    # This allows BlueZ to call GetManagedObjects and GATT methods on us
    # even on systems with strict D-Bus policies (e.g. Pi 5 / Bookworm).
    try:
        bus_name = dbus.service.BusName('com.sentryusb.ble', bus,
                                        do_not_queue=True)
        log.info('Claimed D-Bus bus name: com.sentryusb.ble')
    except dbus.exceptions.NameExistsException:
        log.warning('D-Bus bus name com.sentryusb.ble already claimed, using unique name')
    except Exception as e:
        log.warning(f'Could not claim D-Bus bus name: {e} — using unique name')

    # Detect which port the Go API server is on (80 production, 8788 dev)
    API_BASE = detect_api_base()

    adapter_path = find_adapter(bus)
    if not adapter_path:
        log.error('No Bluetooth LE adapter found')
        sys.exit(1)

    log.info(f'Using adapter: {adapter_path}')

    # Subscribe to BlueZ D-Bus signals so connection events are logged.
    # This makes it possible to see whether iOS actually connects to the Pi
    # versus failing at the BLE advertisement/discovery stage.
    setup_connection_monitoring(bus, adapter_path)

    # Exit (triggering systemd restart) if bluetoothd restarts, which drops
    # our GATT registration and causes iOS to see stale/wrong services.
    setup_bluez_restart_detection(bus)

    # Register a NoInputNoOutput pairing agent so Android devices can pair
    # silently without a PIN prompt.  iOS uses CoreBluetooth internals and
    # doesn't need this, but Android's BLE stack requests a pairing confirmation
    # that BlueZ will reject (timing out the connection) unless an agent is
    # registered.  NoInputNoOutput tells BlueZ the Pi has no keyboard/display,
    # causing it to auto-accept the just-works pairing automatically.
    _pairing_agent = register_android_pairing_agent(bus)

    # Power on adapter and set unique BLE name
    adapter_props = dbus.Interface(
        bus.get_object(BLUEZ_SERVICE, adapter_path), DBUS_PROP_IFACE)
    adapter_props.Set('org.bluez.Adapter1', 'Powered', dbus.Boolean(True))
    ble_name = f'SentryUSB-{get_device_suffix()}'
    adapter_props.Set('org.bluez.Adapter1', 'Alias', dbus.String(ble_name))
    log.info(f'BLE adapter alias set to: {ble_name}')

    # Update Avahi mDNS service name to match the unique BLE name
    update_avahi_service_name(ble_name)

    # Register GATT application
    service_manager = dbus.Interface(
        bus.get_object(BLUEZ_SERVICE, adapter_path), GATT_MANAGER_IFACE)

    app = Application(bus)
    service_manager.RegisterApplication(
        app.get_path(), {},
        reply_handler=register_app_cb,
        error_handler=register_app_error_cb)

    # Register advertisement
    ad_manager = dbus.Interface(
        bus.get_object(BLUEZ_SERVICE, adapter_path), LE_ADVERTISING_MANAGER_IFACE)

    adv = Advertisement(bus, 0, 'peripheral', ad_manager,
                        service_manager=service_manager, app=app,
                        local_name=ble_name)
    ad_manager.RegisterAdvertisement(
        adv.get_path(), {},
        reply_handler=register_ad_cb,
        error_handler=register_ad_error_cb)

    log.info(f'SentryUSB BLE peripheral started: {ble_name}')
    log.info(f'WiFi Setup Service: {WIFI_SERVICE_UUID}')
    log.info(f'API Proxy Service:  {API_SERVICE_UUID}')

    # Run self-test after 3s to verify GATT objects are properly registered
    GLib.timeout_add(3000, verify_gatt_objects, app)

    mainloop = GLib.MainLoop()

    def signal_handler(sig, frame):
        log.info('Shutting down...')
        mainloop.quit()

    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)

    try:
        mainloop.run()
    except KeyboardInterrupt:
        pass


if __name__ == '__main__':
    main()
