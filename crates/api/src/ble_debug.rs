//! GET /api/logs/bluetooth — single-page BLE diagnostic dump.
//!
//! Lives under the existing /api/logs route prefix so it shows up
//! as a "Bluetooth" tab in the Logs UI without needing a new
//! rendering component. Returns text/plain — a structured report
//! split into sections that lets users (or support) see at a glance:
//!   * which adapter is in use
//!   * is the sampler service running, since when
//!   * what does observe() think the car is doing
//!   * latest state + body-controller sample ages
//!   * recent failure counts + the freshest journal lines
//!
//! Pulls everything live (sysfs, systemctl, filesystem mtime, the
//! telemetry DB, journalctl). No caching — each fetch reflects the
//! current moment.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::Local;

use crate::router::AppState;

const CAM_DISK_PATH: &str = "/backingfiles/cam_disk.bin";
const HISTORY_PATH: &str = "/mutable/sentryusb-ble.log";

pub async fn get_ble_debug(State(s): State<AppState>) -> Response {
    let mut out = String::with_capacity(4096);
    let now = unix_now();

    section(&mut out, "Service");
    write_service_status(&mut out).await;

    section(&mut out, "Adapter");
    write_adapter_status(&mut out);

    section(&mut out, "Car observation (drives the sampler's phase machine)");
    write_observation(&mut out, now);

    section(&mut out, "Sample database (last 10 minutes)");
    write_sample_db(&mut out, &s, now).await;

    section(&mut out, "Recent sampler journal (filtered)");
    write_journal(&mut out, 60).await;

    section(&mut out, "Per-minute history (last ~6 hours, /mutable/sentryusb-ble.log)");
    write_history(&mut out);

    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        out,
    )
        .into_response()
}

fn section(out: &mut String, title: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str("===== ");
    out.push_str(title);
    out.push_str(" =====\n");
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

async fn write_service_status(out: &mut String) {
    // Active state from systemctl. is-active prints "active" /
    // "inactive" / "failed" + exit code reflects the same.
    let active = tokio::process::Command::new("systemctl")
        .args(["is-active", "sentryusb-telemetry"])
        .output()
        .await
        .ok()
        .and_then(|o| {
            String::from_utf8(o.stdout).ok()
        })
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "<systemctl unavailable>".into());
    out.push_str("status: ");
    out.push_str(&active);
    out.push('\n');

    // Uptime since ActiveEnterTimestamp — humanize.
    if let Ok(o) = tokio::process::Command::new("systemctl")
        .args([
            "show",
            "sentryusb-telemetry",
            "-p",
            "ActiveEnterTimestamp",
            "--value",
        ])
        .output()
        .await
    {
        let ts = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !ts.is_empty() {
            out.push_str("started: ");
            out.push_str(&ts);
            out.push('\n');
        }
    }
}

fn write_adapter_status(out: &mut String) {
    // Mirrors the picker logic in ble.rs::adapter_source.
    if let Ok(entries) = std::fs::read_dir("/sys/class/bluetooth") {
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                if n.starts_with("hci") && !n.contains(':') {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        ids.sort();
        for id in ids {
            let label = match crate::ble::adapter_source(&id) {
                "onboard" => "Pi built-in (UART)",
                _ => "USB dongle",
            };
            let addr = std::fs::read_to_string(format!(
                "/sys/class/bluetooth/{id}/address"
            ))
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| "?".into());
            // Soft-blocked = rfkilled
            let blocked = std::fs::read_to_string(format!(
                "/sys/class/bluetooth/{id}/rfkill0/soft"
            ))
            .or_else(|_| {
                // rfkill index varies; just probe a few common ones
                (0..4)
                    .find_map(|i| {
                        std::fs::read_to_string(format!(
                            "/sys/class/bluetooth/{id}/rfkill{i}/soft"
                        ))
                        .ok()
                    })
                    .ok_or(std::io::Error::other(""))
            })
            .ok()
            .map(|s| s.trim().to_string());
            let blocked_label = match blocked.as_deref() {
                Some("1") => " [rfkill BLOCKED]",
                Some("0") => "",
                _ => "",
            };
            out.push_str(&format!(
                "  {} = {} ({}){}\n",
                id, label, addr, blocked_label
            ));
        }
    } else {
        out.push_str("  /sys/class/bluetooth missing — bluez not running?\n");
    }
    // Currently-configured adapter from sentryusb.conf.
    let configured = std::fs::read_to_string("/root/sentryusb.conf")
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.trim().strip_prefix("export BLE_ADAPTER=").map(|v| {
                    v.trim_matches(|c| c == '"' || c == '\'').to_string()
                }))
        })
        .unwrap_or_else(|| "<unset (defaults to hci0)>".into());
    out.push_str(&format!("configured BLE_ADAPTER: {}\n", configured));
}

