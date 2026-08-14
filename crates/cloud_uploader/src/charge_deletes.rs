//! Drains locally queued charge deletions after uploads. Settled rows receive
//! one final delete before removal to cover late commits from timed-out uploads.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::client::CloudClient;
use crate::credentials_store::UnlockedCreds;
use crate::state::CloudStateInner;

const BATCH_LIMIT: usize = 200;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeleteBody {
    pi_id: String,
    charge_ids: Vec<String>,
}

#[derive(Deserialize)]
struct DeleteResponse {
    results: Vec<DeleteResult>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteResult {
    charge_id: String,
    status: String,
}

/// One drain pass. Returns the number of sessions newly settled.
pub async fn sweep_once(state: Arc<CloudStateInner>) -> Result<u32> {
    let creds_snapshot = {
        let g = state.creds.lock().await;
        match g.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(0),
        }
    };

    let store = state.store.clone();
    let rows = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || store.charge_delete_outbox_all())
            .await
            .map_err(|e| anyhow!("charge delete prep task: {}", e))??
    };
    if rows.is_empty() {
        return Ok(0);
    }

    let unlocked = UnlockedCreds::unlock(&creds_snapshot).or_else(|_| {
        let serial = std::env::var("SENTRYCLOUD_DEV_SERIAL")
            .map(|s| s.into_bytes())
            .map_err(|_| anyhow!("unlock failed and SENTRYCLOUD_DEV_SERIAL unset"))?;
        UnlockedCreds::unlock_with_serial(&creds_snapshot, &serial)
    })?;
    let client =
        CloudClient::new(&creds_snapshot.cloud_base_url).with_bearer(&unlocked.pi_auth_token);

    // charge_id -> (session_ts, was_settled).
    let by_id: std::collections::HashMap<String, (i64, bool)> = rows
        .iter()
        .map(|(ts, id, settled)| (id.clone(), (*ts, *settled)))
        .collect();

    let mut settled_now: u32 = 0;
    for batch in rows.chunks(BATCH_LIMIT) {
        let body = DeleteBody {
            pi_id: creds_snapshot.pi_id.clone(),
            charge_ids: batch.iter().map(|(_, id, _)| id.clone()).collect(),
        };
        let resp = client
            .post_json_bearer("/api/pi/charges/bulk-delete", &body)
            .await
            .map_err(|e| anyhow!("charge delete POST: {}", e))?;
        let status = resp.status();

        match status.as_u16() {
            401 => {
                warn!("charge delete: 401, wiping credentials");
                state.handle_remote_revoke().await;
                return Err(anyhow!("auth rejected; pi unpaired"));
            }
            403 => {
                let body_text = resp.text().await.unwrap_or_default();
                if body_text.contains("user_suspended") {
                    // Keep rows queued until the account is reinstated.
                    *state.last_upload_error.lock().await = Some("user_suspended".to_string());
                    return Err(anyhow!("user_suspended; charge deletes paused"));
                }
                warn!("charge delete: 403, wiping credentials");
                state.handle_remote_revoke().await;
                return Err(anyhow!("auth rejected; pi unpaired"));
            }
            // Keep rows queued when the endpoint is unavailable.
            404 | 405 | 501 => {
                info!("charge delete: endpoint unavailable (HTTP {}); keeping queue", status);
                return Ok(0);
            }
            s if !(200..300).contains(&s) => {
                let body_text = resp.text().await.unwrap_or_default();
                return Err(anyhow!("charge delete: HTTP {} body={}", status, body_text));
            }
            _ => {}
        }

        let parsed: DeleteResponse = resp.json().await.map_err(|e| anyhow!("parse charge delete response: {}", e))?;
        let now_unix = crate::state::now_ms() / 1000;
        for result in &parsed.results {
            let Some((session_ts, was_settled)) = by_id.get(&result.charge_id) else {
                continue;
            };
            match result.status.as_str() {
                // `missing` includes unknown, foreign, and already-deleted rows.
                "deleted" | "missing" => {
                    let res = if *was_settled {
                        store.charge_delete_remove(*session_ts)
                    } else {
                        settled_now += 1;
                        store.charge_delete_ack(*session_ts, now_unix)
                    };
                    if let Err(e) = res {
                        warn!("charge delete outbox update failed for {}: {}", session_ts, e);
                    }
                }
                other => warn!("charge delete: unexpected status `{}`", other),
            }
        }
    }

    if settled_now > 0 {
        state.hub.broadcast(
            "cloud_charge_delete",
            &serde_json::json!({ "deleted": settled_now }),
        );
    }
    Ok(settled_now)
}
