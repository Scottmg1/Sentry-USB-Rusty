//! Upload-sweep main loop. Owned by the spawned tokio task; runs the
//! full encrypt + batch + post pipeline whenever:
//!
//!   * The `tokio::sync::Notify` clone wired into `Processor` fires
//!     (i.e. `do_process` finished and may have written new routes), or
//!   * The 600 s safety-net timer expires.
//!
//! Idempotent: queries `cloud_uploaded_at IS NULL` so a Notify with no
//! fresh rows is a cheap no-op. Single in-flight request at a time.

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

/// Upload at most this many routes per HTTP request. Matches the
/// server-side Zod schema (max 32).
const BATCH_LIMIT: i64 = 32;

/// Server's per-route blob ceiling (256 KiB binary → ~384 KiB base64).
/// Routes whose encrypted blob exceeds this are permanently rejected by
/// the server (`rejected_too_large`), so we skip them client-side to
/// avoid wasting bandwidth and rate-limit budget.
const MAX_ROUTE_BLOB_B64_LEN: usize = 384 * 1024;

/// Soft ceiling on the serialized JSON body we're willing to POST. The
/// server's Fastify bodyLimit on this endpoint is 16 MiB; stay well
/// under to leave headroom for JSON framing and to tolerate slight
/// estimation drift.
const MAX_BATCH_BODY_BYTES: usize = 14 * 1024 * 1024;

/// 10-minute periodic safety-net.
const SAFETY_TIMER: Duration = Duration::from_secs(600);

/// Wait between consecutive batches when there are still pending rows
/// after one batch landed. Keeps the upload pace below the per-piId rate
/// limit (600/min).
const INTER_BATCH_PAUSE: Duration = Duration::from_millis(50);

/// Loop entry point. Drives forever until the runtime shuts down.
pub async fn run_sweep_loop(state: Arc<CloudStateInner>) {
    loop {
        // Wait for a wake.
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
                // No-op sweep is the steady state — don't log.
            }
            Err(e) => {
                warn!("cloud sweep error: {}", e);
                let mut last_err = state.last_upload_error.lock().await;
                *last_err = Some(format!("{:#}", e));
            }
        }
    }
}

/// One end-to-end sweep. Returns the number of routes successfully
/// stored (server status `stored`; `duplicate` does not increment).
async fn sweep_once(state: Arc<CloudStateInner>) -> Result<u32> {
    // Skip silently when unpaired.
    let creds_snapshot = {
        let g = state.creds.lock().await;
        match g.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(0),
        }
    };

    // Unlock per-Pi key + bearer token.
    let unlocked = UnlockedCreds::unlock(&creds_snapshot).or_else(|_| {
        // Dev fallback for tests / non-Pi hardware.
        let serial = std::env::var("SENTRYCLOUD_DEV_SERIAL")
            .map(|s| s.into_bytes())
            .map_err(|_| anyhow!("unlock failed and SENTRYCLOUD_DEV_SERIAL unset"))?;
        UnlockedCreds::unlock_with_serial(&creds_snapshot, &serial)
    })?;

    let client =
        CloudClient::new(&creds_snapshot.cloud_base_url).with_bearer(&unlocked.pi_auth_token);

    let mut total_stored: u32 = 0;
    loop {
        // Pull the next batch from the local DB.
        let pending = db_ext::select_pending(&state.store, BATCH_LIMIT)
            .context("select pending routes")?;
        if pending.is_empty() {
            break;
        }

        // Encrypt each row + cache route_id back to the DB if uncached.
        // Track cumulative batch size so we don't blow past the server's
        // 16 MiB bodyLimit.
        let mut wire_routes: Vec<UploadRoute> = Vec::with_capacity(pending.len());
        let mut estimated_body_bytes: usize = 64; // JSON envelope overhead
        for p in &pending {
            let encrypted = encrypt::encrypt_route(
                &p.route,
                &unlocked.pi_key,
                &creds_snapshot.user_id,
                &creds_snapshot.pi_id,
                p.cloud_route_id.as_deref(),
            )
            .with_context(|| format!("encrypt {}", p.file))?;
            // Cache the route_id on first encrypt.
            if p.cloud_route_id.is_none() {
                if let Err(e) = db_ext::cache_route_id(&state.store, &p.file, &encrypted.route_id) {
                    warn!("cache_route_id failed for {}: {}", p.file, e);
                }
            }

            // Skip routes whose encrypted blob exceeds the server's
            // per-route cap — they'd be `rejected_too_large` anyway.
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
                + 96; // JSON keys + quotes + braces
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

        // POST the batch.
        let body = UploadBody {
            pi_id: creds_snapshot.pi_id.clone(),
            routes: wire_routes,
        };
        let resp = client
            .post_json_bearer("/api/pi/routes", &body)
            .await
            .map_err(|e| anyhow!("upload POST: {}", e))?;
        let status = resp.status();

        // 401: token invalid → wipe.
        // 403 user_suspended: keep credentials, surface error state.
        // 403 revoked / other: wipe.
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
        // Stale generation → run rekey then retry the same batch.
        if status.as_u16() == 409 {
            let body_text = resp.text().await.unwrap_or_default();
            if body_text.contains("pi_key_stale") {
                info!("cloud upload: pi_key_stale; running rekey");
                match rekey::poll_and_apply(state.clone()).await {
                    Ok(true) => {
                        // Retry the loop with fresh credentials. Reload
                        // happens at the top of `sweep_once`'s next call —
                        // simplest path is to just bail and let the Notify
                        // wake us back up. Caller logs and retries on the
                        // next safety tick.
                        state.notify.notify_one();
                        return Ok(total_stored);
                    }
                    Ok(false) => {
                        // Rekey not yet ready (browser still rotating).
                        // Bail and retry on the next safety tick.
                        return Ok(total_stored);
                    }
                    Err(e) => {
                        return Err(anyhow!("rekey: {}", e));
                    }
                }
            }
            return Err(anyhow!("upload: HTTP 409 body={}", body_text));
        }
        // Anything else non-2xx: bubble up so the caller can log and retry.
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("upload: HTTP {} body={}", status, body_text));
        }

        let parsed: UploadResponse = resp.json().await.context("parse upload response")?;

        // Stamp `cloud_uploaded_at` for any route the server now has
        // (`stored` or `duplicate`). Anything else stays NULL and will
        // be retried on the next sweep — except `rejected_storage_full`
        // which we surface to the user without retrying tightly.
        let now_unix = now_ms() / 1000;
        let mut storage_full_seen = false;
        let mut stored_this_batch: u32 = 0;
        for result in &parsed.results {
            // Find the source `file` for this routeId in our pending list.
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
                    // Permanent rejection — the route's encrypted size
                    // exceeds the server's 256 KiB cap and won't shrink
                    // on retry. Stamp `cloud_uploaded_at = -1` (sentinel
                    // in db_ext) so future sweeps naturally skip it via
                    // the existing `IS NULL` filter; pendingRouteCount
                    // also drops it. Operator can still find these by
                    // querying `cloud_uploaded_at = -1`.
                    if let Some(f) = source_file {
                        warn!("cloud upload: rejected_too_large for {} (permanent skip)", f);
                        if let Err(e) = db_ext::mark_permanent_skip(&state.store, f) {
                            warn!("mark_permanent_skip failed for {}: {}", f, e);
                        }
                    }
                }
                "rejected_stale_generation" => {
                    // Surfaces if the server changes its mind mid-batch.
                    // Trigger rekey on next sweep.
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
            // Don't tight-loop against an exhausted account.
            break;
        }

        // Loop again if there are more pending rows. Brief breather to
        // stay below the rate limit.
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