fn write_observation(out: &mut String, now: i64) {
    // The sampler's phase machine reads observe() each tick. Its
    // output drives whether the parked-awake refresh fires every
    // 3 min (climate/charge) and every 30 min (TPMS), or whether
    // the sampler stays in body-controller-only mode.
    //
    // Source of truth: mtime of /backingfiles/cam_disk.bin (the
    // gadget LUN backing file). Tesla writes to it every ~60s while
    // the car is on (driving OR Sentry OR charging triggers).
    let mtime = std::fs::metadata(CAM_DISK_PATH)
        .and_then(|m| m.modified())
        .ok();
    match mtime {
        Some(t) => {
            let ts = t
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let age = (now - ts).max(0);
            let state = if age < 90 {
                "Awake (last write < 90s)"
            } else if age < 300 {
                "Idle  (90s..5m) — between thresholds; parked-awake refresh paused"
            } else {
                "Asleep (>5m) — body-controller polls only"
            };
            out.push_str(&format!(
                "cam_disk.bin last written: {}s ago\n",
                age
            ));
            out.push_str(&format!("derived car state: {}\n", state));
            out.push_str(
                "\n\
                 If the car is parked + Sentry/charging but state shows\n\
                 Idle/Asleep here, Tesla isn't writing dashcam clips\n\
                 frequently enough for the sampler to know the car is\n\
                 awake — the parked-awake refresh won't fire. This is\n\
                 the most common cause of 'why is my climate/battery\n\
                 stale for >3 min while parked-awake?'\n",
            );
        }
        None => {
            out.push_str(&format!(
                "cam_disk.bin missing or unreadable ({}). \
                 Sampler treats this as Asleep.\n",
                CAM_DISK_PATH
            ));
        }
    }
}

