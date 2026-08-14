//! System health check and diagnostics.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::router::AppState;

#[derive(Serialize)]
struct HealthItem {
    name: String,
    /// "pass" | "warn" | "fail"
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
}

#[derive(Serialize)]
struct HealthCategory {
    name: String,
    items: Vec<HealthItem>,
}

#[derive(Serialize)]
struct HealthReport {
    summary: String,
    categories: Vec<HealthCategory>,
}

fn item(name: &str, status: &'static str, detail: Option<String>) -> HealthItem {
    HealthItem { name: name.to_string(), status, detail }
}

/// GET /api/system/health-check
pub async fn health_check(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let mut categories: Vec<HealthCategory> = Vec::new();

    // Config is needed before temperature formatting.
    let active_cfg: std::collections::HashMap<String, String> =
        sentryusb_config::parse_file(sentryusb_config::find_config_path())
            .map(|(active, _commented)| active)
            .unwrap_or_default();

    let use_f = active_cfg
        .get("TEMPERATURE_UNIT")
        .map_or(false, |v| v.eq_ignore_ascii_case("F"));
    let fmt_temp = |celsius: f64| -> String {
        if use_f {
            format!("{:.1}°F", celsius * 9.0 / 5.0 + 32.0)
        } else {
            format!("{:.1}°C", celsius)
        }
    };
    let fmt_threshold = |celsius: f64| -> String {
        if use_f {
            format!("{:.0}°F", celsius * 9.0 / 5.0 + 32.0)
        } else {
            format!("{:.0}°C", celsius)
        }
    };

    // Hardware
    let mut hw = Vec::new();
    let mut cpu_temp_val: Option<f64> = None;
    if let Ok(data) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(millideg) = data.trim().parse::<f64>() {
            cpu_temp_val = Some(millideg / 1000.0);
        }
    }
    match cpu_temp_val {
        Some(t) if t >= 80.0 => hw.push(item("CPU temperature", "fail",
            Some(format!("{} (>{})", fmt_temp(t), fmt_threshold(80.0))))),
        Some(t) if t >= 70.0 => hw.push(item("CPU temperature", "warn",
            Some(fmt_temp(t)))),
        Some(t) => hw.push(item("CPU temperature", "pass",
            Some(fmt_temp(t)))),
        None => hw.push(item("CPU temperature", "warn",
            Some("unavailable".to_string()))),
    }
    if let Ok(out) = sentryusb_shell::run("vcgencmd", &["measure_temp"]).await {
        let raw = out.trim().trim_start_matches("temp=").trim_end_matches("'C");
        let detail = match raw.parse::<f64>() {
            Ok(celsius) => fmt_temp(celsius),
            Err(_) => raw.to_string(),
        };
        hw.push(item("GPU temperature", "pass", Some(detail)));
    }
    if let Ok(out) = sentryusb_shell::run("vcgencmd", &["get_throttled"]).await {
        let raw = out.trim().trim_start_matches("throttled=").to_string();
        let val = u64::from_str_radix(raw.trim_start_matches("0x"), 16).unwrap_or(0);
        let now = val & 0x7;
        let past = (val >> 16) & 0x7;
        if now != 0 {
            hw.push(item("Power/throttling", "fail", Some(format!("active: {}", raw))));
        } else if past != 0 {
            hw.push(item("Power/throttling", "warn", Some(format!("past event: {}", raw))));
        } else {
            hw.push(item("Power/throttling", "pass", None));
        }
    }
    // The service writes its selected CPU-specific binary variant at startup.
    match std::fs::read_to_string("/opt/sentryusb/active-variant") {
        Ok(s) => {
            let variant = s.trim().to_string();
            if variant.is_empty() {
                hw.push(item("Binary variant", "warn", Some("active-variant file empty".to_string())));
            } else {
                hw.push(item("Binary variant", "pass", Some(variant)));
            }
        }
        Err(_) => {
            hw.push(item(
                "Binary variant",
                "warn",
                Some("active-variant missing (single-binary layout — re-run install-pi.sh to migrate)".to_string()),
            ));
        }
    }
    categories.push(HealthCategory { name: "Hardware".to_string(), items: hw });

    // Storage
    let mut st = Vec::new();
    let mut disk_free_pct: Option<f64> = None;
    if let Ok(out) = sentryusb_shell::run(
        "stat", &["--file-system", "--format=%f %b", "/backingfiles/."],
    ).await {
        let parts: Vec<&str> = out.trim().split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(free), Ok(total)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                if total > 0.0 {
                    disk_free_pct = Some((free / total) * 100.0);
                }
            }
        }
    }
    match disk_free_pct {
        Some(p) if p < 5.0 => st.push(item("Backingfiles free space", "fail", Some(format!("{:.1}% free", p)))),
        Some(p) if p < 15.0 => st.push(item("Backingfiles free space", "warn", Some(format!("{:.1}% free", p)))),
        Some(p) => st.push(item("Backingfiles free space", "pass", Some(format!("{:.1}% free", p)))),
        None => st.push(item("Backingfiles free space", "warn", Some("partition not mounted".to_string()))),
    }
    // Check optional disk images only when configured.
    let user_wants = |size_key: &str| -> bool {
        // Empty or a zero numeric prefix disables the image.
        let Some(raw) = active_cfg.get(size_key) else { return false; };
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return false;
        }
        let digits: String = trimmed
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        digits.parse::<f64>().map(|n| n > 0.0).unwrap_or(false)
    };

    let disks: &[(&str, &str, Option<&str>)] = &[
        // The cam disk is mandatory.
        ("/backingfiles/cam_disk.bin", "cam disk image", None),
        ("/backingfiles/music_disk.bin", "music disk image", Some("MUSIC_SIZE")),
        ("/backingfiles/lightshow_disk.bin", "lightshow disk image", Some("LIGHTSHOW_SIZE")),
        ("/backingfiles/boombox_disk.bin", "boombox disk image", Some("BOOMBOX_SIZE")),
    ];
    for (img, label, size_key) in disks {
        if let Some(key) = size_key {
            if !user_wants(key) {
                continue;
            }
        }
        if std::path::Path::new(img).exists() {
            st.push(item(label, "pass", None));
        } else {
            // Missing configured auxiliary disks warn; the cam disk fails.
            let status = if size_key.is_none() { "fail" } else { "warn" };
            st.push(item(label, status, Some("missing".to_string())));
        }
    }
    // `/mutable/TeslaCam` backs the web and Samba content mounts.
    if std::path::Path::new("/mutable/TeslaCam").is_dir() {
        st.push(item("TeslaCam directory", "pass", None));
    } else {
        st.push(item(
            "TeslaCam directory",
            "fail",
            Some("/mutable/TeslaCam missing — Samba + web listing will be empty".to_string()),
        ));
    }
    categories.push(HealthCategory { name: "Storage".to_string(), items: st });

    // Core files; path-invoked scripts must be executable.
    let mut core = Vec::new();
    let core_files: &[(&str, &str, bool)] = &[
        ("/opt/sentryusb/sentryusb", "SentryUSB binary", true),
        ("/root/bin/archiveloop", "archiveloop script", false),
        ("/root/bin/envsetup.sh", "envsetup.sh", false),
        ("/root/bin/enable_gadget.sh", "enable_gadget.sh", true),
        ("/root/bin/disable_gadget.sh", "disable_gadget.sh", true),
        ("/root/bin/make_snapshot.sh", "make_snapshot.sh", true),
        ("/root/bin/release_snapshot.sh", "release_snapshot.sh", true),
        ("/root/bin/manage_free_space.sh", "manage_free_space.sh", true),
        ("/root/bin/waitforidle", "waitforidle", false),
        ("/root/bin/mountimage", "mountimage", false),
        ("/root/bin/remountfs_rw", "remountfs_rw", false),
    ];
    for (path, label, _must_exec) in core_files {
        match std::fs::metadata(path) {
            Err(_) => core.push(item(label, "fail", Some(format!("{} missing", path)))),
            Ok(_md) => {
                #[cfg(unix)]
                {
                    if *_must_exec {
                        use std::os::unix::fs::PermissionsExt;
                        if _md.permissions().mode() & 0o111 == 0 {
                            core.push(item(label, "warn", Some(format!("{} exists but not executable", path))));
                            continue;
                        }
                    }
                }
                core.push(item(label, "pass", Some(path.to_string())));
            }
        }
    }
    categories.push(HealthCategory { name: "Core Files".to_string(), items: core });

    // Configuration
    let mut cfg = Vec::new();
    let config_path = sentryusb_config::find_config_path();
    if std::path::Path::new(config_path).exists() {
        cfg.push(item("Config file", "pass", Some(config_path.to_string())));
    } else {
        cfg.push(item("Config file", "fail", Some("No sentryusb.conf found".to_string())));
    }
    let setup_markers = [
        "/sentryusb/SENTRYUSB_SETUP_FINISHED",
        "/boot/firmware/SENTRYUSB_SETUP_FINISHED",
        "/boot/SENTRYUSB_SETUP_FINISHED",
    ];
    let setup_finished = setup_markers.iter().find(|p| std::path::Path::new(p).exists());
    match setup_finished {
        Some(p) => cfg.push(item("Setup finished", "pass", Some(format!("{} exists", p)))),
        None => cfg.push(item(
            "Setup finished",
            "fail",
            Some("SENTRYUSB_SETUP_FINISHED marker not found".to_string()),
        )),
    }
    match std::fs::read_to_string("/etc/fstab") {
        Err(_) => cfg.push(item("fstab", "fail", Some("Cannot read /etc/fstab".to_string()))),
        Ok(fstab) => {
            cfg.push(item(
                "backingfiles in fstab",
                if fstab.contains("backingfiles") { "pass" } else { "fail" },
                if fstab.contains("backingfiles") { None } else { Some("Missing from /etc/fstab".to_string()) },
            ));
            cfg.push(item(
                "mutable in fstab",
                if fstab.contains("mutable") { "pass" } else { "fail" },
                if fstab.contains("mutable") { None } else { Some("Missing from /etc/fstab".to_string()) },
            ));
            cfg.push(item(
                "cam_disk in fstab",
                if fstab.contains("cam_disk.bin") { "pass" } else { "warn" },
                if fstab.contains("cam_disk.bin") { None } else { Some("Missing (no cam disk configured?)".to_string()) },
            ));
        }
    }
    categories.push(HealthCategory { name: "Configuration".to_string(), items: cfg });

    // USB gadget health requires a UDC binding and a populated primary LUN.
    let mut gad = Vec::new();
    if sentryusb_gadget::is_active() {
        gad.push(item("Gadget UDC bound", "pass", None));
        let lun0 = "/sys/kernel/config/usb_gadget/sentryusb/functions/mass_storage.0/lun.0/file";
        match std::fs::read_to_string(lun0) {
            Ok(s) if !s.trim().is_empty() => {
                gad.push(item("lun.0 backing file", "pass", Some(s.trim().to_string())));
            }
            _ => gad.push(item(
                "lun.0 backing file",
                "fail",
                Some("gadget is bound but exposes no LUN.0 — car will see the drive but nothing on it".to_string()),
            )),
        }
        // Configfs binding can persist after the physical UDC link dies.
        let udc_state = crate::status::read_udc_state();
        match udc_state.as_str() {
            "configured" => gad.push(item("Host link (UDC state)", "pass", None)),
            "" => {}
            other => gad.push(item(
                "Host link (UDC state)",
                "warn",
                Some(format!(
                    "gadget is bound but the host link reads '{other}' — normal while the car \
                     sleeps or suspends the bus, a problem if the car is awake and recording"
                )),
            )),
        }
    } else if std::path::Path::new("/sys/kernel/config/usb_gadget/sentryusb").exists() {
        gad.push(item(
            "Gadget UDC bound",
            "warn",
            Some("configfs dir exists but UDC is empty — toggle drives to re-bind".to_string()),
        ));
    } else {
        gad.push(item("Gadget UDC bound", "warn", Some("gadget disabled".to_string())));
    }
    categories.push(HealthCategory { name: "USB Gadget".to_string(), items: gad });

    // Report the independent car-telemetry central and phone-app GATT
    // peripheral separately.
    let mut ble = Vec::new();

    let ble_enabled = crate::ble::is_ble_enabled()
        || std::path::Path::new("/root/.ble/key_private.pem").exists();

    // The car link is meaningful only after BLE setup.
    if ble_enabled {
        let tele_running = sentryusb_shell::run(
            "systemctl", &["is-active", "--quiet", "sentryusb-telemetry"],
        ).await.is_ok();
        ble.push(item(
            "Tesla car link (telemetry sampler)",
            if tele_running { "pass" } else { "fail" },
            if tele_running {
                None
            } else {
                Some(
                    "sentryusb-telemetry inactive — no live car data (battery/temps/charge \
                     will be stale)"
                        .to_string(),
                )
            },
        ));
    }

    // This peripheral serves the phone app, not the vehicle.
    let ble_running = sentryusb_shell::run(
        "systemctl", &["is-active", "--quiet", "sentryusb-ble"],
    ).await.is_ok();
    ble.push(item(
        "App pairing service (iOS/Android)",
        if ble_running { "pass" } else { "warn" },
        if ble_running {
            None
        } else {
            Some(
                "sentryusb-ble inactive — phone-app pairing unavailable (does NOT affect \
                 car data)"
                    .to_string(),
            )
        },
    ));
    let dbus_policy = std::path::Path::new("/etc/dbus-1/system.d/com.sentryusb.ble.conf").exists();
    ble.push(item(
        "D-Bus policy",
        if dbus_policy { "pass" } else { "warn" },
        if dbus_policy { None } else { Some("com.sentryusb.ble.conf missing".to_string()) },
    ));
    categories.push(HealthCategory { name: "BLE".to_string(), items: ble });

    // Check RTC hardware only when the user opted in.
    let rtc_opted_in = active_cfg.get("RTC_BATTERY_ENABLED").map(|v| v.trim() == "true").unwrap_or(false);
    if rtc_opted_in {
        let mut rtc = Vec::new();
        let has_rtc = std::path::Path::new("/dev/rtc0").exists();
        rtc.push(item(
            "RTC device",
            if has_rtc { "pass" } else { "warn" },
            if has_rtc { None } else { Some("no /dev/rtc0 — clock will reset on power loss".to_string()) },
        ));
        if has_rtc {
            if let Ok(v) = std::fs::read_to_string("/sys/class/rtc/rtc0/device/charging_voltage_now") {
                let uv: i64 = v.trim().parse().unwrap_or(0);
                let mv = uv / 1000;
                let status = if mv >= 2800 { "pass" } else if mv >= 2000 { "warn" } else { "fail" };
                rtc.push(item("RTC battery", status, Some(format!("{} mV", mv))));
            }
        }
        categories.push(HealthCategory { name: "Clock / RTC".to_string(), items: rtc });
    }

    // Services; the archive loop is critical because it preserves footage.
    let mut svcs = Vec::new();
    for (svc, critical) in &[
        ("sentryusb", true),
        ("sentryusb-archive", true),
        ("avahi-daemon", false),
        ("bluetooth", false),
        ("sentryusb-ble", false),
    ] {
        let active = sentryusb_shell::run(
            "systemctl", &["is-active", "--quiet", svc],
        ).await.is_ok();
        let status = if active { "pass" } else if *critical { "fail" } else { "warn" };
        let detail = if active { None } else { Some("inactive".to_string()) };
        svcs.push(item(svc, status, detail));
    }
    categories.push(HealthCategory { name: "Services".to_string(), items: svcs });

    // Network
    let mut net = Vec::new();
    let has_ip = sentryusb_shell::run(
        "bash", &["-c", "ip -4 -o addr show scope global 2>/dev/null | grep -v ' lo ' | head -1"],
    ).await.ok().map(|s| !s.trim().is_empty()).unwrap_or(false);
    net.push(item(
        "Network connectivity",
        if has_ip { "pass" } else { "fail" },
        if has_ip { None } else { Some("no IPv4 address".to_string()) },
    ));
    let dns_ok = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(5),
        "getent", &["hosts", "tesla.com"],
    ).await.is_ok();
    net.push(item(
        "DNS resolution",
        if dns_ok { "pass" } else { "warn" },
        if dns_ok { None } else { Some("tesla.com lookup failed".to_string()) },
    ));
    categories.push(HealthCategory { name: "Network".to_string(), items: net });

    // System
    let mut sys = Vec::new();
    if let Ok(data) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs) = data.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()) {
            let h = (secs / 3600.0) as u64;
            let m = ((secs % 3600.0) / 60.0) as u64;
            sys.push(item("Uptime", "pass", Some(format!("{}h {}m", h, m))));
        }
    }
    let setup_ok = std::path::Path::new("/sentryusb/SENTRYUSB_SETUP_FINISHED").exists()
        || std::path::Path::new("/boot/firmware/SENTRYUSB_SETUP_FINISHED").exists()
        || std::path::Path::new("/boot/SENTRYUSB_SETUP_FINISHED").exists();
    sys.push(item(
        "Setup completed",
        if setup_ok { "pass" } else { "warn" },
        if setup_ok { None } else { Some("setup has not finished".to_string()) },
    ));
    categories.push(HealthCategory { name: "System".to_string(), items: sys });

    // Summary
    let mut fails = 0;
    let mut warns = 0;
    for c in &categories {
        for i in &c.items {
            match i.status {
                "fail" => fails += 1,
                "warn" => warns += 1,
                _ => {}
            }
        }
    }
    let summary = if fails > 0 {
        format!("{} problem{} found", fails, if fails == 1 { "" } else { "s" })
    } else if warns > 0 {
        format!("{} warning{}", warns, if warns == 1 { "" } else { "s" })
    } else {
        "All systems operational".to_string()
    };

    let report = HealthReport { summary, categories };
    (StatusCode::OK, Json(serde_json::to_value(report).unwrap_or_default()))
}

