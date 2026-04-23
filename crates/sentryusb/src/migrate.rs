//! Startup migration: update peripheral files (shell scripts, BLE daemon,
//! Avahi service, etc.) when the binary has been replaced by a newer version
//! but the surrounding artifacts on disk are stale. Port of server/migrate.go.
//!
//! This solves the bootstrap problem for existing installs whose Rust binary
//! was updated via a minimal replace-only update path — their scripts, BLE
//! daemon, and service files were left at the old version. Once this code
//! has run once, future boots will self-heal automatically.
//!
//! Gated by a marker file (`/opt/sentryusb/.migrated-<version>`) so it runs
//! at most once per installed version. Never touches user setup configuration.

use std::time::Duration;

use tracing::{info, warn};

const VERSION_FILE: &str = "/opt/sentryusb/version";
const MIGRATE_DIR: &str = "/opt/sentryusb";
const MIGRATE_REPO: &str = "Scottmg1/Sentry-USB-Rusty";
const MIGRATE_BRANCH: &str = "main";

pub async fn run_startup_migration() {
    // Skip in dev mode (no version file, or explicit "dev")
    let current_version = match tokio::fs::read_to_string(VERSION_FILE).await {
        Ok(v) => v.trim().to_string(),
        Err(_) => return,
    };
    if current_version.is_empty() || current_version == "dev" {
        return;
    }

    let marker_file = format!("{}/.migrated-{}", MIGRATE_DIR, current_version);
    if tokio::fs::metadata(&marker_file).await.is_ok() {
        return;
    }

    info!("[migrate] Running startup migration for {}...", current_version);

    // Prefer the exact version tag; fall back to the tracking branch if missing.
    let script_ref = if current_version == "unknown" {
        MIGRATE_BRANCH.to_string()
    } else {
        current_version.clone()
    };
    let tarball_url = format!(
        "https://github.com/{}/archive/{}.tar.gz",
        MIGRATE_REPO, script_ref
    );

    let script = build_migration_script(&tarball_url);

    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(180),
        "bash",
        &["-c", &script],
    )
    .await
    {
        Ok(_) => {
            let _ = tokio::fs::create_dir_all(MIGRATE_DIR).await;
            if let Err(e) = tokio::fs::write(&marker_file, b"migrated\n").await {
                warn!("[migrate] Failed to write marker {}: {}", marker_file, e);
            }
            info!("[migrate] Startup migration complete for {}", current_version);
        }
        Err(e) => {
            warn!("[migrate] Warning: startup migration failed: {}", e);
            // Don't write marker — retry on next boot.
        }
    }
}