async fn write_sample_db(out: &mut String, s: &AppState, now: i64) {
    let store = s.drives.store.clone();
    let res = tokio::task::spawn_blocking(move || {
        store.with_read_conn(|conn| {
            let state_ts: Option<i64> = conn
                .query_row(
                    "SELECT ts FROM telemetry_samples WHERE source='state' \
                     ORDER BY ts DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .ok();
            let bc_ts: Option<i64> = conn
                .query_row(
                    "SELECT ts FROM telemetry_samples WHERE source='body_controller' \
                     ORDER BY ts DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .ok();
            let since = now - 600;
            let total: i64 = conn
                .query_row(
                    "SELECT count(*) FROM telemetry_samples WHERE ts >= ?1",
                    (since,),
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let state_n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM telemetry_samples \
                     WHERE ts >= ?1 AND source='state'",
                    (since,),
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let bc_n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM telemetry_samples \
                     WHERE ts >= ?1 AND source='body_controller'",
                    (since,),
                    |r| r.get(0),
                )
                .unwrap_or(0);
            (state_ts, bc_ts, total, state_n, bc_n)
        })
    })
    .await
    .ok();
    let (state_ts, bc_ts, total, state_n, bc_n) = res.unwrap_or((None, None, 0, 0, 0));
    out.push_str(&format!(
        "last state poll:           {}\n",
        format_age(state_ts, now),
    ));
    out.push_str(&format!(
        "last body-controller poll: {}\n",
        format_age(bc_ts, now),
    ));
    out.push_str(&format!(
        "samples last 10 min:       {} total  ({} state, {} body-controller)\n",
        total, state_n, bc_n,
    ));
}

fn format_age(ts: Option<i64>, now: i64) -> String {
    match ts {
        Some(t) => {
            let age = (now - t).max(0);
            if age < 60 {
                format!("{}s ago", age)
            } else if age < 3600 {
                format!("{}m {}s ago", age / 60, age % 60)
            } else {
                format!("{}h {}m ago", age / 3600, (age % 3600) / 60)
            }
        }
        None => "<never>".into(),
    }
}

/// Append the tail of /mutable/sentryusb-ble.log — the per-minute
/// status log written by the sampler's `diag_log` background task.
/// Lets the user scroll back through hours of state without keeping
/// a browser tab open.
fn write_history(out: &mut String) {
    // Tail ~400 lines (≈6+ hours at one line/min). Cheap to read
    // since the file rotates at 5 MB.
    match std::fs::read_to_string(HISTORY_PATH) {
        Ok(raw) => {
            let mut lines: Vec<&str> = raw.lines().collect();
            const MAX: usize = 400;
            let start = lines.len().saturating_sub(MAX);
            for line in lines.drain(start..) {
                out.push_str(line);
                out.push('\n');
            }
        }
        Err(_) => {
            out.push_str(
                "(no history yet — file appears on first sampler tick after install)\n",
            );
        }
    }
}

async fn write_journal(out: &mut String, lines: usize) {
    let cmd = tokio::process::Command::new("journalctl")
        .args([
            "-u",
            "sentryusb-telemetry",
            "-n",
            &lines.to_string(),
            "--no-pager",
            "--output=short-iso",
        ])
        .output()
        .await;
    match cmd {
        Ok(o) if o.status.success() => {
            let raw = String::from_utf8_lossy(&o.stdout);
            // Filter to the noisy-but-useful patterns: state-poll
            // success/fail summary lines, body-controller summary,
            // PersistentSession lifecycle, parked-awake refresh
            // attempts, scan/connect milestones.
            let interesting = [
                "state-poll:",
                "body-controller poll:",
                "PersistentSession:",
                "parked-awake",
                "scanning for Tesla",
                "found target vehicle",
                "connecting to vehicle GATT",
                "session-info from",
                "user_presence flipped",
                "dropping to body-controller",
                "resuming full state polls",
                "WARN",
                "ERROR",
            ];
            for line in raw.lines() {
                if interesting.iter().any(|p| line.contains(p)) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
        Ok(o) => {
            out.push_str(&format!(
                "journalctl exited {}: {}\n",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr).trim(),
            ));
        }
        Err(e) => {
            out.push_str(&format!("journalctl failed to spawn: {}\n", e));
        }
    }
}

// ---------------------------------------------------------------------------
// Bundle endpoint — `GET /api/logs/bluetooth/bundle`
//
// The on-screen dashboard content plus the SSH-only data a support
// session needs (full journal, bluetooth-stack journal, dmesg, hciconfig,
// rfkill, sysfs LE params, pairing fingerprint, full history), as one
// downloadable text blob.
// ---------------------------------------------------------------------------

/// GET /api/logs/bluetooth/bundle — comprehensive single-file BLE
/// diagnostic dump. Content-Disposition makes the browser save it
/// with a timestamped filename instead of rendering it.
pub async fn get_ble_bundle(State(s): State<AppState>) -> Response {
    let mut out = String::with_capacity(64 * 1024);
    let now = unix_now();

    bundle_header(&mut out);

    section(&mut out, "Service status (full)");
    write_systemctl_status_full(&mut out).await;

    section(&mut out, "Adapter status");
    write_adapter_status(&mut out);

    section(&mut out, "BLE LE parameters (sysfs — what we ask the kernel to use)");
    write_le_params(&mut out);

    section(
        &mut out,
        "Live BLE session status (from sampler's status file)",
    );
    write_live_session_status(&mut out);

    section(
        &mut out,
        "Per-connection debugfs (negotiated params if exposed by your kernel)",
    );
    write_per_connection_debugfs(&mut out);

    section(
        &mut out,
        "Persistent disconnect history (/mutable/sentryusb-ble-disconnects.log)",
    );
    write_disconnect_history(&mut out);

    section(&mut out, "hciconfig -a");
    write_hciconfig(&mut out).await;

    section(&mut out, "rfkill list");
    write_rfkill(&mut out).await;

    section(&mut out, "Tesla pairing state");
    write_pairing_state(&mut out);

    section(&mut out, "Radio lock state");
    write_lock_state(&mut out);

    section(&mut out, "sentryusb.conf BLE_* keys");
    write_conf_keys(&mut out);

    section(&mut out, "Car observation");
    write_observation(&mut out, now);

    section(&mut out, "Quiet-mode gate inputs (live, from sampler)");
    write_gate_state(&mut out);

    section(&mut out, "Sample database (last 1 hour)");
    write_sample_db_extended(&mut out, &s, now).await;

    section(&mut out, "dmesg (BLE/bluetooth lines)");
    write_dmesg_ble(&mut out).await;

    section(&mut out, "bluetoothd journal (current boot)");
    write_bluetoothd_journal(&mut out).await;

    section(&mut out, "Full sampler journal (unfiltered, last ~5000 lines)");
    write_journal_full(&mut out, 5000).await;

    section(&mut out, "Per-minute history (full file)");
    write_history_full(&mut out);

    let filename = format!(
        "sentryusb-ble-bundle-{}.txt",
        Local::now().format("%Y%m%d-%H%M%S")
    );

    // HeaderMap (not the tuple/array form) because Content-Disposition
    // carries a dynamic filename — the static-str tuple form won't
    // accept a String.
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    if let Ok(v) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        filename
    )) {
        headers.insert(header::CONTENT_DISPOSITION, v);
    }
    (StatusCode::OK, headers, out).into_response()
}

fn bundle_header(out: &mut String) {
    let now = Local::now().format("%Y-%m-%d %H:%M:%S %Z").to_string();
    let hostname = std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "<unknown>".into());
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(|x| x.to_string()))
        .and_then(|s| s.parse::<f64>().ok())
        .map(|secs| {
            let s = secs as u64;
            format!("{}d {}h {}m", s / 86400, (s % 86400) / 3600, (s % 3600) / 60)
        })
        .unwrap_or_else(|| "<unknown>".into());

    // Pi hardware model — distinguishes Pi 4 from Pi 5 etc. Lets us
    // correlate "this drop pattern only happens on Pi 5" type issues
    // when multiple testers send bundles. /proc/device-tree/model is
    // NUL-terminated, hence the trim_matches.
    let pi_model = std::fs::read_to_string("/proc/device-tree/model")
        .map(|s| s.trim_matches('\0').trim().to_string())
        .unwrap_or_else(|_| "<unknown>".into());

    // Kernel version — bluez and BLE behavior shifts noticeably across
    // kernel minor releases.
    let kernel = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "<unknown>".into());

    // bluetoothd version — bluez 5.66/5.72/etc handle LE link
    // parameters differently. `bluetoothd --version` prints just a
    // version string like "5.72".
    let bluez_ver = std::process::Command::new("bluetoothd")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "<unknown>".into());

    // Daemon binary mtime — tells us when the user last updated.
    // Useful for "did this issue start after a recent update?"
    let binary_mtime = std::fs::metadata("/root/bin/sentryusb-tesla-telemetry")
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH).ok()
        })
        .and_then(|d| chrono::DateTime::from_timestamp(d.as_secs() as i64, 0))
        .map(|dt| {
            dt.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S %Z")
                .to_string()
        })
        .unwrap_or_else(|| "<unknown>".into());

    // sentryusb-api version — matches the binary actually serving
    // this bundle (different crate from the sampler, but a useful
    // cross-check).
    let api_ver = env!("CARGO_PKG_VERSION");

    out.push_str(&format!(
        "SentryUSB BLE diagnostic bundle\n\
         generated:      {}\n\
         hostname:       {}\n\
         uptime:         {}\n\
         pi model:       {}\n\
         kernel:         {}\n\
         bluez:          {}\n\
         api crate ver:  {}\n\
         sampler binary: {} (mtime)\n",
        now, hostname, uptime, pi_model, kernel, bluez_ver, api_ver, binary_mtime
    ));
}

