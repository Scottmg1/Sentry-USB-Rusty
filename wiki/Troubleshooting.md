# Troubleshooting

Common issues and how to resolve them.

---

## Pi Won't Connect to WiFi

- **Double-check SSID and password** — case-sensitive, watch for special characters
- **Connect a monitor + keyboard** to debug directly
- **Pi Zero**: Creates a USB gadget network interface — plug into your computer and try `ssh pi@169.254.x.x`
- **Hidden SSID** (Bookworm+): Use `sudo nmcli device wifi connect "SSID" password "PASS" hidden yes` via SSH
- **Hidden SSID** (legacy): Edit `/boot/wpa_supplicant.conf` and uncomment `scan_ssid=1` in the `network={...}` block

## Tesla Doesn't See the USB Drive

- Use a **data** cable, not a charge-only cable
- **Pi Zero**: Make sure you're plugged into the **USB** port, not the **PWR** port
- **Pi 4/5**: The USB-C port is used for both power and data — only plug into the Tesla after setup is complete
- Wait 2–3 minutes. Check Dashboard → "USB Drives" should show "Connected"

## Web UI Won't Load

1. Try the IP address directly: check your router's DHCP list for the Pi's IP
2. SSH in and check the service:
   ```bash
   ssh pi@sentryusb.local
   sudo systemctl status sentryusb
   sudo journalctl -u sentryusb -f
   ```
3. If the service isn't running, try restarting it:
   ```bash
   sudo systemctl restart sentryusb
   ```

## Setup Fails or Gets Stuck

- Check **Logs** → "Setup Log" in the web UI
- Common causes:
  - Wrong WiFi password
  - Archive server unreachable
  - SD card too small (64 GB minimum, 128 GB+ recommended)
- **The Pi rebooting multiple times during setup is normal** (3–5 reboots, 10–20 minutes total)
- You can safely re-run the wizard — it's idempotent
- Try `sudo -i` then `/etc/rc.local` to manually restart the setup process

### LED Flash Stages During Setup

| Flashes | Stage |
|---------|-------|
| 2 | Verifying configuration |
| 3 | Downloading setup scripts |
| 4 | Creating drive partitions |
| 5 | Setup complete, rebooting |

## WiFi or Access Point Stops Working After Setup

If WiFi or the AP breaks after a reboot (common symptom: `brcmf_cfg80211_stop_ap` or `dnsmasq: Read-only file system` in logs), the read-only root networking fix can be applied without re-running the full setup:

```bash
ssh pi@sentryusb.local
sudo -i
/root/bin/setup-sentryusb fix_networking
reboot
```

This updates fstab and networking paths so WiFi and Ethernet work even when the mutable partition is slow to mount. Safe to run multiple times.

## Archive Not Working

1. **Check connectivity**: Can the Pi reach your archive server?
   ```bash
   ping -c 3 your-server
   ```
2. **Check credentials**: Re-run the Setup Wizard and verify the archive settings
3. **Check logs**: Go to **Logs** → "Archive Loop" in the web UI
4. **rsync**: Make sure SSH keys are set up (`ssh-copy-id` — see [Archive Methods](ArchiveMethods))
5. **rclone**: Make sure `rclone config` was run and the remote name matches what's in the wizard

## Read-Only Filesystem Errors

If you get "read-only filesystem" errors when trying to edit files via SSH:

```bash
sudo -i
/root/bin/remountfs_rw
```

This temporarily remounts the root filesystem as read-write. It will return to read-only after a reboot.

## System Clock Is Wrong

If the date is far off, SSL/TLS authentication will fail, preventing downloads and updates:

```bash
date -s "20 Feb 2026 15:04:05"
```

Or wait for NTP to sync after WiFi connects.

## Diagnostics

### From the Web UI
Go to **Settings** and use the diagnostics download feature to get a full system report.

### From SSH
```bash
sudo /root/bin/setup-sentryusb diagnose
```

This collects system info, logs, and configuration (with sensitive values masked) into a diagnostics bundle.

### Useful Commands

| Command | What It Does |
|---------|-------------|
| `sudo systemctl status sentryusb` | Check if the SentryUSB service is running |
| `sudo journalctl -u sentryusb -f` | Live-tail the SentryUSB server logs |
| `tail -f /sentryusb/sentryusb-setup.log` | Watch setup logs in real time |
| `df -h` | Check disk space |
| `lsblk` | List block devices and partitions |
| `vcgencmd measure_temp` | Check CPU temperature |

---

## Still Stuck?

- Check the [FAQ](FAQ) for common questions
- Search [existing issues](https://github.com/Scottmg1/Sentry-USB/issues) on GitHub
- Ask on [Discord](https://discord.gg/9QZEzVwdnt)
- File a [bug report](https://github.com/Scottmg1/Sentry-USB/issues/new?template=bug_report.yml) if you've found a bug