fn build_migration_script(tarball_url: &str) -> String {
    format!(
        r#"set -e

# Remount filesystem as read-write (no-op if already rw)
/root/bin/remountfs_rw 2>/dev/null || mount -o remount,rw / 2>/dev/null || true

# Ensure /root/bin exists — on fresh Rust installs it isn't created by setup,
# so cp targets below would otherwise fail.
mkdir -p /root/bin

TMPDIR=$(mktemp -d)
trap "rm -rf $TMPDIR" EXIT

# Download repo tarball — try version tag first, fall back to tracking branch
if ! curl -fsSL "{tarball_url}" | tar xz --strip-components=1 -C "$TMPDIR" 2>/dev/null; then
  FALLBACK="https://github.com/{repo}/archive/{branch}.tar.gz"
  curl -fsSL "$FALLBACK" | tar xz --strip-components=1 -C "$TMPDIR" 2>/dev/null || exit 1
fi

# ── Update run/ scripts ──
if [ -d "$TMPDIR/run" ]; then
  for f in "$TMPDIR"/run/*; do
    [ -f "$f" ] || continue
    name=$(basename "$f")
    cp "$f" "/root/bin/$name"
    chmod +x "/root/bin/$name"
  done
fi

# ── Update archive module scripts ──
ARCHIVE_SYSTEM=""
for conf in /root/sentryusb.conf /sentryusb/sentryusb.conf; do
  if [ -f "$conf" ]; then
    ARCHIVE_SYSTEM=$(grep -m1 'ARCHIVE_SYSTEM=' "$conf" 2>/dev/null | tail -1 | sed "s/.*ARCHIVE_SYSTEM=//;s/['\"]//g;s/#.*//" | tr -d ' ') || true
    [ -n "$ARCHIVE_SYSTEM" ] && break
  fi
done
if [ -n "$ARCHIVE_SYSTEM" ]; then
  subdir="${{ARCHIVE_SYSTEM}}_archive"
  if [ -d "$TMPDIR/run/$subdir" ]; then
    for f in "$TMPDIR/run/$subdir"/*; do
      [ -f "$f" ] || continue
      name=$(basename "$f")
      cp "$f" "/root/bin/$name"
      chmod +x "/root/bin/$name"
    done
  fi
fi

# ── Update setup-sentryusb (kept as compatibility wrapper) ──
if [ -f "$TMPDIR/setup/pi/setup-sentryusb" ]; then
  cp "$TMPDIR/setup/pi/setup-sentryusb" "/root/bin/setup-sentryusb"
  chmod +x "/root/bin/setup-sentryusb"
fi

# ── Update envsetup.sh (kept as compatibility wrapper) ──
if [ -f "$TMPDIR/setup/pi/envsetup.sh" ]; then
  cp "$TMPDIR/setup/pi/envsetup.sh" "/root/bin/envsetup.sh"
  chmod +x "/root/bin/envsetup.sh"
fi

# ── Update BLE peripheral daemon (binary and/or Python fallback) ──
if [ -f "$TMPDIR/server/ble/sentryusb-ble.py" ]; then
  cp "$TMPDIR/server/ble/sentryusb-ble.py" "/root/bin/sentryusb-ble.py"
  chmod +x "/root/bin/sentryusb-ble.py"
fi
if [ -f "$TMPDIR/server/ble/sentryusb-ble.service" ]; then
  cp "$TMPDIR/server/ble/sentryusb-ble.service" "/etc/systemd/system/sentryusb-ble.service"
  systemctl daemon-reload
fi

# ── Install BLE Python dependencies if missing ──
for pkg in python3-dbus python3-gi bluez; do
  if ! dpkg-query -W --showformat='${{db:Status-Status}}\n' "$pkg" 2>/dev/null | grep -q '^installed$'; then
    DEBIAN_FRONTEND=noninteractive apt-get -y --force-yes install "$pkg" 2>/dev/null || true
  fi
done

# ── Ensure bluetoothd --experimental override ──
if [ ! -f /etc/systemd/system/bluetooth.service.d/sentryusb-experimental.conf ]; then
  BTDAEMON=$(systemctl cat bluetooth.service 2>/dev/null | grep '^ExecStart=' | head -1 | sed 's/ExecStart=//' | awk '{{print $1}}')
  BTDAEMON=${{BTDAEMON:-$(command -v bluetoothd 2>/dev/null)}}
  if [ -n "$BTDAEMON" ] && [ -x "$BTDAEMON" ]; then
    mkdir -p /etc/systemd/system/bluetooth.service.d
    cat > /etc/systemd/system/bluetooth.service.d/sentryusb-experimental.conf << BTEOF
[Service]
ExecStart=
ExecStart=$BTDAEMON --experimental
BTEOF
    systemctl daemon-reload
    systemctl restart bluetooth 2>/dev/null || true
    sleep 2
  fi
fi

# ── Install/update Avahi mDNS service ──
if [ -f "$TMPDIR/setup/pi/avahi-sentryusb.service" ]; then
  if ! dpkg -s avahi-daemon >/dev/null 2>&1; then
    apt-get update -qq && apt-get install -y -qq avahi-daemon avahi-utils >/dev/null 2>&1 || true
  fi
  if dpkg -s avahi-daemon >/dev/null 2>&1; then
    mkdir -p /etc/avahi/services
    cp "$TMPDIR/setup/pi/avahi-sentryusb.service" /etc/avahi/services/sentryusb.service
    systemctl enable avahi-daemon 2>/dev/null || true
    systemctl restart avahi-daemon 2>/dev/null || true
  fi
fi

# ── Migrate AP to Away Mode (AP off by default) ──
if nmcli -t con show SENTRYUSB_AP &>/dev/null; then
  nmcli con modify SENTRYUSB_AP connection.autoconnect no 2>/dev/null || true
  nmcli con down SENTRYUSB_AP 2>/dev/null || true
  iw dev ap0 del 2>/dev/null || true
fi
WLAN=$(nmcli -t -f TYPE,DEVICE c show --active 2>/dev/null | grep 802-11-wireless | grep -v ':ap0$' | cut -d: -f2 | head -1)
WLAN=${{WLAN:-wlan0}}
if [ -d /etc/NetworkManager/dispatcher.d ]; then
  cat > /etc/NetworkManager/dispatcher.d/10-sentryusb-ap << APEOF
#!/bin/bash
IFACE="\$1"
ACTION="\$2"
if [ "\$IFACE" = "$WLAN" ] && [ "\$ACTION" = "up" ]; then
  if [ -f /mutable/sentryusb_away_mode.json ]; then
    if ! iw dev ap0 info &> /dev/null; then
      iw dev $WLAN interface add ap0 type __ap || true
    fi
    iw $WLAN set power_save off 2>/dev/null || true
    iw ap0 set power_save off 2>/dev/null || true
    nmcli con up SENTRYUSB_AP 2>/dev/null || true
  fi
fi
APEOF
  chmod 755 /etc/NetworkManager/dispatcher.d/10-sentryusb-ap
fi

# ── Restart BLE daemon ──
systemctl enable sentryusb-ble 2>/dev/null || true
systemctl restart sentryusb-ble 2>/dev/null || true

# ── Post-migration patches (persist across upstream script updates) ──
# These heal existing installs whose run/ scripts above were just replaced
# with upstream copies that don't yet carry the user-facing fixes shipped
# in PRs #31 / #35. Idempotent — the `grep -q` guards prevent re-patching.

# Patch 1: send-push-message — respect SENTRY_NOTIFICATION_URL (PR #31)
if grep -q 'https://notifications.sentry-six.com/send"' /root/bin/send-push-message 2>/dev/null; then
  sed -i 's|"https://notifications.sentry-six.com/send"|"${{SENTRY_NOTIFICATION_URL:-https://notifications.sentry-six.com}}/send"|' /root/bin/send-push-message
fi

# Patch 2: archiveloop — read active chime from library dir instead of flat file (PR #35)
if grep -q '[ -f "/mutable/LockChime.wav" ]' /root/bin/archiveloop 2>/dev/null; then
  python3 - <<'PYEOF'
content = open('/root/bin/archiveloop').read()
old = '    if [ -f "/mutable/LockChime.wav" ]\n    then\n      cp -f "/mutable/LockChime.wav" "$CAM_MOUNT/LockChime.wav"'
new = '    _active_chime=$(cat /mutable/LockChime/.active_name 2>/dev/null || true)\n    if [ -n "$_active_chime" ] && [ -f "/mutable/LockChime/$_active_chime" ]\n    then\n      cp -f "/mutable/LockChime/$_active_chime" "$CAM_MOUNT/LockChime.wav"'
if old in content:
    open('/root/bin/archiveloop','w').write(content.replace(old, new, 1))
PYEOF
fi
"#,
        tarball_url = tarball_url,
        repo = MIGRATE_REPO,
        branch = MIGRATE_BRANCH
    )
}