async fn write_systemctl_status_full(out: &mut String) {
    match tokio::process::Command::new("systemctl")
        .args(["status", "sentryusb-telemetry", "--no-pager"])
        .output()
        .await
    {
        Ok(o) => {
            out.push_str(&String::from_utf8_lossy(&o.stdout));
            let err = String::from_utf8_lossy(&o.stderr);
            if !err.trim().is_empty() {
                out.push_str("\n[stderr]\n");
                out.push_str(&err);
            }
        }
        Err(e) => out.push_str(&format!("systemctl failed: {}\n", e)),
    }
}

/// Verify the bluez LE parameter tune (set in the
/// sentryusb-telemetry.service ExecStartPre) actually applied. If
/// supervision_timeout still reads 72 (720ms default) instead of
/// 600 (6s), debugfs wasn't writable or the path was wrong — the
/// supervision-timeout fix didn't take effect for this user.
fn write_le_params(out: &mut String) {
    let mut any = false;
    if let Ok(entries) = std::fs::read_dir("/sys/kernel/debug/bluetooth") {
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with("hci") {
                continue;
            }
            any = true;
            out.push_str(&format!("[{}]\n", name));
            for key in [
                "supervision_timeout",
                "conn_min_interval",
                "conn_max_interval",
                "conn_latency",
                "adv_min_interval",
                "adv_max_interval",
            ] {
                let path = entry.path().join(key);
                let val = std::fs::read_to_string(&path)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| "<unreadable>".into());
                out.push_str(&format!("  {:<22} = {}\n", key, val));
            }
        }
    }
    if !any {
        out.push_str(
            "  /sys/kernel/debug/bluetooth/ is missing (debugfs not mounted?). \
             The ExecStartPre LE-tune writes are silently skipped in this\n  \
             environment — supervision_timeout will be the bluez default (~720ms)\n  \
             which may explain frequent in-drive disconnects.\n",
        );
    }
    out.push_str(
        "\nExpected after our ExecStartPre tune:\n  \
         supervision_timeout = 600 (6000ms, was 72/720ms default)\n  \
         conn_min_interval   = 12  (15ms, was 24/30ms)\n  \
         conn_max_interval   = 24  (30ms, was 40/50ms)\n  \
         conn_latency        = 0   (no events skipped)\n",
    );
}

