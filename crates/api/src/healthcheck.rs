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
        &["-c", "(sudo /root/bin/setup-sentryusb diagnose) &> /tmp/diagnostics.txt"],
    ).await {
        Ok(_) => crate::json_ok(),
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("Failed to generate diagnostics: {}", e)),
    }
}

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
