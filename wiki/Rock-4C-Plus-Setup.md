# Rock 4C+ Setup

> **Tested with:** Armbian 26.8.0 Minimal, DietPi v10.5.2

## Sections

- [DietPi](#dietpi) — recommended, boots faster and uses fewer resources
- [Armbian](#armbian)
- [Cabling](#cabling)

## Cabling

The Rock 4C+ needs both power and a data connection to the car. The OTG data port is the **top blue USB 3.0 port** — the Rock 4C+ has four USB ports: two black USB 2.0 and two blue USB 3.0. Only the top blue port supports OTG/peripheral mode.

### Option 1: USB-C car charger

- **12V USB-C car charger** supporting at least 5V/3A, plugged into the car's 12V outlet
- **USB 3.0 A male to A male cable** from the car's glovebox USB-A port to the top blue USB 3.0 port on the Rock 4C+

This provides power via the car charger and data via the glovebox USB port.

### Option 2: USB splitter cable

Use a splitter to get both power and data from the car's USB-A port:

- **USB 3.0 A male to USB 2.0 A female / USB 3.0 A female splitter cable**
- **USB 3.0 A male to A male cable**
- **USB A to C cable**

Wiring:

1. Plug the splitter cable's A male end into the car's glovebox USB-A port
2. Plug the USB 3.0 A-to-A cable from the **USB 3.0 female** port on the splitter into the **top blue USB 3.0 port** on the Rock 4C+ (data)
3. Plug the USB A-to-C cable from the **USB 2.0 female** port on the splitter into the **USB-C port** on the Rock 4C+ (power)

## DietPi

### 1. Flash the image

Flash the DietPi Rock 4C+ image to your SD card using your preferred flashing tool (Raspberry Pi Imager, balenaEtcher, etc.).

### 2. Configure WiFi and user (before first boot)

After flashing, the SD card has a boot partition with configuration files you need to edit **before** booting the board:

- **`dietpi-wifi.txt`** — set your WiFi SSID and password
- **`dietpi.txt`** — set `AUTO_SETUP_NET_WIFI_ENABLED=1`, your user password, and other options

See [DietPi's automation guide](https://dietpi.com/docs/usage/#how-to-do-an-automatic-base-installation-at-first-boot-dietpi-automation) for the full list of options.

### 3. Boot and SSH in

Boot the board, find its IP on your router, and SSH in:

```bash
ssh root@<ip>
```

### 4. Install the dwc3 USB gadget overlay (required)

The OTG port defaults to **host mode**. Sentry USB needs it in **peripheral mode** to present as a USB mass-storage device the Tesla can read. This requires a device-tree overlay.

> Run these steps as root (`sudo -i`).

**Install dependencies:**

```bash
apt install -y device-tree-compiler
```

**Write the overlay source:**

```bash
cat > /boot/overlay-user/sentryusb-dwc3-hs.dts << 'EOF'
/dts-v1/;
/plugin/;
/ {
    compatible = "rockchip,rk3399";
    fragment@0 {
        target = <&usbdrd_dwc3_0>;
        __overlay__ {
            dr_mode = "peripheral";
            compatible = "snps,dwc3";
            reg = <0x0 0xfe800000 0x0 0x100000>;
            phys = <&tcphy0_usb3>;
            phy-names = "usb3-phy";
            phy_type = "utmi_wide";
            snps,dis_enblslpm_quirk;
            snps,dis-u2-freeclk-exists-quirk;
            snps,dis_u2_susphy_quirk;
            snps,dis-del-phy-power-chg-quirk;
            snps,xhci-slow-suspend-quirk;
        };
    };
};
EOF
```

**Compile and register:**

```bash
dtc -@ -I dts -O dtb -o /boot/overlay-user/sentryusb-dwc3-hs.dtbo /boot/overlay-user/sentryusb-dwc3-hs.dts
```

> The `reg_format` and `avoid_default_addr_size` warnings from `dtc` are expected and harmless — the kernel inherits `#address-cells`/`#size-cells` from the base DTB at apply time.

Verify that `/boot/dietpiEnv.txt` contains the overlay. Check that this line is present:

```
user_overlays=sentryusb-dwc3-hs
```

If it's missing or has a different value, edit `/boot/dietpiEnv.txt` and set `user_overlays=sentryusb-dwc3-hs`.

**Reboot:**

```bash
reboot
```

After reboot, continue from [Getting Started: SSH in and install](Getting-Started#4-ssh-in-and-install).

## Armbian

### 1. Flash the image

Open [Armbian Imager](https://www.armbian.com/#imager), select the Rock 4C+, choose the minimal image, and flash it to your SD card. Use a profile to pre-configure first-boot settings:

- **Enable Ethernet and WiFi**
- **WiFi SSID, password, and country**
- **Root password**
- **Create a first user** with username and password
- **Localization** (timezone, locale)

### 2. Boot and SSH in

Boot the board, find its IP on your router, and SSH in:

```bash
ssh <your-username>@<ip>
sudo -i
```

### 3. Install the dwc3 USB gadget overlay (required)

The OTG port defaults to **host mode**. Sentry USB needs it in **peripheral mode** to present as a USB mass-storage device the Tesla can read. This requires a device-tree overlay.

> Run these steps as root (`sudo -i`).

**Write the overlay source:**

```bash
cat > /boot/dtb/rockchip/overlay/dwc3-0-device.dts << 'EOF'
/dts-v1/;
/plugin/;
/ {
    compatible = "rockchip,rk3399";
    fragment@0 {
        target = <&usbdrd_dwc3_0>;
        __overlay__ {
            dr_mode = "peripheral";
            compatible = "snps,dwc3";
            reg = <0x0 0xfe800000 0x0 0x100000>;
            phys = <&tcphy0_usb3>;
            phy-names = "usb3-phy";
            phy_type = "utmi_wide";
            snps,dis_enblslpm_quirk;
            snps,dis-u2-freeclk-exists-quirk;
            snps,dis_u2_susphy_quirk;
            snps,dis-del-phy-power-chg-quirk;
            snps,xhci-slow-suspend-quirk;
        };
    };
};
EOF
```

**Compile and register:**

```bash
armbian-add-overlay /boot/dtb/rockchip/overlay/dwc3-0-device.dts
```

This compiles the `.dtbo` and adds it to `/boot/armbianEnv.txt` under `overlays=` automatically.

**Reboot:**

```bash
reboot
```

After reboot, continue from [Getting Started: SSH in and install](Getting-Started#4-ssh-in-and-install).