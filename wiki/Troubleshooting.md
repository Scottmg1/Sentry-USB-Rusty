﻿# Troubleshooting

Most install problems fall into a few buckets. If your issue isn't here, ping **[Discord](https://discord.gg/9QZEzVwdnt)** — answers come fast and we'll add common ones to this page over time.

## Can't reach http://sentryusb.local

The Pi is up but its hostname isn't resolving on your network.

**Try:**

1.  Open your router's admin page and confirm the Pi shows up as `sentryusb`. Use the IP directly in your browser instead (e.g., `http://192.168.1.47`).
2.  On Windows, install **[Bonjour Print Services](https://support.apple.com/kb/DL999)** — Windows doesn't ship with mDNS by default.
3.  Some corporate / mesh / guest WiFi networks block mDNS broadcasts. Try a different network or use the IP.

## sentryusb.local stops resolving during setup

Setup reboots the Pi several times, and each reboot drops the `sentryusb.local` name until mDNS re-announces it — which can take a minute, and some networks/browsers cache the failure longer. The Pi itself is fine and setup keeps running on the device.

**Try:**

1.  Open `http://<ip>` in a new tab instead — the IP is printed at the end of the installer output, and the setup page shows it while waiting for a reboot. You can also find it on your router's admin page.
2.  The setup page on the IP picks up right where it left off — setup resumes automatically after every reboot, so nothing is lost.

## Setup loops forever on "Shrinking root partition table"

Older versions could get stuck endlessly re-shrinking the root partition when Raspberry Pi OS's own first-boot auto-expand was still active (it re-grew the filesystem after every shrink). This is fixed — re-run the installer to get the latest version:

```bash
curl -fsSL https://raw.githubusercontent.com/Sentry-Six/Sentry-USB-Rusty/main/install-pi.sh | bash
```

If setup now stops with "Root partition could not be shrunk after 3 attempts", re-flash the SD card/SSD with Raspberry Pi Imager (with OS customization disabled) or Balena Etcher and reinstall.

## Tesla dashcam icon never appears

The car isn't seeing the Pi as a USB drive.

**Try:**

1.  **Check the cable** — make sure it's a **USB data cable**, not charge-only. The cheap ones bundled with most USB chargers are charge-only.
2.  On a **Pi Zero 2 W**, connect the Tesla cable to the port labelled **USB**, not **PWR IN**. The `PWR IN` port can power the Pi, but it does not carry the USB data connection the car needs.
3.  Plug the Pi into a different USB port on the Tesla. Newer cars: glovebox port. Older cars: front console ports.
4.  Power-cycle the Pi (unplug, wait 5 seconds, plug back in).
5.  SSH into the Pi and check the main service: `systemctl status sentryusb`. It includes the USB-gadget functionality; there is no separate `sentryusb-gadget` systemd unit.

## My clips are being saved, but I can't see or access them.

If you know your files are being saved, but Sentry USB can't see them and you are unable to access them, Dashcam Encryption may be enabled on your vehicle. At the moment, Sentry USB does not support Dashcam Encryption and we require that you disable it before use.

To disable it, go to:
<br> Settings → Safety → Dashcam → "Encrypt Dashcam Recordings"

> [!NOTE]
> The "Encrypt Dashcam Recordings" feature is only available for AMD-equipped vehicles.

## BLE Keep Awake stops working / can't pair

The Bluetooth keep-awake holds your Tesla awake by talking to the car over BLE. If pairing fails or the connection drops, **Logs → Bluetooth** has a live diagnostic dump.

**Try:**

1.  **Logs → Bluetooth** — shows which adapter the daemon picked, connection state, sample-DB counts, and the recent sampler journal lines.
2.  Click **Download Bluetooth Bundle** on that tab to grab a single text report (full journal, LE params, hciconfig, rfkill, pairing state, dmesg, and the per-minute history) for sharing on Discord — saves a lot of back-and-forth.
3.  Re-pair from **Settings → Car & Network → "BLE Pairing"** card: confirm the VIN, click **Re-pair**, stay near the car, and **tap your key card on the center console** when prompted. The pair handler power-cycles the adapter and verifies the link before declaring success.
4.  Reduce nearby BLE keys before re-pairing — Tesla allows only a few active BLE keys, and phones, watches, and BLE commanders can use them all up and block the Pi from pairing.

## CIFS/SMB connection fails

**Try:**

1.  Re-run the [Setup Wizard](Setup-Wizard-Guide) → Archive step → fill in **CIFS Version** as `2.0` or `1.0` (some older NAS devices reject SMB3 negotiation).
2.  Check your password — special characters sometimes need escaping; try a simpler password as a test.
3.  From the Pi, try mounting manually:
    ```bash
    sudo mount -t cifs //<server>/<share> /mnt -o username=<user>
    ```
    If that errors, the error message tells you exactly what's wrong.

## Still stuck?

*   **[Discord](https://discord.gg/9QZEzVwdnt)** — fastest help, real humans
*   **[Open an issue](https://github.com/Sentry-Six/Sentry-USB-Rusty/issues)** — for reproducible bugs
*   Include: Pi model, what step you're stuck on, exact error message, and the output of `journalctl -u sentryusb -n 100` if relevant.
