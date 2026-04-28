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

    // ── Hardware ──────────────────────────────────────────────────────────
    let mut hw = Vec::new();
    let mut cpu_temp_val: Option<f64> = None;
    if let Ok(data) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(millideg) = data.trim().parse::<f64>() {
            cpu_temp_val = Some(millideg / 1000.0);
        }
    }
    match cpu_temp_val {
        Some(t) if t >= 80.0 => hw.push(item("CPU temperature", "fail", Some(format!("{:.1}°C (>80°C)", t)))),
        Some(t) if t >= 70.0 => hw.push(item("CPU temperature", "warn", Some(format!("{:.1}°C", t)))),
        Some(t) => hw.push(item("CPU temperature", "pass", Some(format!("{:.1}°C", t)))),
        None => hw.push(item("CPU temperature", "warn", Some("unavailable".to_string()))),
    }
    if let Ok(out) = sentryusb_shell::run("vcgencmd", &["measure_temp"]).await {
        let s = out.trim().trim_start_matches("temp=").trim_end_matches("'C").to_string();
        hw.push(item("GPU temperature", "pass", Some(s)));
    }
    // Throttling
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
    categories.push(HealthCategory { name: "Hardware".to_string(), items: hw });

    // ── Storage ───────────────────────────────────────────────────────────
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
    // Load the active config so we only nag about disks the user
    // actually asked for. If MUSIC_SIZE=0 (or unset) the user opted
    // out of the music disk entirely — warning "music disk image
    // missing" when they never configured it is just noise.
    let active_cfg: std::collections::HashMap<String, String> =
        sentryusb_config::parse_file(sentryusb_config::find_config_path())
            .map(|(active, _commented)| active)
            .unwrap_or_default();
    let user_wants = |size_key: &str| -> bool {
        // "0", "0G", "0M", "0K", empty, unset → disabled. Any non-zero
        // numeric prefix → enabled. Strict-enough for health: the
        // actual size value is the installer/setup's problem.
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
        // cam disk is always expected — hard fail if it's missing.
        ("/backingfiles/cam_disk.bin", "cam disk image", None),
        ("/backingfiles/music_disk.bin", "music disk image", Some("MUSIC_SIZE")),
        ("/backingfiles/lightshow_disk.bin", "lightshow disk image", Some("LIGHTSHOW_SIZE")),
        ("/backingfiles/boombox_disk.bin", "boombox disk image", Some("BOOMBOX_SIZE")),
    ];
    for (img, label, size_key) in disks {
        // Optional disk the user didn't ask for → skip the check entirely.
        if let Some(key) = size_key {
            if !user_wants(key) {
                continue;
            }
        }
        if std::path::Path::new(img).exists() {
            st.push(item(label, "pass", None));
        } else {
            // cam is critical; configured-but-missing optional disks are
            // warn (something went wrong during setup/archiving).
            let status = if size_key.is_none() { "fail" } else { "warn" };
            st.push(item(label, status, Some("missing".to_string())));
        }
    }
    // TeslaCam directory on /mutable — the bind-mount target for
    // `mount.ctts`. Without it, the FUSE wrapper can't expose the cam
    // content to Samba/web downloads, and the dashboard would otherwise
    // show "all green" while TeslaCam is silently empty.
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

    // ── Core files ────────────────────────────────────────────────────────
    //
    // Matches Go `checkCoreFiles` (server/api/healthcheck.go:74-109). Scripts
    // marked `exec` must be executable; missing-or-not-executable is a fail/warn
    // because archiveloop invokes these by path.
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

    // ── Configuration ─────────────────────────────────────────────────────
    //
    // Matches Go `checkConfig` (healthcheck.go:112-162): config file presence,
    // setup-finished marker, fstab entries for backingfiles/mutable/cam_disk.
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

    // ── USB gadget ────────────────────────────────────────────────────────
    //
    // Goes deeper than the Go check: verify the gadget is not just present in
    // configfs but actually bound to a UDC and exposing at least `lun.0` with
    // a real backing file. An enumerated-but-no-LUNs gadget is the failure
    // mode Phase A.2 / A.4 were fixing.
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

    // ── BLE ───────────────────────────────────────────────────────────────
    //
    // Daemon is a Python service (`sentryusb-ble.service`) bundled by
    // install-pi.sh. Check that it's running AND that the required D-Bus
    // policy file is in place — without the policy file the daemon can't own
    // its well-known name and iOS pairing silently fails.
    let mut ble = Vec::new();
    let ble_running = sentryusb_shell::run(
        "systemctl", &["is-active", "--quiet", "sentryusb-ble"],
    ).await.is_ok();
    ble.push(item(
        "sentryusb-ble daemon",
        if ble_running { "pass" } else { "warn" },
        if ble_running { None } else { Some("inactive — iOS pairing unavailable".to_string()) },
    ));
    let dbus_policy = std::path::Path::new("/etc/dbus-1/system.d/com.sentryusb.ble.conf").exists();
    ble.push(item(
        "D-Bus policy",
        if dbus_policy { "pass" } else { "warn" },
        if dbus_policy { None } else { Some("com.sentryusb.ble.conf missing".to_string()) },
    ));
    categories.push(HealthCategory { name: "BLE".to_string(), items: ble });

    // ── RTC ───────────────────────────────────────────────────────────────
    //
    // Only surface this category when the user opted in (RTC_BATTERY_ENABLED).
    // Pi 4 and earlier ship without an RTC; warning "no /dev/rtc0" on a Pi 4
    // whose owner never configured an external RTC module is just noise.
    // On Pi 5 with RTC_BATTERY_ENABLED=true we expect /dev/rtc0 and a
    // readable battery voltage; missing either is a warn/fail.
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
            // Pi 5 RTC battery charge level.
            if let Ok(v) = std::fs::read_to_string("/sys/class/rtc/rtc0/device/charging_voltage_now") {
                let uv: i64 = v.trim().parse().unwrap_or(0);
                let mv = uv / 1000;
                let status = if mv >= 2800 { "pass" } else if mv >= 2000 { "warn" } else { "fail" };
                rtc.push(item("RTC battery", status, Some(format!("{} mV", mv))));
            }
        }
        categories.push(HealthCategory { name: "Clock / RTC".to_string(), items: rtc });
    }

    // ── Services ──────────────────────────────────────────────────────────
    // sentryusb-archive is the archiveloop unit — marked critical so a
    // crashed archive loop shows up as RED on the dashboard instead of
    // being invisible (the previous list omitted it, so users would see
    // "all green" while their Tesla footage wasn't being archived).
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

    // ── Network ───────────────────────────────────────────────────────────
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

    // ── System ────────────────────────────────────────────────────────────
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

    // ── Summary ───────────────────────────────────────────────────────────
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

/// Inline diagnostics gathering script — replaces the old `setup-sentryusb diagnose` command.
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
  tail -50 /mutable/archiveloop.log 2>/dev/null || echo "no archiveloop log"
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
            // Strip ANSI escape codes and control chars
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
    // Strip ANSI escape codes
    let ansi_re = regex::Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap();
    let cleaned = ansi_re.replace_all(raw, "");

    // Remove control chars except \t \n \r
    cleaned
        .chars()
        .filter(|&c| c == '\t' || c == '\n' || c == '\r' || c >= '\x20')
        .collect()
}
