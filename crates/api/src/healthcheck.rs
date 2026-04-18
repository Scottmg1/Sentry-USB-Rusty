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
    for (img, label) in &[
        ("/backingfiles/cam_disk.bin", "cam disk image"),
        ("/backingfiles/music_disk.bin", "music disk image"),
    ] {
        if std::path::Path::new(img).exists() {
            st.push(item(label, "pass", None));
        } else {
            let status = if *label == "cam disk image" { "fail" } else { "warn" };
            st.push(item(label, status, Some("missing".to_string())));
        }
    }
    categories.push(HealthCategory { name: "Storage".to_string(), items: st });

    // ── Services ──────────────────────────────────────────────────────────
    let mut svcs = Vec::new();
    for (svc, critical) in &[
        ("sentryusb", true),
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
