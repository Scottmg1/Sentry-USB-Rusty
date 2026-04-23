//! WiFi AP configuration — port of `configure-ap.sh`.
//!
//! Sets up a concurrent AP on a virtual interface (ap0). Tries NetworkManager
//! first, falls back to writing the .nmconnection keyfile directly (needed
//! when NM was started on a read-only root and its keyfile plugin refuses
//! `nmcli con add`), and finally falls back to wpa_supplicant + hostapd on
//! systems without NetworkManager.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::env::SetupEnv;
use crate::SetupEmitter;

/// Configure the WiFi access point.
///
/// The runner gates this on `AP_SSID` and a valid `AP_PASS` being set, so by
/// the time we get here both are populated. We still defend against missing
/// values in case this is called directly.
pub async fn configure_ap(env: &SetupEnv, emitter: &SetupEmitter) -> Result<()> {
    let ssid = match env.config.get("AP_SSID") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            info!("AP_SSID not set, skipping AP configuration");
            return Ok(());
        }
    };

    let pass = match env.config.get("AP_PASS") {
        Some(v) if !v.is_empty() && v != "password" && v.len() >= 8 => v.clone(),
        _ => bail!("AP_PASS not set, unchanged from default, or too short (min 8 chars)"),
    };

    emitter.begin_phase("wifi_ap", "WiFi access point");
    emitter.progress(&format!("Configuring WiFi AP: {}", ssid));

    let ip = env.get("AP_IP", "192.168.66.1");

    // NetworkManager path (by far the most common on modern Pi OS / Trixie).
    if sentryusb_shell::run("systemctl", &["--quiet", "is-enabled", "NetworkManager.service"])
        .await
        .is_ok()
    {
        // Make sure `iw` is installed — it would otherwise get swept up by
        // autoremove when alsa-utils is removed in the readonly phase.
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(120),
            "apt-get", &["-y", "install", "iw"],
        ).await;

        match nm_add_ap(&ssid, &pass, &ip, emitter).await {
            Ok(()) => {
                emitter.progress("AP configured via NetworkManager.");
                return Ok(());
            }
            Err(e) => {
                info!("nmcli AP add failed ({e}); falling back to keyfile writer");
                // NM's keyfile plugin sometimes refuses `nmcli con add` when
                // NM was started while root was read-only. Writing the
                // .nmconnection file directly works because the keyfile
                // plugin re-reads it on `con reload` — and that doesn't
                // require the plugin to be healthy.
                nm_write_ap_file(&ssid, &pass, &ip, emitter).await
                    .context("failed to configure AP (both nmcli and keyfile paths failed)")?;
                emitter.progress("AP configured via keyfile fallback.");
                return Ok(());
            }
        }
    }

    // wpa_supplicant + hostapd path (non-NM systems).
    if !Path::new("/etc/wpa_supplicant/wpa_supplicant.conf").exists() {
        emitter.progress("No wpa_supplicant, skipping AP setup.");
        return Ok(());
    }
    let current = std::fs::read_to_string("/etc/wpa_supplicant/wpa_supplicant.conf")
        .unwrap_or_default();
    if current.contains("id_str") {
        emitter.progress("AP mode already configured");
        return Ok(());
    }
    configure_hostapd_path(&ssid, &pass, &ip, emitter).await
}

