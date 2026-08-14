use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};

use crate::router::AppState;

/// GET /api/memory — JSON memory stats
pub async fn memory_stats(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let mut stats = serde_json::Map::new();

    // `/proc/self/status` avoids assumptions about kernel page size.
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            let (key, target) = match line.split_once(':') {
                Some(("VmRSS", rest)) => (rest, "rss_mb"),
                Some(("VmSize", rest)) => (rest, "vsz_mb"),
                _ => continue,
            };
            if let Some(kb) = key
                .trim()
                .strip_suffix("kB")
                .and_then(|v| v.trim().parse::<u64>().ok())
            {
                stats.insert(target.into(), serde_json::json!(kb as f64 / 1024.0));
            }
        }
    }

    (StatusCode::OK, Json(serde_json::Value::Object(stats)))
}

/// GET /memory — HTML memory debug page
pub async fn memory_page(State(_s): State<AppState>) -> impl IntoResponse {
    Html(r#"<!DOCTYPE html>
<html><head><title>SentryUSB Memory</title>
<style>body{font-family:monospace;background:#1a1a2e;color:#eee;padding:20px;}
button{background:#0f3460;color:#eee;border:none;padding:8px 16px;cursor:pointer;margin:10px 0;}
pre{background:#16213e;padding:10px;border-radius:4px;overflow-x:auto;}</style>
</head><body>
<h1>SentryUSB Memory Debug</h1>
<button onclick="refresh()">Refresh</button>
<pre id="data">Loading...</pre>
<script>
async function refresh() {
  const r = await fetch('/api/memory');
  const d = await r.json();
  document.getElementById('data').textContent = JSON.stringify(d, null, 2);
}
refresh();
</script></body></html>"#)
}
