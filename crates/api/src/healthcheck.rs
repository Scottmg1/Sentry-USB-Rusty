//! System health check and diagnostics.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde::Serialize;

use crate::router::AppState;

#[derive(Serialize)]
struct HealthStatus {
    status: String,
    gpu_temp: Option<String>,
    cpu_temp: Option<String>,
    disk_free_pct: Option<f64>,
    uptime_secs: Option<f64>,
    archive_status: String,
}

/// GET /api/system/health-check
pub async fn health_check(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let mut health = HealthStatus {
        status: "ok".to_string(),
        gpu_temp: None,
        cpu_temp: None,
        disk_free_pct: None,
        uptime_secs: None,
        archive_status: "unknown".to_string(),
    };

    // CPU temp
    if let Ok(data) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(millideg) = data.trim().parse::<f64>() {
            let temp = millideg / 1000.0;
            health.cpu_temp = Some(format!("{:.1}", temp));
            if temp > 80.0 {
                health.status = "warning".to_string();
            }
        }
    }

    // GPU temp (Raspberry Pi)
    if let Ok(out) = sentryusb_shell::run("vcgencmd", &["measure_temp"]).await {
        // Format: temp=50.5'C
        if let Some(temp_str) = out.split('=').nth(1) {
            health.gpu_temp = Some(temp_str.trim_end_matches("'C\n").to_string());
        }
    }

    // Disk free
    if let Ok(out) = sentryusb_shell::run("stat", &["--file-system", "--format=%f %b", "/backingfiles/."]).await {
        let parts: Vec<&str> = out.trim().split_whitespace().collect();
        if parts.len() >= 2 {
            if let (Ok(free), Ok(total)) = (parts[0].parse::<f64>(), parts[1].parse::<f64>()) {
                if total > 0.0 {
                    let pct = (free / total) * 100.0;
                    health.disk_free_pct = Some((pct * 10.0).round() / 10.0);
                    if pct < 5.0 {
                        health.status = "warning".to_string();
                    }
                }
            }
        }
    }

    // Uptime
    if let Ok(data) = std::fs::read_to_string("/proc/uptime") {
        if let Some(secs) = data.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()) {
            health.uptime_secs = Some(secs);
        }
    }

    // Archive status (check if archiveloop is running)
    health.archive_status = match sentryusb_shell::run("pgrep", &["-f", "archiveloop"]).await {
        Ok(out) if !out.trim().is_empty() => "running".to_string(),
        _ => "idle".to_string(),
    };

    (StatusCode::OK, Json(serde_json::to_value(health).unwrap_or_default()))
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
