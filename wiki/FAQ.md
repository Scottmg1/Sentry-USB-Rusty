# Frequently Asked Questions (FAQ)

## Does this void my Tesla warranty?

We can't speak for Tesla — read your warranty terms if it matters. The Pi connects to a regular Tesla USB port the same way any USB drive does. It writes to itself, not to your car.

## Does Tesla officially support this?

No. Sentry USB is a third-party source available project, not affiliated with Tesla.

## Can I use the new dashcam encryption feature with this?

No. Tesla encrypts and stores the keys on the car and with your Tesla account. Decrypting the video file requires your Tesla credentials. To use Sentry USB, you must disable the dashcam encryption feature.

To do so, find the toggle in your settings:
Settings → Safety → Dashcam → "Encrypt Dashcam Recordings"

> [!NOTE]
> The dashcam encryption feature is only available for AMD-equipped vehicles.

## Does it cost anything?

The Pi software itself is **free and source-available** under the Polyform Noncommercial license. You only pay for the hardware (Pi + SD card + cable).

The optional [Sentry Cloud](Sentry-Cloud) service has a paid tier — see [sentryusb.com](https://sentryusb.com) for details.

## Can I use it without internet?

Yes. Sentry USB only needs internet for:

*   **First-time setup** (downloads the binary, installs system packages).
*   **Updates** (auto-update checks).
*   **Cloud sync** (if you use Sentry Cloud).
*   Some **archive backends** (rclone to cloud storage).

All local archive methods (CIFS, NFS, rsync to a LAN server) work offline.

## How often does it archive? Can I trigger manually?

Sentry USB archives **whenever the Pi connects to a known WiFi network**. For most users, that means every time you park in your driveway or garage.

To trigger manually, open the web UI and click the **Archive Sync** action at the top of the **Settings** page.

## Can I run it on hardware other than a Raspberry Pi?

Officially supported: Raspberry Pi 4B, Pi 5, Pi Zero 2 W, Pi 3A+ (needs a USB-A-to-USB-A cable).

**Not** the Pi 3B or 3B+: their USB ports go through a hub chip that strips USB device (OTG) mode, so they can only act as hosts and can never appear as a drive to the car. This is a hardware limitation — no setting can work around it. The same applies to the Pi 2 and Pi 1, and the original Pi Zero W is too underpowered.

Community-tested: Radxa Rock Pi 4C+, Radxa Zero 3W. These work but we don't actively test on them.

Anything else is uncharted — community help on [Discord](https://discord.gg/9QZEzVwdnt) is your best bet.

## What does Sentry Cloud see of my drives?

Nothing readable. Each route is encrypted on the Pi before it leaves your network. The cloud only ever sees ciphertext. Decryption happens in your browser when you sign in to view a drive — there's no key on the server.

See [Sentry Cloud](Sentry-Cloud) for the short version, or [sentryusb.com](https://sentryusb.com) for the full pitch.
