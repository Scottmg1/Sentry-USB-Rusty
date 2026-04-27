# Setup Wizard Guide

The Setup Wizard is a 9-step guided configuration tool built into the SentryUSB web UI. It covers everything from network settings to notifications — no SSH or config file editing required.

## Opening the Wizard

1. Open **http://sentryusb.local** in your browser
2. Go to **Settings** → **Open Wizard**

You can also import an existing `sentryusb.conf` file on the Welcome step — the wizard will pre-fill all recognized settings.

> **Tip**: You can click any step in the progress bar to jump directly to it. You can also re-run the wizard at any time to change settings.

---

## Step 1: Welcome

The landing page of the wizard. Here you can:

- **Import a config file** — Drag and drop (or browse for) an existing `sentryusb.conf` file. The wizard parses all `export` variables and pre-fills the corresponding fields across all steps. Imported values are grouped and displayed for review.
- **Start fresh** — Just click **Next** to begin with default values.

---

## Step 2: Network

Configure how SentryUSB connects to your network.

| Field | Description |
|-------|-------------|
| **Device Hostname** | The `.local` name for your Pi (default: `sentryusb`). Accessible at `hostname.local`. |

**WiFi**: WiFi credentials (SSID/password) are configured during SD card imaging in Raspberry Pi Imager, not in the wizard. The wizard shows an info banner explaining this. If you need to change WiFi later, re-flash or use `sudo nmcli device wifi connect "SSID" password "PASS"` via SSH.

**WiFi Access Point** (optional): Create a hotspot so you can access SentryUSB on the road without home WiFi.

| Field | Description |
|-------|-------------|
| **Enable WiFi Access Point** | Toggle to create a hotspot |
| **AP SSID** | Name of the hotspot (default: `SENTRYUSB WIFI`) |
| **AP Password** | Must be at least 8 characters |
| **AP IP Address** | IP for the hotspot (default: `192.168.66.1`) |

When the AP is enabled, connect to it from your phone/laptop and open `http://192.168.66.1` to access SentryUSB on the road.

---

## Step 3: Storage

Configure the size of each USB drive partition that Tesla sees.

| Field | Description |
|-------|-------------|
| **Dashcam Size** | Storage for TeslaCam recordings (default: 40 GB). Recommended: 40–60 GB. |
| **Music** | Optional music drive. Leave empty for none. |
| **LightShow** | Optional lightshow drive. Leave empty for none. |
| **Boombox** | Optional boombox drive. Leave empty for none. |
| **External Data Drive** | Select an external USB/NVMe drive instead of using the SD card. Click **Refresh** to detect connected drives. |
| **Use ExFAT** | Toggle filesystem type (default: enabled). |

> **Important**: Don't allocate your entire SD card to the dashcam. Leave room for snapshots — the car saves ~1 hour of recent footage and rotates it. If there's not enough free space, recent clips won't save properly.

---

## Step 4: Archive

Choose how recorded clips are automatically backed up when the Pi connects to WiFi.

| Method | Description |
|--------|-------------|
| **CIFS / SMB** | Windows/Mac file sharing. Fields: Archive Server, Share Name, Username, Password, Domain (optional), CIFS Version (optional). |
| **rsync** | SSH-based file sync. Fields: Server, Username, Remote Path. Requires SSH key setup after wizard (see [Archive Methods](ArchiveMethods)). |
| **rclone** | Cloud storage (Google Drive, S3, Dropbox, etc.). Fields: Remote Name, Remote Path, Archive Server (for connectivity checks). Requires `rclone config` via SSH after wizard. |
| **NFS** | Network File System. Fields: NFS Server, Export Path. |
| **None** | No archiving — clips stay on the Pi's local storage only. |

**What to Archive** checkboxes (when archive is enabled):
- Saved Clips (default: on)
- Sentry Clips (default: on)
- Recent Clips (default: on)
- Track Mode Clips (default: on)

---

## Step 5: Keep Awake

The Tesla may cut USB power when it goes to sleep. Choose a method to keep the car awake during archiving.

