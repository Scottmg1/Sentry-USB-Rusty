//! Two-way mutable-state sync: drive/charge tags, charge cost
//! overrides, and the per-Pi rate config, with the cloud as rendezvous
//! and last-writer-wins on timestamps.
//!
//! PUSH: drain the local `mutable_dirty` queue (written by every
//! user-facing tag/cost/rate setter), encrypt each item under its
//! content key, POST to `/api/pi/sync/mutables`. `not_found` (deleted in
//! cloud / never uploaded) and `stale` (server newer; the pull will
//! overwrite us) both clear the dirty row.
//!
//! PULL: GET `/api/pi/sync/changes?sinceMs=<cursor>` for web-side writes
//! scoped to this Pi's uploads, decrypt (the Pi holds piKey), apply via
//! the `*_from_sync` store setters (no echo loop), advance the cursor in
//! the drives `meta` table.
//!
//! Object deletions never propagate in either direction.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use sentryusb_cloud_crypto::aad;
use sentryusb_drives::{grouper, schema};

use crate::client::CloudClient;
use crate::credentials_store::UnlockedCreds;
use crate::encrypt::{self, ChargeMutable, CostOverride};
use crate::state::CloudStateInner;

const CURSOR_META_KEY: &str = "cloud_sync_cursor_ms";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PushItem {
    kind: &'static str,
    id: String,
    ciphertext: Option<String>,
    changed_at_ms: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PushBody {
    pi_id: String,
    items: Vec<PushItem>,
}

#[derive(Deserialize)]
struct PushResponse {
    results: Vec<PushResult>,
}

#[derive(Deserialize)]
struct PushResult {
    kind: String,
    id: String,
    status: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangesResponse {
    next_cursor_ms: i64,
    truncated: bool,
    routes: Vec<RouteChange>,
    charges: Vec<ChargeChange>,
    rate_config: Option<RateConfigChange>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteChange {
    route_id: String,
    tags_ciphertext: Option<String>,
    wrapped_route_key: String,
    updated_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChargeChange {
    charge_id: String,
    mutable_ciphertext: Option<String>,
    wrapped_charge_key: String,
    updated_at_ms: i64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RateConfigChange {
    ciphertext: Option<String>,
    updated_at_ms: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteKeysBody {
    pi_id: String,
    route_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteKeysResponse {
    items: Vec<RouteKeyItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteKeyItem {
    route_id: String,
    wrapped_route_key: String,
}

/// One full sync pass: push local dirty mutables, then pull web-side
/// changes. Either half failing fails the pass (retried on next sweep).
pub async fn run_once(state: Arc<CloudStateInner>) -> Result<()> {
    let creds_snapshot = {
        let g = state.creds.lock().await;
        match g.as_ref() {
            Some(c) => c.clone(),
            None => return Ok(()),
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

    push_dirty(&state, &client, &creds_snapshot.user_id, &creds_snapshot.pi_id, &unlocked.pi_key)
        .await
        .context("sync push")?;
    pull_changes(&state, &client, &creds_snapshot.user_id, &creds_snapshot.pi_id, &unlocked.pi_key)
        .await
        .context("sync pull")?;
    Ok(())
}

/// drive_key → member files, plus the reverse file → drive_key map.
fn drive_maps(
    state: &CloudStateInner,
) -> Result<(HashMap<String, Vec<String>>, HashMap<String, String>)> {
    let pairs = state
        .store
        .with_route_summaries(|summaries| grouper::drive_key_file_map(summaries))?;
    let mut by_key = HashMap::with_capacity(pairs.len());
    let mut by_file = HashMap::new();
    for (key, files) in pairs {
        for f in &files {
            by_file.insert(f.clone(), key.clone());
        }
        by_key.insert(key, files);
    }
    Ok((by_key, by_file))
}

async fn push_dirty(
    state: &Arc<CloudStateInner>,
    client: &CloudClient,
    user_id: &str,
    pi_id: &str,
    pi_key: &[u8; 32],
) -> Result<()> {
    let store = state.store.clone();
    let dirty = store.dirty_mutables()?;
    if dirty.is_empty() {
        return Ok(());
    }

    let (by_key, _) = drive_maps(state)?;
    let charge_uploads = store.charge_uploads_map()?;

    let mut items: Vec<PushItem> = Vec::new();
    // routeId → (dirty kind, dirty key, changed_at) so per-route acks can
    // resolve which dirty drive row they belong to. A drive's dirty row
    // clears only when every member route acked.
    let mut route_owner: HashMap<String, (String, i64)> = HashMap::new();
    let mut drive_pending_routes: HashMap<String, usize> = HashMap::new();
    // Non-drive items resolve 1:1 — remember (kind, key, changed_at) by id.
    let mut direct_owner: HashMap<(String, String), (String, String, i64)> = HashMap::new();

    for (kind, key, changed_at) in &dirty {
        match kind.as_str() {
            "drive" => {
                let Some(files) = by_key.get(key) else {
                    // Drive no longer exists locally (deleted/regrouped):
                    // nothing to push, drop the dirty row.
                    let _ = store.clear_mutable_dirty(kind, key, *changed_at);
                    continue;
                };
                let tags = store.get_drive_tags(key).unwrap_or_default();
                let file_refs: Vec<&str> = files.iter().map(|s| s.as_str()).collect();
                let infos = store.route_sync_info_for_files(&file_refs)?;
                // Backfill wrapped keys for member routes uploaded before
                // the cache column existed.
                let missing: Vec<String> = files
                    .iter()
                    .filter_map(|f| match infos.get(f) {
                        Some((Some(rid), None, true)) => Some(rid.clone()),
                        _ => None,
                    })
                    .collect();
                let fetched = if missing.is_empty() {
                    HashMap::new()
                } else {
                    fetch_route_keys(state, client, pi_id, &missing).await?
                };

                let mut pushed_any = false;
                for f in files {
                    let Some((Some(rid), cached_key, uploaded)) = infos.get(f) else {
                        continue;
                    };
                    if !uploaded {
                        // Not in the cloud yet. Route uploads don't carry
                        // tags, so the dirty row survives (see below) and
                        // the first sync pass after the upload pushes them.
                        continue;
                    }
                    let wrapped_b64 = match cached_key.clone().or_else(|| fetched.get(rid).cloned())
                    {
                        Some(k) => k,
                        None => continue, // cloud doesn't know it either
                    };
                    let route_key = match encrypt::unwrap_content_key(
                        pi_key,
                        &wrapped_b64,
                        &aad::route_key(user_id, pi_id, rid),
                    ) {
                        Ok(k) => k,
                        Err(e) => {
                            warn!("sync push: unwrap routeKey {} failed: {}", rid, e);
                            continue;
                        }
                    };
                    let ciphertext = if tags.is_empty() {
                        None
                    } else {
                        Some(encrypt::seal_json_b64(
                            &route_key,
                            &aad::route_tags(user_id, pi_id, rid),
                            &tags,
                        )?)
                    };
                    items.push(PushItem {
                        kind: "route",
                        id: rid.clone(),
                        ciphertext,
                        changed_at_ms: *changed_at,
                    });
                    route_owner.insert(rid.clone(), (key.clone(), *changed_at));
                    *drive_pending_routes.entry(key.clone()).or_insert(0) += 1;
                    pushed_any = true;
                }
                if !pushed_any {
                    // Every member route is still un-uploaded (or unknown).
                    // Keep the dirty row if any member might upload later;
                    // drop it when no member is upload-eligible at all
                    // (e.g. a Tessie-only drive — never uploaded).
                    let any_eligible = files.iter().any(|f| matches!(infos.get(f), Some((_, _, false))));
                    if !any_eligible {
                        let _ = store.clear_mutable_dirty("drive", key, *changed_at);
                    }
                }
            }
            "charge" => {
                let Ok(ts) = key.parse::<i64>() else {
                    let _ = store.clear_mutable_dirty(kind, key, *changed_at);
                    continue;
                };
                let Some((charge_id, wrapped_b64, uploaded_at)) = charge_uploads.get(&ts) else {
                    // Session not uploaded yet — the upload payload will
                    // carry this state; keep the dirty row until then
                    // (charges.rs clears it on stored/duplicate).
                    continue;
                };
                if *uploaded_at < 0 {
                    // Permanently skipped session — nothing to sync to.
                    let _ = store.clear_mutable_dirty(kind, key, *changed_at);
                    continue;
                }
                let charge_key = match encrypt::unwrap_content_key(
                    pi_key,
                    wrapped_b64,
                    &aad::charge_key(user_id, pi_id, charge_id),
                ) {
                    Ok(k) => k,
                    Err(e) => {
                        warn!("sync push: unwrap chargeKey {} failed: {}", charge_id, e);
                        continue;
                    }
                };
                let tags = store.get_charge_tags(ts).unwrap_or_default();
                let cost = store.get_charge_cost(ts).unwrap_or_default();
                let mutable = ChargeMutable {
                    tags,
                    cost_override: cost.map(|(amount, currency)| CostOverride { amount, currency }),
                };
                let ciphertext =
                    if mutable.tags.is_empty() && mutable.cost_override.is_none() {
                        None
                    } else {
                        Some(encrypt::seal_json_b64(
                            &charge_key,
                            &aad::charge_mutable(user_id, pi_id, charge_id),
                            &mutable,
                        )?)
                    };
                items.push(PushItem {
                    kind: "charge",
                    id: charge_id.clone(),
                    ciphertext,
                    changed_at_ms: *changed_at,
                });
                direct_owner.insert(
                    ("charge".into(), charge_id.clone()),
                    ("charge".into(), key.clone(), *changed_at),
                );
            }
            "rate" => {
                let Some(access) = state.rate_config.as_ref() else {
                    let _ = store.clear_mutable_dirty(kind, key, *changed_at);
                    continue;
                };
                let doc = access.load_doc();
                let ciphertext = encrypt::seal_json_b64(
                    pi_key,
                    &aad::rate_config(user_id, pi_id),
                    &doc,
                )?;
                items.push(PushItem {
                    kind: "rateConfig",
                    id: pi_id.to_string(),
                    ciphertext: Some(ciphertext),
                    changed_at_ms: *changed_at,
                });
                direct_owner.insert(
                    ("rateConfig".into(), pi_id.to_string()),
                    ("rate".into(), key.clone(), *changed_at),
                );
            }
            other => {
                warn!("sync push: unknown dirty kind `{}`", other);
                let _ = store.clear_mutable_dirty(kind, key, *changed_at);
            }
        }
    }

    if items.is_empty() {
        return Ok(());
    }

    // Server caps at 200 items per call.
    for chunk in items.chunks(200) {
        let body = PushBody {
            pi_id: pi_id.to_string(),
            items: chunk
                .iter()
                .map(|i| PushItem {
                    kind: i.kind,
                    id: i.id.clone(),
                    ciphertext: i.ciphertext.clone(),
                    changed_at_ms: i.changed_at_ms,
                })
                .collect(),
        };
        let resp = client
            .post_json_bearer("/api/pi/sync/mutables", &body)
            .await
            .map_err(|e| anyhow!("sync push POST: {}", e))?;
        let resp = CloudClient::classify(resp).await.map_err(|e| {
            anyhow!("sync push rejected: {}", e)
        })?;
        let parsed: PushResponse = resp.json().await.context("parse sync push response")?;

        for r in &parsed.results {
            // applied / stale / not_found / rejected_too_large all clear
            // the dirty row: applied means the cloud has it, stale means
            // the cloud is newer (the pull will bring it down), not_found
            // means there's nothing to sync to, and rejected_too_large
            // will never succeed on retry.
            let done = matches!(
                r.status.as_str(),
                "applied" | "stale" | "not_found" | "rejected_too_large"
            );
            if !done {
                continue;
            }
            if r.kind == "route" {
                if let Some((drive_key, changed_at)) = route_owner.get(&r.id) {
                    let remaining = drive_pending_routes.get_mut(drive_key).unwrap();
                    *remaining -= 1;
                    if *remaining == 0 {
                        let _ = store.clear_mutable_dirty("drive", drive_key, *changed_at);
                    }
                }
            } else if let Some((kind, key, changed_at)) =
                direct_owner.get(&(r.kind.clone(), r.id.clone()))
            {
                let _ = store.clear_mutable_dirty(kind, key, *changed_at);
            }
        }
    }

    debug!("sync push complete: {} items", items.len());
    Ok(())
}

/// Backfill wrappedRouteKeys for routes uploaded before the local cache
/// column existed. Caches every fetched key.
async fn fetch_route_keys(
    state: &Arc<CloudStateInner>,
    client: &CloudClient,
    pi_id: &str,
    route_ids: &[String],
) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for chunk in route_ids.chunks(200) {
        let body = RouteKeysBody {
            pi_id: pi_id.to_string(),
            route_ids: chunk.to_vec(),
        };
        let resp = client
            .post_json_bearer("/api/pi/sync/route-keys", &body)
            .await
            .map_err(|e| anyhow!("route-keys POST: {}", e))?;
        let resp = CloudClient::classify(resp)
            .await
            .map_err(|e| anyhow!("route-keys rejected: {}", e))?;
        let parsed: RouteKeysResponse = resp.json().await.context("parse route-keys response")?;
        for item in parsed.items {
            // Cache for next time.
            if let Ok(Some((file, _))) =
                state.store.route_sync_info_by_cloud_id(&item.route_id)
            {
                let _ = state
                    .store
                    .set_cloud_wrapped_route_key(&file, &item.wrapped_route_key);
            }
            out.insert(item.route_id, item.wrapped_route_key);
        }
    }
    Ok(out)
}

async fn pull_changes(
    state: &Arc<CloudStateInner>,
    client: &CloudClient,
    user_id: &str,
    pi_id: &str,
    pi_key: &[u8; 32],
) -> Result<()> {
    let store = state.store.clone();

    // Local dirty rows newer than an incoming change win locally — the
    // push (this pass or the next) carries them up; LWW resolves on the
    // server side.
    let dirty: HashMap<(String, String), i64> = store
        .dirty_mutables()
        .unwrap_or_default()
        .into_iter()
        .map(|(kind, key, at)| ((kind, key), at))
        .collect();

    loop {
        let cursor: i64 = store
            .with_locked_conn(|conn| schema::meta_get(conn, CURSOR_META_KEY))?
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);

        let resp = client
            .get_bearer(&format!("/api/pi/sync/changes?sinceMs={}", cursor))
            .await
            .map_err(|e| anyhow!("sync pull GET: {}", e))?;
        let resp = CloudClient::classify(resp)
            .await
            .map_err(|e| anyhow!("sync pull rejected: {}", e))?;
        let parsed: ChangesResponse = resp.json().await.context("parse sync changes")?;

        let has_changes = !parsed.routes.is_empty()
            || !parsed.charges.is_empty()
            || parsed.rate_config.is_some();
        if has_changes {
            let (_, by_file) = drive_maps(state)?;

            for rc in &parsed.routes {
                let Ok(Some((file, cached_key))) = store.route_sync_info_by_cloud_id(&rc.route_id)
                else {
                    continue; // route no longer on this Pi — ignore
                };
                if cached_key.is_none() {
                    let _ = store.set_cloud_wrapped_route_key(&file, &rc.wrapped_route_key);
                }
                let Some(drive_key) = by_file.get(&file) else { continue };
                if dirty
                    .get(&("drive".to_string(), drive_key.clone()))
                    .is_some_and(|at| *at > rc.updated_at_ms)
                {
                    continue; // local edit is newer
                }
                let tags: Vec<String> = match &rc.tags_ciphertext {
                    None => Vec::new(),
                    Some(ct) => {
                        let route_key = match encrypt::unwrap_content_key(
                            pi_key,
                            &rc.wrapped_route_key,
                            &aad::route_key(user_id, pi_id, &rc.route_id),
                        ) {
                            Ok(k) => k,
                            Err(e) => {
                                warn!("sync pull: unwrap routeKey {} failed: {}", rc.route_id, e);
                                continue;
                            }
                        };
                        match encrypt::open_json_b64(
                            &route_key,
                            &aad::route_tags(user_id, pi_id, &rc.route_id),
                            ct,
                        ) {
                            Ok(t) => t,
                            Err(e) => {
                                warn!("sync pull: open tags {} failed: {}", rc.route_id, e);
                                continue;
                            }
                        }
                    }
                };
                if let Err(e) = store.set_drive_tags_from_sync(drive_key, &tags) {
                    warn!("sync pull: set drive tags {} failed: {}", drive_key, e);
                }
            }

            for cc in &parsed.charges {
                let Ok(Some(session_ts)) = store.charge_session_ts_for_cloud_id(&cc.charge_id)
                else {
                    continue; // never uploaded from this Pi / locally deleted
                };
                if dirty
                    .get(&("charge".to_string(), session_ts.to_string()))
                    .is_some_and(|at| *at > cc.updated_at_ms)
                {
                    continue;
                }
                let mutable: ChargeMutable = match &cc.mutable_ciphertext {
                    None => ChargeMutable { tags: Vec::new(), cost_override: None },
                    Some(ct) => {
                        let charge_key = match encrypt::unwrap_content_key(
                            pi_key,
                            &cc.wrapped_charge_key,
                            &aad::charge_key(user_id, pi_id, &cc.charge_id),
                        ) {
                            Ok(k) => k,
                            Err(e) => {
                                warn!("sync pull: unwrap chargeKey {} failed: {}", cc.charge_id, e);
                                continue;
                            }
                        };
                        match encrypt::open_json_b64(
                            &charge_key,
                            &aad::charge_mutable(user_id, pi_id, &cc.charge_id),
                            ct,
                        ) {
                            Ok(m) => m,
                            Err(e) => {
                                warn!("sync pull: open mutable {} failed: {}", cc.charge_id, e);
                                continue;
                            }
                        }
                    }
                };
                // "Home" is a reserved, derived tag — strip any casing pulled from
                // the cloud so a synced envelope can't reintroduce it as a stored tag.
                // Reserved tags are DERIVED on read and must never be stored.
                // The local write handler already enforces this; a tag arriving
                // from the cloud has to clear the same bar, or a peer running an
                // older build could reintroduce one as a real stored tag. Uses
                // the shared vocabulary so this can't drift from the API's copy.
                let synced_tags: Vec<String> =
                    sentryusb_drives::charging::strip_reserved_tags(mutable.tags.clone());
                if let Err(e) = store.set_charge_tags_from_sync(session_ts, &synced_tags) {
                    warn!("sync pull: set charge tags {} failed: {}", session_ts, e);
                }
                let cost = mutable.cost_override.map(|c| (c.amount, c.currency));
                if let Err(e) = store.set_charge_cost_from_sync(session_ts, cost) {
                    warn!("sync pull: set charge cost {} failed: {}", session_ts, e);
                }
            }

            if let Some(rcfg) = &parsed.rate_config {
                if let (Some(ct), Some(access)) = (&rcfg.ciphertext, state.rate_config.as_ref()) {
                    if dirty
                        .get(&("rate".to_string(), String::new()))
                        .is_some_and(|at| *at > rcfg.updated_at_ms)
                    {
                        // Local rate edit is newer; push wins.
                    } else {
                        match encrypt::open_json_b64::<serde_json::Value>(
                            pi_key,
                            &aad::rate_config(user_id, pi_id),
                            ct,
                        ) {
                            Ok(doc) => {
                                if let Err(e) = access.store_doc(&doc) {
                                    warn!("sync pull: store rate config failed: {}", e);
                                }
                            }
                            Err(e) => warn!("sync pull: open rate config failed: {}", e),
                        }
                    }
                }
            }

            info!(
                "sync pull applied: {} route tag(s), {} charge mutable(s){}",
                parsed.routes.len(),
                parsed.charges.len(),
                if parsed.rate_config.is_some() { ", rate config" } else { "" },
            );
        }

        store.with_locked_conn(|conn| {
            schema::meta_set(conn, CURSOR_META_KEY, &parsed.next_cursor_ms.to_string())
        })?;

        if !parsed.truncated {
            break;
        }
    }

    Ok(())
}