/// Primary NM path: `nmcli con add` + modifications.
async fn nm_add_ap(
    ssid: &str,
    pass: &str,
    ip: &str,
    emitter: &SetupEmitter,
) -> Result<()> {
    let wlan = find_wifi_device().await?;
    emitter.progress(&format!("WiFi client interface: {}", wlan));

    // Create virtual AP interface if it doesn't exist.
    if sentryusb_shell::run("iw", &["dev", "ap0", "info"]).await.is_err() {
        sentryusb_shell::run(
            "iw", &["dev", &wlan, "interface", "add", "ap0", "type", "__ap"],
        ).await.context("failed to create ap0 virtual interface")?;
    }

    // Disable power save on both interfaces (they share hardware, and we
    // don't want one to sleep just because the other is idle).
    let _ = sentryusb_shell::run("iw", &[&wlan, "set", "power_save", "off"]).await;
    let _ = sentryusb_shell::run("iw", &["ap0", "set", "power_save", "off"]).await;

    // Remove old / legacy connection names.
    let _ = sentryusb_shell::run("nmcli", &["con", "delete", "SENTRYUSB_AP"]).await;
    let _ = sentryusb_shell::run("nmcli", &["con", "delete", "TESLAUSB_AP"]).await;

    sentryusb_shell::run(
        "nmcli", &["con", "add", "type", "wifi", "ifname", "ap0", "mode", "ap",
                   "con-name", "SENTRYUSB_AP", "ssid", ssid],
    ).await.context("nmcli con add failed")?;

    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP",
                   "802-11-wireless-security.key-mgmt", "wpa-psk"],
    ).await?;
    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP",
                   "802-11-wireless-security.psk", pass],
    ).await?;
    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP",
                   "ipv4.addr", &format!("{}/24", ip)],
    ).await?;
    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP", "ipv4.method", "shared"],
    ).await?;
    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP", "ipv6.method", "disabled"],
    ).await?;
    // Don't auto-start — Away Mode brings the AP up only when enabled.
    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP", "connection.autoconnect", "no"],
    ).await?;

    // Clean up stale if-up.d script from previous installs.
    let _ = std::fs::remove_file("/etc/network/if-up.d/sentryusb-ap");

    install_ap_dispatcher(&wlan).await?;
    Ok(())
}

/// Fallback: write the connection file directly and `nmcli con reload`.
///
/// When NM's keyfile plugin started on a read-only root it refuses
/// `nmcli con add`, but once the FS is remounted rw we can write the
/// file ourselves. `nmcli con reload` picks it up without a full restart,
/// so SSH sessions survive.
async fn nm_write_ap_file(
    ssid: &str,
    pass: &str,
    ip: &str,
    emitter: &SetupEmitter,
) -> Result<()> {
    emitter.progress("Writing AP connection file directly (NM keyfile workaround)");

    let wlan = find_wifi_device().await?;

    if sentryusb_shell::run("iw", &["dev", "ap0", "info"]).await.is_err() {
        sentryusb_shell::run(
            "iw", &["dev", &wlan, "interface", "add", "ap0", "type", "__ap"],
        ).await.context("failed to create ap0 virtual interface")?;
    }
    let _ = sentryusb_shell::run("iw", &[&wlan, "set", "power_save", "off"]).await;
    let _ = sentryusb_shell::run("iw", &["ap0", "set", "power_save", "off"]).await;

    let _ = sentryusb_shell::run("nmcli", &["con", "delete", "SENTRYUSB_AP"]).await;
    let _ = sentryusb_shell::run("nmcli", &["con", "delete", "TESLAUSB_AP"]).await;

    std::fs::create_dir_all("/etc/NetworkManager/system-connections")?;
    let file = "/etc/NetworkManager/system-connections/SENTRYUSB_AP.nmconnection";
    let contents = format!(
        "[connection]\n\
         id=SENTRYUSB_AP\n\
         type=wifi\n\
         interface-name=ap0\n\
         autoconnect=false\n\
         \n\
         [wifi]\n\
         mode=ap\n\
         ssid={ssid}\n\
         \n\
         [wifi-security]\n\
         key-mgmt=wpa-psk\n\
         psk={pass}\n\
         \n\
         [ipv4]\n\
         address1={ip}/24\n\
         method=shared\n\
         \n\
         [ipv6]\n\
         method=disabled\n"
    );
    std::fs::write(file, contents)?;
    // NM's keyfile plugin enforces 0600 on connection files; enforce it too.
    let _ = sentryusb_shell::run("chmod", &["0600", file]).await;

    let _ = sentryusb_shell::run("nmcli", &["con", "reload"]).await;

    let _ = std::fs::remove_file("/etc/network/if-up.d/sentryusb-ap");
    install_ap_dispatcher(&wlan).await?;
    Ok(())
}