async fn write_hciconfig(out: &mut String) {
    match tokio::process::Command::new("hciconfig").arg("-a").output().await {
        Ok(o) => out.push_str(&String::from_utf8_lossy(&o.stdout)),
        Err(e) => out.push_str(&format!("hciconfig failed: {}\n", e)),
    }
}

async fn write_rfkill(out: &mut String) {
    match tokio::process::Command::new("rfkill").arg("list").output().await {
        Ok(o) => out.push_str(&String::from_utf8_lossy(&o.stdout)),
        Err(e) => out.push_str(&format!("rfkill failed: {}\n", e)),
    }
}

/// Confirm the BLE keypair exists and when it was generated — file
/// metadata plus a SHA-256 fingerprint of the public key (never the key
/// material), enough to tell whether the user re-paired or is missing
/// the key.
fn write_pairing_state(out: &mut String) {
    let priv_path = "/root/.ble/key_private.pem";
    let pub_path = "/root/.ble/key_public.pem";
    for (label, path) in [
        ("private key", priv_path),
        ("public key", pub_path),
    ] {
        match std::fs::metadata(path) {
            Ok(m) => {
                let size = m.len();
                let mtime = m
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let mtime_str = chrono::DateTime::from_timestamp(mtime as i64, 0)
                    .map(|dt| {
                        dt.with_timezone(&Local)
                            .format("%Y-%m-%d %H:%M:%S %Z")
                            .to_string()
                    })
                    .unwrap_or_else(|| "<unparseable>".into());
                out.push_str(&format!(
                    "  {:<12} {} ({} bytes, mtime {})\n",
                    label, path, size, mtime_str
                ));
            }
            Err(_) => {
                out.push_str(&format!(
                    "  {:<12} {} MISSING\n",
                    label, path
                ));
            }
        }
    }
    // Public-key fingerprint — a stable identity marker. SHA-256 of
    // the SPKI PEM bytes is fine; the user can compare across
    // re-pairings to see whether the key actually changed. Truncated
    // to 8 bytes (16 hex chars + colons) — long enough to be
    // distinct, short enough to read at a glance.
    if let Ok(pem) = std::fs::read(pub_path) {
        let digest = ring::digest::digest(&ring::digest::SHA256, &pem);
        let fp: String = digest
            .as_ref()
            .iter()
            .take(8)
            .map(|b| format!("{:02x}", b))
            .collect::<Vec<_>>()
            .join(":");
        out.push_str(&format!("  pubkey sha256 (truncated): {}\n", fp));
    }
}

fn write_lock_state(out: &mut String) {
    let lock_path = "/tmp/ble_radio_owner";
    match std::fs::read_to_string(lock_path) {
        Ok(contents) => {
            out.push_str(&format!("  {} contents:\n", lock_path));
            for line in contents.lines() {
                out.push_str(&format!("    {}\n", line));
            }
            if let Ok(m) = std::fs::metadata(lock_path) {
                if let Ok(mtime) = m.modified() {
                    if let Ok(d) = mtime.duration_since(UNIX_EPOCH) {
                        let age = unix_now() - d.as_secs() as i64;
                        out.push_str(&format!("  lock age: {}s\n", age.max(0)));
                    }
                }
            }
        }
        Err(_) => {
            out.push_str(&format!("  {}: not held (no owner)\n", lock_path));
        }
    }
}

