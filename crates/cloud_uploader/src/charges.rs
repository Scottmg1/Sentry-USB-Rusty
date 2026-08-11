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
use sentryusb_drives::schema::{
    self, CHARGE_SWEEP_CURSOR_KEY, CHARGE_SWEEP_FULL_DATE_KEY,
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

#[cfg(test)]
mod tests {
    use super::*;
    use sentryusb_drives::charging::ChargeRow;

    const NOW: i64 = 1_000_000;

    fn session(start: i64, len: i64) -> Vec<ChargeRow> {
        (0..len)
            .map(|i| ChargeRow { ts: start + i * 60, ..Default::default() })
            .collect()
    }

    /// Long finished — outside the gap window.
    fn old(start: i64) -> Vec<ChargeRow> {
        session(start, 3)
    }

    #[test]
    fn settled_history_advances_to_the_newest_session() {
        let sessions = vec![old(1_000), old(50_000), old(100_000)];
        assert_eq!(
            sweep_frontier(&sessions, NOW, |_| true),
            Some(100_000),
            "everything sent and closed — start at the newest session"
        );
    }

    #[test]
    fn holds_at_the_oldest_unsent_session() {
        let sessions = vec![old(1_000), old(50_000), old(100_000)];
        // The middle one failed to upload; the cursor must not pass it.
        assert_eq!(
            sweep_frontier(&sessions, NOW, |ts| ts != 50_000),
            Some(50_000)
        );
    }

    /// A session still inside the gap window may still grow, so its rows
    /// must stay in scope.
    #[test]
    fn holds_at_a_still_open_session() {
        let mut sessions = vec![old(1_000)];
        sessions.push(session(NOW - 60, 2));
        assert_eq!(sweep_frontier(&sessions, NOW, |_| true), Some(NOW - 60));
    }

    #[test]
    fn no_sessions_leaves_the_cursor_alone() {
        assert_eq!(sweep_frontier(&[], NOW, |_| true), None);
    }

    /// The frontier is a session START, never a row inside one — session
    /// identity is that timestamp.
    #[test]
    fn frontier_is_always_a_session_start() {
        let sessions = vec![old(1_000), old(50_000)];
        let f = sweep_frontier(&sessions, NOW, |ts| ts != 50_000).unwrap();
        assert!(sessions.iter().any(|s| s[0].ts == f));
    }
}

/// Where the next sweep may start: the oldest session that is still open
/// or still unsent, so a transient upload failure is retried rather than
/// skipped. Falls back to the newest session's start once everything is
/// settled. Always a session's FIRST ts — never a point inside one.
fn sweep_frontier(
    sessions: &[Vec<charging::ChargeRow>],
    now_secs: i64,
    handled: impl Fn(i64) -> bool,
) -> Option<i64> {
    let settled = |s: &Vec<charging::ChargeRow>| {
        let closed = s.last().is_some_and(|l| now_secs - l.ts > SESSION_GAP_SECS);
        closed && s.first().is_some_and(|f| handled(f.ts))
    };
    sessions
        .iter()
        .filter(|s| !s.is_empty())
        .find(|s| !settled(s))
        .or_else(|| sessions.iter().rfind(|s| !s.is_empty()))
        .and_then(|s| s.first())
        .map(|r| r.ts)
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
    let pending = {
        let store = store.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
            // Raw cursor string doubles as a CAS token: an import deletes
            // the key mid-sweep, and overwriting that deletion with a
            // stale frontier would hide restored history until the next
            // daily full scan.
            let (from, full_scan, cursor_token) = store.with_read_conn(|conn| -> Result<_> {
                let token = schema::meta_get(conn, CHARGE_SWEEP_CURSOR_KEY)?;
                if schema::meta_get(conn, CHARGE_SWEEP_FULL_DATE_KEY)?.as_deref()
                    != Some(today.as_str())
                {
                    return Ok((0, true, token));
                }
                let cursor = token
                    .as_deref()
                    .and_then(|s| s.parse::<i64>().ok())
                    .unwrap_or(0);
                Ok((cursor, false, token))
            })?;

            // Sessions in scope all start at or after `from`, so uploads
            // before it can never be consulted.
            let uploads = store
                .charge_uploads_map_since(from)
                .context("charge_uploads_map")?;
            let rows = store
                .with_read_conn(|conn| -> Result<_> { charging::load_charge_rows(conn, from, None) })
                .context("load charge rows")?;
            let now_secs = now_ms() / 1000;
            let sessions = charging::group_sessions(rows);

            // `>` not `>=`, matching group_sessions' `<=` continuation:
            // at exactly the gap a straggler row still joins the session,
            // so calling it closed there could upload one about to grow.
            let closed = |s: &Vec<charging::ChargeRow>| {
                s.last().is_some_and(|l| now_secs - l.ts > SESSION_GAP_SECS)
            };
            let handled = |s: &Vec<charging::ChargeRow>| {
                s.first().is_some_and(|f| uploads.contains_key(&f.ts))
            };

            // Computed BEFORE the uploads below, so it can only ever be
            // too conservative.
            let frontier = sweep_frontier(&sessions, now_secs, |ts| uploads.contains_key(&ts));
            store.with_locked_conn(|conn| -> Result<()> {
                if schema::meta_get(conn, CHARGE_SWEEP_CURSOR_KEY)? != cursor_token {
                    // Import raced us; keep its reset. FULL_DATE stays
                    // unstamped too, so the next sweep rescans from 0.
                    return Ok(());
                }
                if let Some(ts) = frontier {
                    schema::meta_set(conn, CHARGE_SWEEP_CURSOR_KEY, &ts.to_string())?;
                }
                if full_scan {
                    schema::meta_set(conn, CHARGE_SWEEP_FULL_DATE_KEY, &today)?;
                }
                Ok(())
            })?;

            // Delete-outbox exclusion is a POST gate only — it must
            // NOT feed sweep_frontier's handled predicate above, or the
            // cursor could advance past a re-imported session until the
            // daily full scan.
            let outboxed: std::collections::HashSet<i64> = store
                .charge_delete_outbox_all()
                .unwrap_or_default()
                .into_iter()
                .map(|(ts, _, _)| ts)
                .collect();
            let pending: Vec<Vec<charging::ChargeRow>> = sessions
                .into_iter()
                .filter(|s| {
                    closed(s)
                        && !handled(s)
                        && !s.first().is_some_and(|f| outboxed.contains(&f.ts))
                })
                .collect();
            Ok(pending)
        })
        .await
        .map_err(|e| anyhow!("charge prep task: {}", e))??
    };
    if pending.is_empty() {
        return Ok(0);
    }

    // Tags, costs and dirty rows only for the sessions actually going up.
    let (tag_map, cost_map, dirty) = {
        let store = store.clone();
        let ids: Vec<i64> = pending.iter().filter_map(|s| s.first().map(|r| r.ts)).collect();
        tokio::task::spawn_blocking(move || {
            let mut tag_map = std::collections::HashMap::new();
            let mut cost_map = std::collections::HashMap::new();
            for id in ids {
                if let Ok(tags) = store.get_charge_tags(id) {
                    if !tags.is_empty() {
                        tag_map.insert(id, tags);
                    }
                }
                if let Ok(Some(cost)) = store.get_charge_cost(id) {
                    cost_map.insert(id, cost);
                }
            }
            // Cleared on stored/duplicate so the sync push doesn't re-send
            // the same state.
            let dirty: std::collections::HashMap<String, i64> = store
                .dirty_mutables()
                .unwrap_or_default()
                .into_iter()
                .filter(|(kind, _, _)| kind == "charge")
                .map(|(_, key, at)| (key, at))
                .collect();
            (tag_map, cost_map, dirty)
        })
        .await
        .map_err(|e| anyhow!("charge maps task: {}", e))?
    };

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
                    // A local delete may have queued this session after
                    // prep; marking it uploaded now would leave the Pi
                    // claiming "uploaded" over an empty cloud once the
                    // delete stage (later this sweep) retires it.
                    if store.charge_delete_outbox_contains(*session_ts).unwrap_or(false) {
                        continue;
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