/// hostapd + dnsmasq path — for systems that don't use NetworkManager.
async fn configure_hostapd_path(
    ssid: &str,
    pass: &str,
    ip: &str,
    emitter: &SetupEmitter,
) -> Result<()> {
    emitter.progress("installing dnsmasq and hostapd");
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "apt-get", &["-y", "install", "dnsmasq", "hostapd"],
    ).await.context("failed to install hostapd/dnsmasq")?;

    emitter.progress(&format!("configuring AP '{ssid}' with IP {ip}"));

    let net = ip.rsplit_once('.').map(|(p, _)| p).unwrap_or("192.168.66");

    let mac = std::fs::read_to_string("/sys/class/net/wlan0/address")
        .unwrap_or_default();
    let mac = mac.trim();

    // udev rule — creates ap0 on hardware phy0 and pins its MAC.
    let udev_rule = format!(
        "SUBSYSTEM==\"ieee80211\", ACTION==\"add|change\", \
         ATTR{{macaddress}}==\"{mac}\", KERNEL==\"phy0\", \
         RUN+=\"/sbin/iw phy phy0 interface add ap0 type __ap\", \
         RUN+=\"/bin/ip link set ap0 address {mac}\"\n"
    );
    let _ = std::fs::create_dir_all("/etc/udev/rules.d");
    std::fs::write("/etc/udev/rules.d/70-persistent-net.rules", udev_rule)?;

    std::fs::write(
        "/etc/dnsmasq.conf",
        format!(
            "interface=lo,ap0\n\
             no-dhcp-interface=lo,wlan0\n\
             bind-interfaces\n\
             bogus-priv\n\
             dhcp-range={net}.10,{net}.254,12h\n\
             # don't configure a default route, we're not a router\n\
             dhcp-option=3\n"
        ),
    )?;

    let _ = std::fs::create_dir_all("/etc/hostapd");
    std::fs::write(
        "/etc/hostapd/hostapd.conf",
        format!(
            "ctrl_interface=/var/run/hostapd\n\
             ctrl_interface_group=0\n\
             interface=ap0\n\
             driver=nl80211\n\
             ssid={ssid}\n\
             hw_mode=g\n\
             channel=11\n\
             wmm_enabled=0\n\
             macaddr_acl=0\n\
             auth_algs=1\n\
             wpa=2\n\
             wpa_passphrase={pass}\n\
             wpa_key_mgmt=WPA-PSK\n\
             wpa_pairwise=TKIP CCMP\n\
             rsn_pairwise=CCMP\n"
        ),
    )?;

    std::fs::write(
        "/etc/default/hostapd",
        "DAEMON_CONF=\"/etc/hostapd/hostapd.conf\"\n",
    )?;

    std::fs::write(
        "/etc/network/interfaces",
        format!(
            "source-directory /etc/network/interfaces.d\n\
             \n\
             auto lo\n\
             auto ap0\n\
             auto wlan0\n\
             iface lo inet loopback\n\
             \n\
             allow-hotplug ap0\n\
             iface ap0 inet static\n\
             \x20\x20\x20\x20address {ip}\n\
             \x20\x20\x20\x20netmask 255.255.255.0\n\
             \x20\x20\x20\x20hostapd /etc/hostapd/hostapd.conf\n\
             \n\
             allow-hotplug wlan0\n\
             iface wlan0 inet manual\n\
             \x20\x20\x20\x20wpa-roam /etc/wpa_supplicant/wpa_supplicant.conf\n\
             iface AP1 inet dhcp\n"
        ),
    )?;

    // Bullseye needs wpa_supplicant explicitly disabled on ap0.
    let dhcpcd_line = "# disable wpa_supplicant for the ap0 interface\n\
                       interface ap0\n\
                       nohook wpa_supplicant\n";
    append_unless_contains("/etc/dhcpcd.conf", "nohook wpa_supplicant", dhcpcd_line)?;

    // Migrate /var/lib/misc to /mutable so the dnsmasq lease file persists
    // across reboots when root is read-only.
    if !Path::new("/var/lib/misc").is_symlink() {
        if sentryusb_shell::run("findmnt", &["--mountpoint", "/mutable"])
            .await
            .is_err()
        {
            let _ = sentryusb_shell::run("mount", &["/mutable"]).await;
        }
        let _ = std::fs::create_dir_all("/mutable/varlib");
        let _ = sentryusb_shell::run("mv", &["/var/lib/misc", "/mutable/varlib/"]).await;
        #[cfg(unix)]
        let _ = std::os::unix::fs::symlink("/mutable/varlib/misc", "/var/lib/misc");
    }

    // Update hosts so the sentryusb mDNS name resolves to the AP IP rather
    // than 127.0.0.1 for clients connected to the AP.
    if let Ok(hosts) = std::fs::read_to_string("/etc/hosts") {
        let new: String = hosts
            .lines()
            .map(|l| {
                if l.trim_start().starts_with("127.0.0.1")
                    && !l.trim_start().starts_with("127.0.0.1\tlocalhost")
                    && !l.trim_start().starts_with("127.0.0.1 localhost")
                {
                    // Replace 127.0.0.1 prefix with the AP IP; keep the rest.
                    let rest = l
                        .trim_start()
                        .strip_prefix("127.0.0.1")
                        .unwrap_or(l);
                    format!("{}{}", ip, rest)
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write("/etc/hosts", new + "\n")?;
    }

    // Tag the wpa_supplicant block with the id_str the ifupdown config
    // maps to AP1.
    if let Ok(conf) = std::fs::read_to_string("/etc/wpa_supplicant/wpa_supplicant.conf") {
        let new = conf.replace("}", "  id_str=\"AP1\"\n}");
        std::fs::write("/etc/wpa_supplicant/wpa_supplicant.conf", new)?;
    }

    emitter.progress("AP configured via hostapd/wpa_supplicant");
    Ok(())
}

/// Find the primary WiFi client device from NetworkManager.
async fn find_wifi_device() -> Result<String> {
    for _ in 0..5 {
        let output = sentryusb_shell::run(
            "bash",
            &[
                "-c",
                "nmcli -t -f TYPE,DEVICE c show --active | grep 802-11-wireless | grep -v ':ap0$' | cut -c17-",
            ],
        ).await.unwrap_or_default();
        let wlan = output.trim().to_string();
        if !wlan.is_empty() {
            return Ok(wlan);
        }
        info!("Waiting for WiFi interface...");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    bail!("Could not determine WiFi client device")
}

/// Install the NM dispatcher that activates the AP on Away Mode events.
async fn install_ap_dispatcher(wlan: &str) -> Result<()> {
    // Dispatcher only brings the AP up when the away-mode flag file exists.
    // During normal operation the AP stays off so wlan0 can scan freely.
    let script = format!(
        "#!/bin/bash\n\
         # Recreate ap0 virtual interface when the wifi client comes up,\n\
         # but ONLY if Away Mode is active (flag file exists).\n\
         # Created by SentryUSB configure-ap.\n\
         \n\
         IFACE=\"$1\"\n\
         ACTION=\"$2\"\n\
         \n\
         if [ \"$IFACE\" = \"{wlan}\" ] && [ \"$ACTION\" = \"up\" ]\n\
         then\n\
         \x20\x20if [ -f /mutable/sentryusb_away_mode.json ]; then\n\
         \x20\x20\x20\x20if ! iw dev ap0 info &> /dev/null; then\n\
         \x20\x20\x20\x20\x20\x20iw dev {wlan} interface add ap0 type __ap || true\n\
         \x20\x20\x20\x20fi\n\
         \x20\x20\x20\x20iw {wlan} set power_save off 2>/dev/null || true\n\
         \x20\x20\x20\x20iw ap0 set power_save off 2>/dev/null || true\n\
         \x20\x20\x20\x20nmcli con up SENTRYUSB_AP 2>/dev/null || true\n\
         \x20\x20fi\n\
         fi\n"
    );

    let dispatcher_dir = "/etc/NetworkManager/dispatcher.d";
    std::fs::create_dir_all(dispatcher_dir)?;
    let path = format!("{}/10-sentryusb-ap", dispatcher_dir);
    std::fs::write(&path, script)?;
    let _ = sentryusb_shell::run("chmod", &["755", &path]).await;
    Ok(())
}

fn append_unless_contains(path: &str, needle: &str, text: &str) -> Result<()> {
    let current = std::fs::read_to_string(path).unwrap_or_default();
    if current.contains(needle) {
        return Ok(());
    }
    let mut new = current;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(text);
    std::fs::write(path, new)?;
    Ok(())
}