/// POST /api/diagnostics/refresh
pub async fn refresh_diagnostics(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(60),
        "bash",
        &["-c", DIAGNOSTICS_SCRIPT],
    ).await {
        Ok(_) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to generate diagnostics: {}", e)),
    }
}

/// Inline diagnostics gathering script.
const DIAGNOSTICS_SCRIPT: &str = r#"{
  echo "====== SentryUSB Diagnostics ======"
  echo "Date: $(date)"
  echo "Hostname: $(hostname)"
  echo "Uptime: $(uptime)"
  echo ""

  echo "====== version ======"
  cat /opt/sentryusb/version 2>/dev/null || echo "unknown"
  uname -a
  cat /sys/firmware/devicetree/base/model 2>/dev/null; echo
  echo ""

  echo "====== disk / images ======"
  df -h /sentryusb/ / /backingfiles/ /mutable/ 2>/dev/null
  for img in cam music lightshow boombox wraps; do
    f="/backingfiles/${img}_disk.bin"
    if [ -f "$f" ]; then
      echo "$img disk: $(du -h "$f" | cut -f1)"
    fi
  done
  echo ""

  echo "====== USB gadget ======"
  if [ -d /sys/kernel/config/usb_gadget/sentryusb ]; then
    echo "Gadget: active"
    for i in 0 1 2 3 4 5; do
      lun="/sys/kernel/config/usb_gadget/sentryusb/functions/mass_storage.0/lun.${i}/file"
      [ -e "$lun" ] && echo "  lun${i}: $(cat "$lun")"
    done
  else
    echo "Gadget: inactive"
  fi
  cat /sys/class/udc/*/state 2>/dev/null || true
  echo ""

  echo "====== network ======"
  ip -4 addr show 2>/dev/null | grep inet || ifconfig 2>/dev/null
  echo ""

  echo "====== services ======"
  for svc in sentryusb sentryusb-archive sentryusb-ble avahi-daemon bluetooth; do
    status=$(systemctl is-active "$svc" 2>/dev/null || echo "not found")
    echo "  $svc: $status"
  done
  echo ""

  echo "====== archiveloop ======"
  # Bounded, but wide enough to show a failure repeating across several
  # archive cycles rather than a single truncated window.
  tail -1000 /mutable/archiveloop.log 2>/dev/null || echo "no archiveloop log"
  echo ""

  echo "====== bluetooth / BLE telemetry ======"
  # Same single-page BLE dump the Logs -> Bluetooth view shows: adapter in
  # use, sampler service state, observed vehicle activity, sample ages,
  # recent failures and journal lines. Without this a BLE question cannot be
  # answered from this report at all.
  curl -fsS --max-time 10 http://[::1]/api/logs/bluetooth 2>/dev/null \
    || echo "could not reach /api/logs/bluetooth"
  echo ""

  echo "====== sentryusb service journal (last 300) ======"
  journalctl -u sentryusb -n 300 --no-pager 2>/dev/null \
    || echo "no sentryusb journal entries"
  echo ""

  echo "====== drive-import history (persisted, last 20) ======"
  curl -fsS --max-time 5 http://[::1]/api/drives/data/import-history 2>/dev/null \
    || echo "could not reach /api/drives/data/import-history"
  echo ""

  echo "====== drive-import logs (journalctl, last 7 days) ======"
  journalctl -u sentryusb --since "7 days ago" --no-pager 2>/dev/null \
    | grep -E "import_json|group_clips|hide_tessie_overlapping_sei|upload_data|drive cache:" \
    | tail -200 \
    || echo "no matching journalctl entries"
  echo ""

  echo "====== temperatures ======"
  cat /sys/class/thermal/thermal_zone0/temp 2>/dev/null | awk '{printf "CPU: %.1f°C\n", $1/1000}'
  vcgencmd measure_temp 2>/dev/null || true
  echo ""

  echo "====== dmesg (last 30) ======"
  dmesg -T 2>/dev/null | tail -30
  echo ""

  echo "====== end of diagnostics ======"
} &> /tmp/diagnostics.txt"#;

/// GET /api/diagnostics
pub async fn get_diagnostics(State(_s): State<AppState>) -> impl IntoResponse {
    match std::fs::read_to_string("/tmp/diagnostics.txt") {
        Ok(data) => {
            let cleaned = sanitize_diagnostics(&data);
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                cleaned,
            )
        }
        Err(_) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "Diagnostics have not been generated yet.\nClick the Refresh button above to generate a diagnostics report.".to_string(),
        ),
    }
}

fn sanitize_diagnostics(raw: &str) -> String {
    let ansi_re = regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap();
    let cleaned = ansi_re.replace_all(raw, "");

    // Preserve text formatting controls only.
    cleaned
        .chars()
        .filter(|&c| c == '\t' || c == '\n' || c == '\r' || c >= '\x20')
        .collect()
}
