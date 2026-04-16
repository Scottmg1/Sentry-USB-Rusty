//! WiFi AP configuration — replaces `configure-ap.sh`.
//!
//! Sets up a concurrent AP on a virtual interface (ap0) using NetworkManager.

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::env::SetupEnv;

/// Configure the WiFi access point via NetworkManager.
pub async fn configure_ap(env: &SetupEnv, progress: &dyn Fn(&str)) -> Result<()> {
    let ssid = match env.config.get("AP_SSID") {
        Some(v) if !v.is_empty() => v.clone(),
        _ => {
            info!("AP_SSID not set, skipping AP configuration");
            return Ok(());
        }
    };

    let pass = match env.config.get("AP_PASS") {
        Some(v) if !v.is_empty() && v != "password" && v.len() >= 8 => v.clone(),
        _ => {
            bail!("AP_PASS not set, unchanged from default, or too short (min 8 chars)");
        }
    };

    progress(&format!("Configuring WiFi AP: {}", ssid));

    // Make sure NetworkManager is available
    if sentryusb_shell::run("which", &["nmcli"]).await.is_err() {
        bail!("NetworkManager (nmcli) not found — required for AP setup");
    }

    // Find the WiFi client device
    let wlan = find_wifi_device().await?;
    progress(&format!("WiFi client interface: {}", wlan));

    // Create virtual AP interface if it doesn't exist
    if sentryusb_shell::run("iw", &["dev", "ap0", "info"]).await.is_err() {
        sentryusb_shell::run("iw", &["dev", &wlan, "interface", "add", "ap0", "type", "__ap"]).await
            .context("failed to create ap0 virtual interface")?;
    }

    // Disable power save on both interfaces
    let _ = sentryusb_shell::run("iw", &[&wlan, "set", "power_save", "off"]).await;
    let _ = sentryusb_shell::run("iw", &["ap0", "set", "power_save", "off"]).await;

    // Remove old connections
    let _ = sentryusb_shell::run("nmcli", &["con", "delete", "SENTRYUSB_AP"]).await;
    let _ = sentryusb_shell::run("nmcli", &["con", "delete", "TESLAUSB_AP"]).await;

    // Create AP connection
    sentryusb_shell::run(
        "nmcli", &["con", "add", "type", "wifi", "ifname", "ap0", "mode", "ap",
                    "con-name", "SENTRYUSB_AP", "ssid", &ssid],
    ).await.context("nmcli con add failed")?;

    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP",
                    "802-11-wireless-security.key-mgmt", "wpa-psk"],
    ).await?;

    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP",
                    "802-11-wireless-security.psk", &pass],
    ).await?;

    let ip = env.get("AP_IP", "192.168.66.1");
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

    // Don't auto-start — Away Mode controls when it comes up
    sentryusb_shell::run(
        "nmcli", &["con", "modify", "SENTRYUSB_AP", "connection.autoconnect", "no"],
    ).await?;

    // Install NM dispatcher script to bring up AP when WiFi connects
    install_ap_dispatcher().await?;

    progress("WiFi AP configured.");
    Ok(())
}

/// Find the primary WiFi device from NetworkManager.
async fn find_wifi_device() -> Result<String> {
    for _ in 0..5 {
        let output = sentryusb_shell::run(
            "bash", &["-c", "nmcli -t -f TYPE,DEVICE c show --active | grep 802-11-wireless | grep -v ':ap0$' | cut -c17-"],
        ).await.unwrap_or_default();
        let wlan = output.trim().to_string();
        if !wlan.is_empty() {
            return Ok(wlan);
        }
        info!("Waiting for WiFi interface...");
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
    bail!("Could not determine WiFi client device");
}

/// Install a NetworkManager dispatcher script that brings up the AP.
async fn install_ap_dispatcher() -> Result<()> {
    let script = r#"#!/bin/bash
# Bring up SentryUSB AP when a WiFi client connection is activated.
IFACE="$1"
ACTION="$2"

if [ "$ACTION" != "up" ] && [ "$ACTION" != "connectivity-change" ]; then
    exit 0
fi

# Only act for WiFi connections (not ap0 itself)
if [ "$IFACE" = "ap0" ]; then
    exit 0
fi

# Check if this is a WiFi interface
if ! nmcli -t -f TYPE,DEVICE c show --active | grep "802-11-wireless:${IFACE}$" > /dev/null 2>&1; then
    exit 0
fi

# Bring up the AP if not already active
if ! nmcli -t c show --active | grep -q "SENTRYUSB_AP"; then
    sleep 2
    nmcli con up SENTRYUSB_AP 2>/dev/null || true
fi
"#;

    let dispatcher_dir = "/etc/NetworkManager/dispatcher.d";
    let _ = std::fs::create_dir_all(dispatcher_dir);
    let path = format!("{}/99-sentryusb-ap", dispatcher_dir);
    std::fs::write(&path, script)?;
    sentryusb_shell::run("chmod", &["+x", &path]).await?;

    Ok(())
}