| Method | Description | Fields |
|--------|-------------|--------|
| **BLE** | Bluetooth Low Energy wake. Requires pairing after setup. | Vehicle VIN |
| **TeslaFi** | Uses TeslaFi API to prevent sleep. | TeslaFi API Token |
| **Tessie** | Uses Tessie API to prevent sleep. | Tessie API Token, Vehicle VIN |
| **Webhook** | Custom HTTP call (e.g., Home Assistant). | Webhook URL |
| **None** | Don't actively keep car awake. | — |

**Sentry Mode Behavior**: When a keep-awake method is selected, you can choose how the Pi behaves when Sentry Mode is active (e.g., always archive, only archive when sentry is off, etc.).

After setup, if you chose BLE, go to **Settings** → **Pair BLE** to complete the Bluetooth pairing with your car.

---

## Step 6: Notifications

Get push notifications when archiving completes or errors occur. Enable any combination of providers.

| Provider | Fields |
|----------|--------|
| **Pushover** | User Key, App Key |
| **Gotify** | Domain, App Token, Priority |
| **Discord** | Webhook URL |
| **Telegram** | Chat ID, Bot Token |
| **IFTTT** | Event Name, Key |
| **Slack** | Webhook URL |
| **Signal** | Signal CLI URL, From Number, To Number |
| **Matrix** | Server URL, Username, Password, Room ID |
| **AWS SNS** | Region, Access Key ID, Secret Key, Topic ARN |
| **Webhook** | Webhook URL |
| **ntfy** | URL & Topic, Access Token (optional), Priority |

You can also set a **Notification Title** (defaults to "SentryUSB").

See [Notifications](Notifications) for detailed instructions on obtaining API keys for each provider.

---

## Step 7: Security

Protect the web interface and SSH access.

| Field | Description |
|-------|-------------|
| **Web Username** | Username for web UI authentication. Leave empty to disable web auth. |
| **Web Password** | Password for web UI authentication. |
| **SSH Public Key** | Optional. Paste your public key to allow SSH login as root. |
| **Disable SSH password auth** | Only enable this if you've set an SSH public key above. |

---

## Step 8: Advanced

Fine-tune system behavior.

**Time Zone**: Searchable dropdown of all IANA timezones. Default: `auto` (detect automatically).

**Archive Tuning**:
| Field | Description |
|-------|-------------|
| Archive Delay (seconds) | Delay between WiFi connect and archiving start (default: 20) |
| Snapshot Interval (seconds) | Set ~2 min shorter than car's RecentClips retention |

**Temperature Monitoring** (toggle between °C and °F):
| Field | Description |
|-------|-------------|
| Warning Threshold | High temp warning (default: 68°C / 154.4°F) |
| Caution Threshold | Moderate temp alert (default: 55°C / 131°F) |
| Log Interval (minutes) | How often to log temperature (default: 60) |
| Log temperature after archive | Toggle (default: on) |

**System Tuning**:
| Field | Description |
|-------|-------------|
| Increase Root Size | Extra space for packages (e.g., `500M` or `2G`). Only works during initial setup. |
| Additional Packages | Space-separated list of apt packages to install |
| CPU Governor | Leave empty for SentryUSB defaults |
| Dirty Background Bytes | VM write-back tuning. Leave empty for defaults. |

**Drive Map**: Automatically extract GPS data from dashcam clips after archiving to build a map of all your drives. Choose distance unit (miles or kilometers).

**Update Source**: Configure which GitHub repo and branch SentryUSB pulls updates from (default: `Sentry-Six` / `main`).

---

## Step 9: Review

Displays a summary of all configured settings, grouped by category. Sensitive values (passwords, tokens) are masked.

- Review your settings
- Click **Apply & Run Setup** to save the configuration and start the setup process

## What Happens After Apply

1. The wizard saves all settings to `/root/sentryusb.conf`
2. Setup scripts begin running automatically
3. The Pi will reboot **several times** (3–5 reboots is normal)
4. The full process takes **10–20 minutes**
5. **Do not power off the device** during this time
6. The web UI shows a live progress screen with status updates
7. When complete, you'll see a "Setup Complete!" message and can go to the Dashboard

LED flash stages during setup:
| Flashes | Stage |
|---------|-------|
| 2 | Verifying configuration |
| 3 | Downloading setup scripts |
| 4 | Creating drive partitions |
| 5 | Setup complete, rebooting |
