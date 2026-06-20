//! Startup migration: update peripheral files (shell scripts, BLE daemon,
//! Avahi service, etc.) when the binary has been replaced by a newer version
//! but the surrounding artifacts on disk are stale.
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
const MIGRATE_REPO: &str = "Sentry-Six/Sentry-USB-Rusty";
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

    // Retry up to 3 times with exponential backoff. The script itself
    // fails fast on `curl: Could not resolve host: github.com` when DNS
    // isn't ready yet — and that's exactly the state we hit racing
    // network-online.target at boot. Every retry is a full script run;
    // `set -e` + idempotent file writes mean re-running after a partial
    // success is safe (existing files get overwritten with identical
    // bytes from the tarball).
    //
    // The `nss-lookup.target` dependency on the service unit is the
    // primary fix; this is belt-and-suspenders for the edge case where
    // the resolver comes up but its first query fails because the
    // upstream DNS cache is cold.
    let mut last_err: Option<String> = None;
    for attempt in 1..=3 {
        match sentryusb_shell::run_with_timeout(
            Duration::from_secs(180),
            "bash",
            &["-c", &script],
        )
        .await
        {
            Ok(_) => {
                // Re-apply runtime patches AFTER the migration. The migration
                // script unconditionally rewrites /root/bin/sentryusb-ble.py
                // from the upstream tarball — which silently undoes board-
                // specific fixes the OTA updater already applied (BCM4345C0
                // non-fatal-adv on Rock 4C+ is the headline case: OTA patches
                // ble.py → reboot → this migration unpatches it → BLE crash
                // loop on next start). Invoke the standalone runtime-patches
                // script (idempotent + detection-gated) so the patches survive
                // every migration. Best-effort: a missing script just yields
                // an info log — the OTA flow's bootstrap path will populate
                // it on the next update.
                if std::path::Path::new("/usr/local/bin/sentryusb-apply-runtime-patches").exists() {
                    match sentryusb_shell::run_with_timeout(
                        Duration::from_secs(30),
                        "/usr/local/bin/sentryusb-apply-runtime-patches",
                        &[],
                    )
                    .await
                    {
                        Ok(_) => info!("[migrate] runtime-patches re-applied post-migration"),
                        Err(e) => warn!(
                            "[migrate] runtime-patches post-migration run failed: {} — BLE pairing may be broken on Rock 4C+",
                            e
                        ),
                    }
                } else {
                    info!("[migrate] runtime-patches script not present (pre-bootstrap install) — skipping; OTA path will populate it");
                }

                let _ = tokio::fs::create_dir_all(MIGRATE_DIR).await;
                if let Err(e) = tokio::fs::write(&marker_file, b"migrated\n").await {
                    warn!("[migrate] Failed to write marker {}: {}", marker_file, e);
                }
                info!("[migrate] Startup migration complete for {}", current_version);
                return;
            }
            Err(e) => {
                let msg = e.to_string();
                // Only retry for transient failure signatures. A genuine
                // 404 on the tarball URL, a write permission error, or
                // an archive-corrupt error isn't going to fix itself on
                // a second try, and we don't want to burn 30+ seconds
                // on guaranteed-failing retries.
                let transient = msg.contains("Could not resolve host")
                    || msg.contains("Temporary failure in name resolution")
                    || msg.contains("Connection timed out")
                    || msg.contains("Network is unreachable");
                if attempt < 3 && transient {
                    let wait = Duration::from_secs(5 * attempt as u64);
                    warn!(
                        "[migrate] Startup migration attempt {}/3 hit a transient failure ({}); retrying in {:?}",
                        attempt, msg, wait
                    );
                    tokio::time::sleep(wait).await;
                    last_err = Some(msg);
                    continue;
                }
                last_err = Some(msg);
                break;
            }
        }
    }
    warn!(
        "[migrate] Warning: startup migration failed after retries: {}",
        last_err.as_deref().unwrap_or("unknown")
    );
    // Don't write marker — retry on next boot.
}

