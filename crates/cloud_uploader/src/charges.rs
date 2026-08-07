//! Charge-session upload sweep.
//!
//! Mirrors the route sweep in `uploader.rs`: derive completed sessions
//! from `telemetry_samples` (shared logic in `sentryusb_drives::charging`
//! — identity and grouping MUST match the local /api/charging view),
//! encrypt each one under a fresh chargeKey, and batch-POST to
//! `/api/pi/charges`. Only sessions that ended at least
//! `SESSION_GAP_SECS` ago are eligible — grouping is final then, so the
//! immutable blob never needs re-cutting.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use sentryusb_drives::charging::{
    self, ChargeSessionSummary, SESSION_GAP_SECS,
};

use crate::client::CloudClient;
use crate::credentials_store::UnlockedCreds;
use crate::encrypt::{self, ChargeMutable, CostOverride};
use crate::state::{now_ms, CloudStateInner};

/// Curve points per uploaded session. Charge curves don't need full
/// sample density; downsampling bounds the blob size and keeps the
/// cloud detail view fast to open.
const MAX_BLOB_POINTS: usize = 200;

const BATCH_LIMIT: usize = 32;

/// `charge_uploads.uploaded_at` sentinel for permanently-skipped
/// sessions (rejected_too_large), mirroring db_ext's route sentinel.
pub const PERMANENT_SKIP_SENTINEL: i64 = -1;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadCharge {
    charge_id: String,
    charge_blob: String,
    wrapped_charge_key: String,
    summary_ciphertext: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    mutable_ciphertext: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UploadBody {
    pi_id: String,
    charges: Vec<UploadCharge>,
}

#[derive(Deserialize)]
struct UploadResponse {
    results: Vec<UploadResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadResult {
    charge_id: String,
    status: String,
}

/// One sweep pass. Returns the number of sessions newly stored.
pub async fn sweep_once(state: Arc<CloudStateInner>) -> Result<u32> {
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

    // Derive eligible sessions. Sessions still inside the gap window may
    // yet grow — skip them; the safety timer re-sweeps soon enough.
    // The full charge-row scan + grouping is sync DB/CPU work — blocking
    // pool, so the 10-minute safety sweep can't stall the reactor.
    let store = state.store.clone();
    let (pending, tag_map, cost_map, dirty) = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let uploads = store.charge_uploads_map().context("charge_uploads_map")?;
            let rows = store
                .with_read_conn(|conn| -> Result<_> { charging::load_charge_rows(conn, 0, None) })
                .context("load charge rows")?;
            let now_secs = now_ms() / 1000;
            let pending: Vec<Vec<charging::ChargeRow>> = charging::group_sessions(rows)
                .into_iter()
                .filter(|s| {
                    let Some(first) = s.first() else { return false };
                    let Some(last) = s.last() else { return false };
                    now_secs - last.ts >= SESSION_GAP_SECS && !uploads.contains_key(&first.ts)
                })
                .collect();

            let tag_map = store.get_all_charge_tags().unwrap_or_default();
            let cost_map = store.get_all_charge_costs().unwrap_or_default();
            // Dirty rows we're about to fold into upload payloads — cleared on
            // stored/duplicate so the sync push doesn't re-send the same state.
            let dirty: std::collections::HashMap<String, i64> = store
                .dirty_mutables()
                .unwrap_or_default()
                .into_iter()
                .filter(|(kind, _, _)| kind == "charge")
                .map(|(_, key, at)| (key, at))
                .collect();
            Ok((pending, tag_map, cost_map, dirty))
        })
        .await
        .map_err(|e| anyhow!("charge prep task: {}", e))??
    };
    if pending.is_empty() {
        return Ok(0);
    }

    let client =
        CloudClient::new(&creds_snapshot.cloud_base_url).with_bearer(&unlocked.pi_auth_token);

    let mut total_stored: u32 = 0;
    for batch in pending.chunks(BATCH_LIMIT) {
        let mut wire = Vec::with_capacity(batch.len());
        // charge_id → (session_ts, wrapped key b64) for the ack loop.
        let mut by_id = std::collections::HashMap::new();
        for session in batch {
            let summary: ChargeSessionSummary = charging::summarize(session);
            let points =
                charging::downsample_points(charging::session_points(session), MAX_BLOB_POINTS);
            let tags = tag_map.get(&summary.id).cloned().unwrap_or_default();
            let cost = cost_map.get(&summary.id).cloned();
            let mutable = if tags.is_empty() && cost.is_none() {
                None
            } else {
                Some(ChargeMutable {
                    tags,
                    cost_override: cost.map(|(amount, currency)| CostOverride { amount, currency }),
                })
            };
            let enc = encrypt::encrypt_charge(
                &summary,
                &points,
                mutable.as_ref(),
                &unlocked.pi_key,
                &creds_snapshot.user_id,
                &creds_snapshot.pi_id,
            )
            .with_context(|| format!("encrypt charge {}", summary.id))?;
            by_id.insert(
                enc.charge_id.clone(),
                (summary.id, enc.wrapped_charge_key_b64.clone()),
            );
            wire.push(UploadCharge {
                charge_id: enc.charge_id,
                charge_blob: enc.charge_blob_b64,
                wrapped_charge_key: enc.wrapped_charge_key_b64,
                summary_ciphertext: enc.summary_ciphertext_b64,
                mutable_ciphertext: enc.mutable_ciphertext_b64,
            });
        }

        let body = UploadBody {
            pi_id: creds_snapshot.pi_id.clone(),
            charges: wire,
        };
        let resp = client
            .post_json_bearer("/api/pi/charges", &body)
            .await
            .map_err(|e| anyhow!("charge upload POST: {}", e))?;
        let status = resp.status();

        if status.as_u16() == 401 {
            warn!("charge upload: 401, wiping credentials");
            state.handle_remote_revoke().await;
            return Err(anyhow!("auth rejected; pi unpaired"));
        }
        if status.as_u16() == 403 {
            let body_text = resp.text().await.unwrap_or_default();
            if body_text.contains("user_suspended") {
                *state.last_upload_error.lock().await = Some("user_suspended".to_string());
                return Err(anyhow!("user_suspended; charge uploads paused"));
            }
            warn!("charge upload: 403, wiping credentials");
            state.handle_remote_revoke().await;
            return Err(anyhow!("auth rejected; pi unpaired"));
        }
        if status.as_u16() == 409 {
            let body_text = resp.text().await.unwrap_or_default();
            if body_text.contains("pi_key_stale") {
                // The route sweep owns the rekey-poll flow; just bail and
                // let the next sweep (post-rekey) retry charges.
                return Err(anyhow!("pi_key_stale; awaiting rekey"));
            }
            return Err(anyhow!("charge upload: HTTP 409 body={}", body_text));
        }
        if !status.is_success() {
            let body_text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("charge upload: HTTP {} body={}", status, body_text));
        }

        let parsed: UploadResponse = resp.json().await.context("parse charge upload response")?;
        let now_unix = now_ms() / 1000;
        let mut consent_required = false;
        let mut storage_full = false;
        for result in &parsed.results {
            let Some((session_ts, wrapped_b64)) = by_id.get(&result.charge_id) else {
                continue;
            };
            match result.status.as_str() {
                "stored" | "duplicate" => {
                    if result.status == "stored" {
                        total_stored += 1;
                    }
                    if let Err(e) = store.charge_upload_mark(
                        *session_ts,
                        &result.charge_id,
                        wrapped_b64,
                        now_unix,
                    ) {
                        warn!("charge_upload_mark failed for {}: {}", session_ts, e);
                    }
                    // The payload carried the latest local mutable state;
                    // matching dirty rows are now redundant.
                    if let Some(at) = dirty.get(&session_ts.to_string()) {
                        let _ = store.clear_mutable_dirty("charge", &session_ts.to_string(), *at);
                    }
                }
                "rejected_too_large" => {
                    warn!("charge upload: rejected_too_large for {} (permanent skip)", session_ts);
                    if let Err(e) = store.charge_upload_mark(
                        *session_ts,
                        &result.charge_id,
                        wrapped_b64,
                        PERMANENT_SKIP_SENTINEL,
                    ) {
                        warn!("charge_upload_mark(skip) failed for {}: {}", session_ts, e);
                    }
                }
                "rejected_storage_full" => storage_full = true,
                "rejected_consent_required" => consent_required = true,
                other => warn!("charge upload: unexpected status `{}`", other),
            }
        }

        if total_stored > 0 {
            state.hub.broadcast(
                "cloud_charge_upload",
                &serde_json::json!({ "uploaded": total_stored }),
            );
        }
        if consent_required {
            // Sessions stay queued; the user must accept the v2 consent
            // text in the web UI. Surfaced via /api/cloud/status.
            *state.last_upload_error.lock().await = Some("charge_consent_required".to_string());
            info!("charge upload: consent_required; pausing charge sweep");
            break;
        }
        if storage_full {
            *state.last_upload_error.lock().await = Some("storage_full".to_string());
            break;
        }
    }

    Ok(total_stored)
}