fn write_conf_keys(out: &mut String) {
    let path = "/root/sentryusb.conf";
    match std::fs::read_to_string(path) {
        Ok(s) => {
            for line in s.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("export BLE_")
                    || trimmed.starts_with("export TESLA_BLE_VIN")
                {
                    // Mask the VIN — only show first 3 + last 4 chars
                    // so a screenshot of this bundle doesn't leak the
                    // full VIN. Same length so the format is stable.
                    if trimmed.starts_with("export TESLA_BLE_VIN") {
                        let masked = mask_vin_line(trimmed);
                        out.push_str(&format!("  {}\n", masked));
                    } else {
                        out.push_str(&format!("  {}\n", trimmed));
                    }
                }
            }
        }
        Err(e) => out.push_str(&format!("  {} unreadable: {}\n", path, e)),
    }
}

/// `export TESLA_BLE_VIN="5YJ3E1EA1KF000001"` -> `... "5YJ...0001"`
/// Keeps the format recognizable but drops the middle 10 chars that
/// uniquely identify the vehicle.
fn mask_vin_line(line: &str) -> String {
    let parts: Vec<&str> = line.splitn(2, '=').collect();
    if parts.len() != 2 {
        return line.to_string();
    }
    let value = parts[1].trim_matches(|c| c == '"' || c == '\'');
    if value.len() < 8 {
        return line.to_string();
    }
    let masked = format!("{}...{}", &value[..3], &value[value.len() - 4..]);
    format!("{}=\"{}\"", parts[0], masked)
}

/// Same as write_sample_db but a 1-hour window with per-source counts
/// + per-10-minute bucket breakdown so a tester can see where the
/// gap was if the link dropped mid-bundle window.
async fn write_sample_db_extended(out: &mut String, s: &AppState, now: i64) {
    let store = s.drives.store.clone();
    let res = tokio::task::spawn_blocking(move || {
        store.with_read_conn(|conn| {
            let since = now - 3600;
            let state_n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM telemetry_samples \
                     WHERE ts >= ?1 AND source='state'",
                    (since,),
                    |r| r.get(0),
                )
                .unwrap_or(0);
            let bc_n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM telemetry_samples \
                     WHERE ts >= ?1 AND source='body_controller'",
                    (since,),
                    |r| r.get(0),
                )
                .unwrap_or(0);
            // 10-min buckets across the hour, oldest to newest.
            let mut buckets = Vec::with_capacity(6);
            for i in 0..6 {
                let bucket_start = since + i * 600;
                let bucket_end = bucket_start + 600;
                let n: i64 = conn
                    .query_row(
                        "SELECT count(*) FROM telemetry_samples \
                         WHERE ts >= ?1 AND ts < ?2",
                        (bucket_start, bucket_end),
                        |r| r.get(0),
                    )
                    .unwrap_or(0);
                buckets.push(n);
            }
            (state_n, bc_n, buckets)
        })
    })
    .await
    .ok();
    let (state_n, bc_n, buckets) = res.unwrap_or((0, 0, vec![0; 6]));
    out.push_str(&format!(
        "last hour: {} total ({} state, {} body-controller)\n\n",
        state_n + bc_n,
        state_n,
        bc_n,
    ));
    out.push_str("per-10-min bucket (oldest → newest):\n");
    for (i, n) in buckets.iter().enumerate() {
        let bar = "#".repeat((*n as usize).min(60));
        out.push_str(&format!("  [-{:>2}min..-{:>2}min] {:>4} {}\n",
            60 - i * 10,
            50 - i * 10,
            n,
            bar,
        ));
    }
}

/// dmesg lines mentioning bluetooth/BLE/hci. Useful for catching
/// firmware load failures, link-supervision-timeout events, and
/// HCI command failures that don't reach our user-space logs.
async fn write_dmesg_ble(out: &mut String) {
    match tokio::process::Command::new("dmesg")
        .args(["-T", "--ctime"])
        .output()
        .await
    {
        Ok(o) => {
            let raw = String::from_utf8_lossy(&o.stdout);
            let patterns = ["Bluetooth", "bluetooth", "BLE", "hci", "btusb"];
            let mut last: Vec<&str> = raw
                .lines()
                .filter(|l| patterns.iter().any(|p| l.contains(p)))
                .collect();
            // Cap to last 200 BLE-related lines — older entries are
            // almost always boot-time firmware load noise.
            let start = last.len().saturating_sub(200);
            for line in last.drain(start..) {
                out.push_str(line);
                out.push('\n');
            }
        }
        Err(e) => out.push_str(&format!("dmesg failed: {}\n", e)),
    }
}