fn build_migration_script(tarball_url: &str) -> String {
    format!(
        r#"set -e

# Remount filesystem as read-write (no-op if already rw)
/root/bin/remountfs_rw 2>/dev/null || mount -o remount,rw / 2>/dev/null || true

# Ensure /root/bin exists — on fresh Rust installs it isn't created by setup,
# so cp targets below would otherwise fail.
mkdir -p /root/bin

# ── Migrate broken cttseraser FUSE fstab entry to bind mount ──
# Installs that completed setup before the FUSE → bind-mount switch still
# carry the legacy `mount.ctts#/mutable/TeslaCam …` fstab line, which
# depends on a /sbin/mount.ctts helper and the cttseraser binary —
# both removed in the bind-mount switch. With the helper gone the mount
# unit fails on every boot and /var/www/html/TeslaCam stays empty,
# returning 404 for every clip in the dashboard player. The setup
# wizard rewrites the fstab on first run, but already-set-up installs
# never re-run it, so they need this one-shot migration instead.
# Idempotent: skipped entirely when no `mount.ctts#` entry exists.
if grep -qE '^[^#]*mount\.ctts#' /etc/fstab 2>/dev/null; then
  # Strip the legacy entry AND any prior bind-mount line targeting the
  # same path, then append the canonical bind entry. awk + mv is atomic
  # so a power loss mid-write leaves /etc/fstab in its previous state.
  awk '
    /^[^#]*mount\.ctts#/ {{ next }}
    /^[^#]*\/var\/www\/html\/TeslaCam[[:space:]]+none[[:space:]]+bind/ {{ next }}
    {{ print }}
  ' /etc/fstab > /etc/fstab.new
  echo '/mutable/TeslaCam /var/www/html/TeslaCam none bind,nofail,x-systemd.requires=/mutable 0 0' >> /etc/fstab.new
  mv /etc/fstab.new /etc/fstab
  systemctl daemon-reload 2>/dev/null || true
  # Clear the failed state from the legacy FUSE mount unit so the new
  # bind-mount unit (same name, generated from the new fstab entry by
  # systemd-fstab-generator) can activate without the residual error.
  systemctl reset-failed var-www-html-TeslaCam.mount 2>/dev/null || true
  systemctl start var-www-html-TeslaCam.mount 2>/dev/null || true
  echo "migrate: replaced legacy mount.ctts# fstab entry with bind mount"
fi

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
    DEBIAN_FRONTEND=noninteractive apt-get -y install "$pkg" 2>/dev/null || true
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

# ── Refresh the per-CPU binary picker ──
# The picker now also re-validates the telemetry + ble-action symlinks
# under /root/bin at every boot (the issue #88 SIGILL self-heal). Ship
# the new version to existing installs from the tarball. Harmless on
# very old installs whose sentryusb.service predates the ExecStartPre
# hook — the script just sits unused until the unit is updated.
if [ -f "$TMPDIR/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb-pick-binary" ]; then
  install -m 755 "$TMPDIR/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb-pick-binary" /usr/local/bin/sentryusb-pick-binary
fi

# ── Install Tesla BLE telemetry sampler service + aux binaries ──
# Two concerns here:
#   1. The systemd unit ships in the source tarball — copy it into place
#      and enable it. Idempotent on every upgrade.
#   2. The telemetry + ble-action binaries need to be on disk in the
#      picker-managed layout: per-CPU variant copies under
#      /opt/sentryusb with /root/bin/<name> as symlinks the picker
#      re-validates at every boot (issue #88). A legacy regular file at
#      /root/bin is converted by re-downloading the right variant — its
#      own variant is unknowable (it may be the wrong-CPU build that
#      caused the SIGILL crash-loop in the first place), so we never
#      trust it. The old file is only replaced after a successful
#      download; offline installs keep working and convert on a later
#      run. Already-converted installs (symlink present) are skipped.
if [ -f "$TMPDIR/server/ble/sentryusb-telemetry.service" ]; then
  cp "$TMPDIR/server/ble/sentryusb-telemetry.service" "/etc/systemd/system/sentryusb-telemetry.service"
  systemctl daemon-reload
  systemctl enable sentryusb-telemetry 2>/dev/null || true

  # Match the suffix scheme update.rs uses. Three-tier: active-variant
  # file (written by sentryusb-pick-binary) → live CPU detection → arch
  # family fallback. The aarch64 suffix is per-CPU (-a53/-a72/-a76) so
  # the aux binaries' tuning matches the main daemon's tuning.
  # Only trust active-variant when it's a real release suffix — old
  # pickers recorded their on-disk fallback (or "legacy") here, and a
  # download URL built from that installs the wrong CPU variant.
  _suffix=""
  if [ -s /opt/sentryusb/active-variant ]; then
    _suffix=$(cat /opt/sentryusb/active-variant 2>/dev/null | tr -d '[:space:]')
    case "$_suffix" in
      linux-arm64-a53|linux-arm64-a72|linux-arm64-a76|linux-armv7|linux-amd64) ;;
      *) _suffix="" ;;
    esac
  fi
  if [ -z "$_suffix" ]; then
    _arch=$(dpkg --print-architecture 2>/dev/null || true)
    case "$_arch" in
      armhf)  _suffix=linux-armv7 ;;
      amd64)  _suffix=linux-amd64 ;;
      arm64)
        # Sub-detect for aarch64: HWCAP atomics+aes → a76, CPU part 0xD08
        # → a72, else a53. Same rules as sentryusb-pick-binary (the a76
        # build keeps the crypto extension, so it needs the aes hwcap).
        if grep -qE '^Features.*\batomics\b' /proc/cpuinfo 2>/dev/null \
          && grep -qE '^Features.*\baes\b' /proc/cpuinfo 2>/dev/null; then
          _suffix=linux-arm64-a76
        elif grep -qE '^CPU part[[:space:]]*:[[:space:]]*0x[dD]08' /proc/cpuinfo 2>/dev/null; then
          _suffix=linux-arm64-a72
        else
          _suffix=linux-arm64-a53
        fi
        ;;
      *)
        _arch=$(uname -m 2>/dev/null || echo "")
        case "$_arch" in
          armv7l)  _suffix=linux-armv7 ;;
          x86_64)  _suffix=linux-amd64 ;;
          aarch64)
            if grep -qE '^Features.*\batomics\b' /proc/cpuinfo 2>/dev/null \
              && grep -qE '^Features.*\baes\b' /proc/cpuinfo 2>/dev/null; then
              _suffix=linux-arm64-a76
            elif grep -qE '^CPU part[[:space:]]*:[[:space:]]*0x[dD]08' /proc/cpuinfo 2>/dev/null; then
              _suffix=linux-arm64-a72
            else
              _suffix=linux-arm64-a53
            fi
            ;;
          *)       _suffix="" ;;
        esac
        ;;
    esac
  fi
  if [ -n "$_suffix" ]; then
    for _name in sentryusb-tesla-telemetry sentryusb-ble-action; do
      # Symlink already in place → the picker owns it now; skip.
      if [ -L "/root/bin/$_name" ]; then
        continue
      fi
      _url="https://github.com/{repo}/releases/latest/download/$_name-$_suffix"
      if curl -sfI --max-time 10 "$_url" >/dev/null 2>&1; then
        mkdir -p /root/bin /opt/sentryusb
        if curl -fsSL --max-time 120 "$_url" -o "/tmp/$_name.migrate" 2>/dev/null; then
          chmod +x "/tmp/$_name.migrate"
          mv "/tmp/$_name.migrate" "/opt/sentryusb/$_name-$_suffix"
          ln -sfn "/opt/sentryusb/$_name-$_suffix" "/root/bin/$_name"
          echo "migrate: installed $_name-$_suffix and linked /root/bin/$_name"
        fi
      fi
    done
  fi

  # Only attempt restart if the binary is actually present — the
  # ConditionPathExists in the unit would otherwise log a confusing
  # "skipped" line on every upgrade where BLE isn't set up yet.
  if [ -x /root/bin/sentryusb-tesla-telemetry ]; then
    systemctl restart sentryusb-telemetry 2>/dev/null || true
  fi
