

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::client::CloudClient;
use crate::credentials_store::UnlockedCreds;
use crate::db_ext;
use crate::encrypt;
use crate::rekey;
use crate::state::{now_ms, CloudStateInner};

const BATCH_LIMIT: i64 = 32;

const MAX_ROUTE_BLOB_B64_LEN: usize = 384 * 1024;

const MAX_BATCH_BODY_BYTES: usize = 14 * 1024 * 1024;

const SAFETY_TIMER: Duration = Duration::from_secs(600);

const INTER_BATCH_PAUSE: Duration = Duration::from_millis(50);

pub async fn run_sweep_loop(state: Arc<CloudStateInner>) {
    loop {

        tokio::select! {
            _ = state.notify.notified() => {
                debug!("cloud sweep: woken by Notify");
            }
            _ = tokio::time::sleep(SAFETY_TIMER) => {
                debug!("cloud sweep: woken by safety timer");
            }
        }

        match sweep_once(state.clone()).await {
            Ok(uploaded) if uploaded > 0 => {
                info!("cloud sweep complete: {} routes uploaded", uploaded);
            }
            Ok(_) => {

            }
            Err(e) => {
                warn!("cloud sweep error: {}", e);
                let mut last_err = state.last_upload_error.lock().await;
                *last_err = Some(format!("{:#}", e));
            }
        }
    }
}