async fn write_bluetoothd_journal(out: &mut String) {
    match tokio::process::Command::new("journalctl")
        .args([
            "-u",
            "bluetooth",
            "-b", // current boot only
            "--no-pager",
            "--output=short-iso",
        ])
        .output()
        .await
    {
        Ok(o) if o.status.success() => {
            out.push_str(&String::from_utf8_lossy(&o.stdout));
        }
        Ok(o) => {
            out.push_str(&format!(
                "journalctl -u bluetooth exited {}: {}\n",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr).trim(),
            ));
        }
        Err(e) => out.push_str(&format!("journalctl failed: {}\n", e)),
    }
}

/// Unfiltered journal tail for the sampler service. The on-screen
/// view filters to "interesting" patterns; the bundle keeps
/// everything so we can spot rare panics, RUST_LOG=debug lines, etc.
async fn write_journal_full(out: &mut String, lines: usize) {
    match tokio::process::Command::new("journalctl")
        .args([
            "-u",
            "sentryusb-telemetry",
            "-n",
            &lines.to_string(),
            "--no-pager",
            "--output=short-iso",
        ])
        .output()
        .await
    {
        Ok(o) if o.status.success() => {
            out.push_str(&String::from_utf8_lossy(&o.stdout));
        }
        Ok(o) => {
            out.push_str(&format!(
                "journalctl exited {}: {}\n",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr).trim(),
            ));
        }
        Err(e) => out.push_str(&format!("journalctl failed: {}\n", e)),
    }
}

/// Whole per-minute history file (vs the on-screen 400-line tail).
/// File rotates at 5 MB so this is bounded — typically 100-200 KB.
fn write_history_full(out: &mut String) {
    match std::fs::read_to_string(HISTORY_PATH) {
        Ok(raw) => out.push_str(&raw),
        Err(_) => out.push_str(
            "(no history yet — file appears on first sampler tick after install)\n",
        ),
    }
}

/// Dump the live gate file: sentry/charge/shift the daemon is gating on.
/// `shift_state=Unknown` while parked is expected on Intel-MCU Teslas.
fn write_gate_state(out: &mut String) {
    const GATE_PATH: &str = "/mutable/sentryusb-ble-gate.txt";
    match std::fs::read_to_string(GATE_PATH) {
        Ok(s) if s.trim().is_empty() => {
            out.push_str("  (gate file present but empty)\n");
        }
        Ok(s) => {
            for line in s.lines() {
                out.push_str("  ");
                out.push_str(line.trim());
                out.push('\n');
            }
        }
        Err(_) => {
            out.push_str(
                "  (no gate file — sampler hasn't run since this feature was added,\n  \
                 or the sampler binary on this Pi is older than the api binary)\n",
            );
        }
    }
}

/// Read the live status file the sampler maintains via
/// crates/tesla_ble/src/manager.rs. Cross-process IPC via a single
/// line of plain text — no async, no parsing complexity, and the
/// raw file content is what we surface to the user anyway.
///
/// This is the most reliable "is the sampler connected RIGHT NOW"
/// signal in the bundle. The disconnect log shows history; the
/// systemctl section shows the process; this section shows the BLE
/// link itself.
fn write_live_session_status(out: &mut String) {
    const STATUS_PATH: &str = "/mutable/sentryusb-ble-status.txt";
    match std::fs::read_to_string(STATUS_PATH) {
        Ok(s) if s.trim().is_empty() => {
            out.push_str(
                "  (status file present but empty — sampler hasn't yet attempted a connect)\n",
            );
        }
        Ok(s) => {
            out.push_str("  ");
            out.push_str(s.trim());
            out.push('\n');
        }
        Err(_) => {
            out.push_str(
                "  (no status file — sampler hasn't run since this feature was added,\n  \
                 or the sampler binary on this Pi is older than the api binary)\n",
            );
        }
    }
    out.push_str(
        "\nField guide:\n  \
         state=connected  — sampler holds an active BLE session right now\n  \
         state=disconnected — sampler was connected previously but isn't now\n  \
         since=...       — when the current state was entered (UTC)\n  \
         (last_)mac=...  — Tesla BLE address the sampler talks to\n  \
         (last_)scan_rssi=... — signal strength observed at scan time, just\n    \
                              before the most recent connect attempt. Live\n    \
                              RSSI for active LE connections is NOT exposed\n    \
                              by modern bluez (5.x+) through any standard\n    \
                              userspace API, so scan-time RSSI is the best\n    \
                              correlation we have for slot races and range\n    \
                              issues. Typical values: -40 (very strong) to\n    \
                              -85 (weak); below -90 the link is usually\n    \
                              unusable.\n  \
         drops_total=... — cumulative connection drops since daemon start\n  \
         desync_recoveries=... — how many times our buffer started with a\n    \
                              too-large length prefix and we silently cleared\n    \
                              + kept RX'ing within the same query. Zero is\n    \
                              ideal. Low non-zero (a few per hour) is normal\n    \
                              cleanup of stale BLE notifications. Steadily\n    \
                              climbing is a leading indicator the link is\n    \
                              being polluted by something we don't filter yet\n    \
                              (unknown VCSEC broadcast type, chunked-response\n    \
                              straggler, etc.); each entry corresponds to a\n    \
                              query that ultimately succeeded.\n",
    );
}