fi

# Clean up the pre-v6 lock path if it's still around from an older
# awake_start. The new path is /tmp/ble_radio_owner; leaving the old
# file lying around looks like a held lock to the new code.
rm -f /tmp/ble_keep_awake_active 2>/dev/null || true

# ── Restart BLE daemon ──
systemctl enable sentryusb-ble 2>/dev/null || true
systemctl restart sentryusb-ble 2>/dev/null || true

# ── Post-migration patches (persist across upstream script updates) ──
# These re-apply user-facing fixes onto the run/ scripts just replaced with
# upstream copies. Idempotent — the `grep -q` guards prevent re-patching.

# Patch 1: send-push-message — respect SENTRY_NOTIFICATION_URL
if grep -q 'https://notifications.sentry-six.com/send"' /root/bin/send-push-message 2>/dev/null; then
  sed -i 's|"https://notifications.sentry-six.com/send"|"${{SENTRY_NOTIFICATION_URL:-https://notifications.sentry-six.com}}/send"|' /root/bin/send-push-message
fi

# Patch 2: archiveloop — read active chime from library dir instead of flat file
if grep -q '[ -f "/mutable/LockChime.wav" ]' /root/bin/archiveloop 2>/dev/null; then
  python3 - <<'PYEOF'
content = open('/root/bin/archiveloop').read()
old = '    if [ -f "/mutable/LockChime.wav" ]\n    then\n      cp -f "/mutable/LockChime.wav" "$CAM_MOUNT/LockChime.wav"'
new = '    _active_chime=$(cat /mutable/LockChime/.active_name 2>/dev/null || true)\n    if [ -n "$_active_chime" ] && [ -f "/mutable/LockChime/$_active_chime" ]\n    then\n      cp -f "/mutable/LockChime/$_active_chime" "$CAM_MOUNT/LockChime.wav"'
if old in content:
    open('/root/bin/archiveloop','w').write(content.replace(old, new, 1))