async fn sweep_once(state: Arc<CloudStateInner>) -> Result<u32> {

    let creds_snapshot = {
        let g = state.creds.lock().await;
        match g.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(0),
        }
    };

    let unlocked = UnlockedCreds::unlock(&creds_snapshot).or_else(|_| {

        let serial = std::env::var("SENTRYCLOUD_DEV_SERIAL")
            .map(|s| s.into_bytes())
            .map_err(|_| anyhow!("unlock failed and SENTRYCLOUD_DEV_SERIAL unset"))?;
        UnlockedCreds::unlock_with_serial(&creds_snapshot, &serial)
    })?;

    let client =
        CloudClient::new(&creds_snapshot.cloud_base_url).with_bearer(&unlocked.pi_auth_token);

    let metadata_header = {
        use base64::Engine;
        let json = crate::pairing::pi_metadata().to_string();
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(json.as_bytes())
    };

    let mut total_stored: u32 = 0;
    loop {

        let pending = db_ext::select_pending(&state.store, BATCH_LIMIT)
            .context("select pending routes")?;
        if pending.is_empty() {
            break;
        }

        let mut wire_routes: Vec<UploadRoute> = Vec::with_capacity(pending.len());
        let mut estimated_body_bytes: usize = 64;
        for p in &pending {
            let encrypted = encrypt::encrypt_route(
                &p.route,
                &unlocked.pi_key,
                &creds_snapshot.user_id,
                &creds_snapshot.pi_id,
                p.cloud_route_id.as_deref(),
            )
            .with_context(|| format!("encrypt {}", p.file))?;

            if p.cloud_route_id.is_none() {
                if let Err(e) = db_ext::cache_route_id(&state.store, &p.file, &encrypted.route_id) {
                    warn!("cache_route_id failed for {}: {}", p.file, e);
                }
            }

            if encrypted.route_blob_b64.len() > MAX_ROUTE_BLOB_B64_LEN {
                warn!(
                    "cloud upload: skipping {} (blob {} bytes > {} limit)",
                    p.file,
                    encrypted.route_blob_b64.len(),
                    MAX_ROUTE_BLOB_B64_LEN,
                );
                if let Err(e) = db_ext::mark_permanent_skip(&state.store, &p.file) {
                    warn!("mark_permanent_skip failed for {}: {}", p.file, e);
                }
                continue;
            }

            let route_json_size = encrypted.route_blob_b64.len()
                + encrypted.wrapped_route_key_b64.len()
                + encrypted.route_id.len()
                + 96;
            if !wire_routes.is_empty()
                && estimated_body_bytes + route_json_size > MAX_BATCH_BODY_BYTES
            {
                debug!(
                    "cloud upload: capping batch at {} routes (est {} bytes)",
                    wire_routes.len(),
                    estimated_body_bytes,
                );
                break;
            }
            estimated_body_bytes += route_json_size;

            wire_routes.push(UploadRoute {
                route_id: encrypted.route_id,
                route_blob: encrypted.route_blob_b64,
                wrapped_route_key: encrypted.wrapped_route_key_b64,
            });
        }

        if wire_routes.is_empty() {
            break;
        }

        let body = UploadBody {
            pi_id: creds_snapshot.pi_id.clone(),
            routes: wire_routes,
        };
        let resp = client
            .post_json_bearer_with_headers(
                "/api/pi/routes",
                &body,
                &[("X-Sentryusb-Metadata", metadata_header.clone())],
            )
            .await
            .map_err(|e| anyhow!("upload POST: {}", e))?;
        let status = resp.status();

        if status.as_u16() == 401 {
            warn!("cloud upload: 401, wiping credentials");
            state.handle_remote_revoke().await;
            return Err(anyhow!("auth rejected; pi unpaired"));
        }
        if status.as_u16() == 403 {
            let body_text = resp.text().await.unwrap_or_default();
            let err_field = serde_json::from_str::<serde_json::Value>(&body_text)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()));
            match err_field.as_deref() {
                Some("user_suspended") => {
                    warn!("cloud upload: user_suspended; pausing without unpair");
                    *state.last_upload_error.lock().await =
                        Some("user_suspended".to_string());
                    state.hub.broadcast(
                        "cloud_upload",
                        &serde_json::json!({
                            "uploaded": 0,
                            "pending": db_ext::pending_count(&state.store),
                            "error": "user_suspended",
                        }),
                    );
                    return Err(anyhow!("user_suspended; uploads paused"));
                }
                _ => {
                    warn!("cloud upload: 403 ({:?}), wiping credentials", err_field);
                    state.handle_remote_revoke().await;
                    return Err(anyhow!("auth rejected; pi unpaired"));
                }
            }
        }

        if status.as_u16() == 409 {
            let body_text = resp.text().await.unwrap_or_default();
            if body_text.contains("pi_key_stale") {
                info!("cloud upload: pi_key_stale; running rekey");
                match rekey::poll_and_apply(state.clone()).await {
                    Ok(true) => {

                        state.notify.notify_one();
                        return Ok(total_stored);
                    }
                    Ok(false) => {

                        return Ok(total_stored);
                    }
                    Err(e) => {
                        return Err(anyhow!("rekey: {}", e));
                    }
                }
            }
            return Err(anyhow!("upload: HTTP 409 body={}", body_text));
        }

        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("upload: HTTP {} body={}", status, body_text));
        }

        let parsed: UploadResponse = resp.json().await.context("parse upload response")?;

        let now_unix = now_ms() / 1000;
        let mut storage_full_seen = false;
        let mut stored_this_batch: u32 = 0;
        for result in &parsed.results {

            let source_file = pending
                .iter()
                .find(|p| {
                    p.cloud_route_id.as_deref() == Some(&result.route_id)
                        || sentryusb_cloud_crypto::ids::route_id_from_path(&p.route.file)
                            == result.route_id
                })
                .map(|p| p.file.as_str());
            match result.status.as_str() {
                "stored" => {
                    stored_this_batch += 1;
                    if let Some(f) = source_file {
                        if let Err(e) = db_ext::mark_uploaded(&state.store, f, now_unix) {
                            warn!("mark_uploaded failed for {}: {}", f, e);
                        }
                    }
                }
                "duplicate" => {
                    if let Some(f) = source_file {
                        if let Err(e) = db_ext::mark_uploaded(&state.store, f, now_unix) {
                            warn!("mark_uploaded failed for {}: {}", f, e);
                        }
                    }
                }
                "rejected_storage_full" => {
                    storage_full_seen = true;
                }
                "rejected_too_large" => {

                    if let Some(f) = source_file {
                        warn!("cloud upload: rejected_too_large for {} (permanent skip)", f);
                        if let Err(e) = db_ext::mark_permanent_skip(&state.store, f) {
                            warn!("mark_permanent_skip failed for {}: {}", f, e);
                        }
                    }
                }
                "rejected_stale_generation" => {

                }
                other => {
                    warn!("cloud upload: unexpected status `{}`", other);
                }
            }
        }

        total_stored += stored_this_batch;
        state
            .total_uploaded
            .fetch_add(stored_this_batch as u64, Ordering::Relaxed);
        if stored_this_batch > 0 {
            state.last_upload_at_ms.store(now_ms(), Ordering::Relaxed);
            *state.last_upload_error.lock().await = None;
            let pending = db_ext::pending_count(&state.store);
            state.hub.broadcast(
                "cloud_upload",
                &serde_json::json!({
                    "uploaded": stored_this_batch,
                    "pending": pending,
                    "error": serde_json::Value::Null,
                }),
            );
        }

        if storage_full_seen {
            *state.last_upload_error.lock().await = Some("storage_full".to_string());

            break;
        }

        tokio::time::sleep(INTER_BATCH_PAUSE).await;
    }

    Ok(total_stored)
}

#[derive(Serialize)]
struct UploadBody {
    #[serde(rename = "piId")]
    pi_id: String,
    routes: Vec<UploadRoute>,
}

#[derive(Serialize)]
struct UploadRoute {
    #[serde(rename = "routeId")]
    route_id: String,
    #[serde(rename = "routeBlob")]
    route_blob: String,
    #[serde(rename = "wrappedRouteKey")]
    wrapped_route_key: String,
}

#[derive(Deserialize)]
struct UploadResponse {
    results: Vec<UploadResult>,
}

#[derive(Deserialize)]
struct UploadResult {
    #[serde(rename = "routeId")]
    route_id: String,
    status: String,
}