/// Walk `/sys/kernel/debug/bluetooth/hci*/<HANDLE>/` for per-connection
/// files. On bluez 5.82 + recent kernels these dirs exist but are empty
/// by design, so usually nothing is found; older stacks that populate
/// them get dumped.
fn write_per_connection_debugfs(out: &mut String) {
    let mut any_adapter = false;
    let mut any_handle = false;
    let mut any_populated = false;
    let adapters = match std::fs::read_dir("/sys/kernel/debug/bluetooth") {
        Ok(d) => d,
        Err(_) => {
            out.push_str(
                "  /sys/kernel/debug/bluetooth/ missing (debugfs not mounted).\n",
            );
            return;
        }
    };
    for adapter in adapters.filter_map(|e| e.ok()) {
        let aname = adapter.file_name().to_string_lossy().to_string();
        if !aname.starts_with("hci") {
            continue;
        }
        any_adapter = true;
        // Per-handle dirs live DIRECTLY under hci*/ (their names are
        // numeric — the BLE connection handle). Skip the named
        // top-level files (supervision_timeout, conn_min_interval,
        // etc. — those are the kernel-default tunables, already
        // covered by the previous section).
        let sub = match std::fs::read_dir(adapter.path()) {
            Ok(d) => d,
            Err(_) => continue,
        };
        for entry in sub.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let path = entry.path();
            // Skip non-directories (the names we care about are all
            // dirs holding per-conn files).
            if !path.is_dir() {
                continue;
            }
            any_handle = true;
            out.push_str(&format!("[{} handle {}]\n", aname, name));
            let inner = match std::fs::read_dir(&path) {
                Ok(d) => d,
                Err(_) => {
                    out.push_str("  (unreadable)\n");
                    continue;
                }
            };
            let mut printed = 0;
            for f in inner.filter_map(|e| e.ok()) {
                let fname = f.file_name().to_string_lossy().to_string();
                let p = f.path();
                if !p.is_file() {
                    continue;
                }
                let val = std::fs::read_to_string(&p)
                    .map(|s| s.trim().to_string())
                    .unwrap_or_else(|_| "<unreadable>".into());
                out.push_str(&format!("  {:<22} = {}\n", fname, val));
                printed += 1;
            }
            if printed > 0 {
                any_populated = true;
            } else {
                out.push_str("  (no files — bluez 5.82+/recent kernel doesn't populate this)\n");
            }
        }
    }
    if !any_adapter {
        out.push_str("  no hci adapters present in debugfs\n");
    } else if !any_handle {
        out.push_str("  no active connections (no numeric handle dirs present)\n");
    } else if !any_populated {
        out.push_str(
            "\nAll per-handle dirs were empty. On bluez 5.x with current Pi\n\
             OS kernels this is expected — the kernel stopped populating\n\
             per-connection debugfs files several releases ago. We track\n\
             connection liveness via the status file and disconnect log\n\
             above; the negotiated link parameters aren't readable from\n\
             userspace on this stack.\n",
        );
    }
}

/// Read the persistent disconnect log written by tesla_ble::manager
/// on every drop. The bundle includes the whole file so trends
/// across days are visible even after journald rotates. File rotates
/// at 256 KB on its own.
fn write_disconnect_history(out: &mut String) {
    const PATH: &str = "/mutable/sentryusb-ble-disconnects.log";
    match std::fs::read_to_string(PATH) {
        Ok(raw) if raw.trim().is_empty() => {
            out.push_str("(empty — no drops recorded yet on this install)\n");
        }
        Ok(raw) => {
            // Newest-first for human reading. Lines are timestamped
            // and one-line-each, so reversing is safe.
            let mut lines: Vec<&str> = raw.lines().collect();
            lines.reverse();
            out.push_str(&format!(
                "(showing newest first, total {} drops on record)\n\n",
                lines.len()
            ));
            for line in lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        Err(_) => {
            out.push_str(
                "(no disconnect log yet — file appears on first drop \
                 after install)\n",
            );
        }
    }
}
