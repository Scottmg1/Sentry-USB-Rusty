use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};

use crate::router::AppState;

/// GET /api/memory — JSON memory stats
pub async fn memory_stats(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let mut stats = serde_json::Map::new();

    // Read RSS from /proc/self/statm on Linux
    if let Ok(statm) = std::fs::read_to_string("/proc/self/statm") {
        let parts: Vec<&str> = statm.split_whitespace().collect();
        if parts.len() >= 2 {
            let page_size = 4096u64; // typical page size
            if let Ok(pages) = parts[1].parse::<u64>() {
                stats.insert("rss_mb".into(), serde_json::json!((pages * page_size) as f64 / 1024.0 / 1024.0));
            }
            if let Ok(pages) = parts[0].parse::<u64>() {
                stats.insert("vsz_mb".into(), serde_json::json!((pages * page_size) as f64 / 1024.0 / 1024.0));
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