PYEOF
fi

# ── Remove obsolete tesla-control / tesla-keygen ──
# Pairing, keygen and every Tesla BLE command are native now
# (sentryusb-ble-action + the tesla_ble crate); nothing references the
# external Go binaries anymore. They're ~28 MB of dead weight on the
# space-tight root partition. The telemetry unit copied + reloaded above
# no longer gates on tesla-control, so removing them can't break startup.
# Idempotent.
rm -f /root/bin/tesla-control /root/bin/tesla-keygen \
      /usr/local/bin/tesla-control /usr/local/bin/tesla-keygen 2>/dev/null || true
"#,
        tarball_url = tarball_url,
        repo = MIGRATE_REPO,
        branch = MIGRATE_BRANCH
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The migration script is a format!() template full of shell — a
    /// stray unescaped `{` or a typo'd quote renders a script that fails
    /// at runtime on every user's Pi. Render it and let bash parse it.
    #[test]
    fn migration_script_parses() {
        let script = build_migration_script(
            "https://github.com/Sentry-Six/Sentry-USB-Rusty/archive/v0.0.0.tar.gz",
        );
        // Placeholders must all have been substituted.
        assert!(!script.contains("{repo}"), "unsubstituted {{repo}}");
        assert!(!script.contains("{branch}"), "unsubstituted {{branch}}");
        assert!(!script.contains("{tarball_url}"), "unsubstituted {{tarball_url}}");

        let dir = std::env::temp_dir().join("sentryusb-migrate-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("migration.sh");
        std::fs::write(&path, &script).unwrap();
        let status = std::process::Command::new("bash")
            .arg("-n")
            .arg(&path)
            .status()
            .expect("bash not available");
        assert!(status.success(), "bash -n rejected the migration script");
    }
}
