use std::sync::Arc;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, Notify};
use tracing::warn;

use sentryusb_cloud_crypto::credentials::CloudCredentialsV1;
use sentryusb_drives::DriveStore;
use sentryusb_ws::Hub;

use crate::db_ext;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    Idle,
    Handshaking,
    Polling,
    Complete,
    Error,
}

#[derive(Debug, Clone, Serialize)]
pub struct CloudStatus {
    pub paired: bool,
    pub user_id: Option<String>,
    pub pi_id: Option<String>,
    pub paired_at: Option<DateTime<Utc>>,
    pub last_upload_at: Option<DateTime<Utc>>,
    pub last_upload_error: Option<String>,
    pub pending_route_count: i64,
    pub total_uploaded_route_count: i64,
    pub dek_rotation_generation: Option<u32>,
    pub cloud_base_url: String,
    pub pairing_state: PairingState,
    pub pairing_error: Option<String>,

    /// Set when an on-disk credentials file existed at startup but
    /// failed to load (corrupt JSON, partial write, wrong perms, etc.).
    /// `paired` will be `false` in this case. UI should surface this
    /// distinctly from "never paired" — the user needs to re-pair.
    pub credentials_load_error: Option<String>,
}

/// Read/write access to the local charging rate-config document (the
/// `charging_*` preference keys as one JSON object). Lives behind a
/// trait because the preferences store is in the api crate, which
/// depends on this crate — the binary wires the implementation in at
/// spawn time.
pub trait RateConfigAccess: Send + Sync {
    fn load_doc(&self) -> serde_json::Value;
    fn store_doc(&self, doc: &serde_json::Value) -> anyhow::Result<()>;
}

pub struct CloudStateInner {
    pub store: Arc<DriveStore>,
    pub hub: Hub,
    pub notify: Arc<Notify>,
    pub cloud_base_url: String,
    pub credentials_path: String,

    /// None when the binary didn't wire a preferences hook (tests).
    /// Rate-config sync is skipped in that case.
    pub rate_config: Option<Arc<dyn RateConfigAccess>>,

    pub creds: Mutex<Option<CloudCredentialsV1>>,

    pub pairing: Mutex<PairingProgress>,

    pub pairing_cancel: Mutex<Option<Arc<Notify>>>,

    pub last_upload_error: Mutex<Option<String>>,

    pub credentials_load_error: Mutex<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct PairingProgress {
    pub state: PairingState,
    pub error: Option<String>,
}

impl Default for PairingProgress {
    fn default() -> Self {
        PairingProgress { state: PairingState::Idle, error: None }
    }
}

impl CloudStateInner {
    pub fn new(
        store: Arc<DriveStore>,
        hub: Hub,
        notify: Arc<Notify>,
        cloud_base_url: String,
        credentials_path: String,
        rate_config: Option<Arc<dyn RateConfigAccess>>,
    ) -> Self {
        CloudStateInner {
            store,
            hub,
            notify,
            cloud_base_url,
            credentials_path,
            rate_config,
            creds: Mutex::new(None),
            pairing: Mutex::new(PairingProgress::default()),
            pairing_cancel: Mutex::new(None),
            last_upload_error: Mutex::new(None),
            credentials_load_error: Mutex::new(None),
        }
    }

    pub async fn bootstrap_load_credentials(&self) {
        match sentryusb_cloud_crypto::credentials::load(&self.credentials_path) {
            Ok(creds) => {
                let mut guard = self.creds.lock().await;
                *guard = Some(creds);
            }
            Err(e) => {
                // Distinguish "no credentials file" (normal new install) from
                // "file exists but won't load" (corruption, partial write,
                // wrong perms). The latter is the failure mode where the Pi
                // silently appears unpaired while the cloud thinks it's still
                // paired — surface it loudly so the user knows to re-pair.
                if std::path::Path::new(&self.credentials_path).exists() {
                    let msg = format!("{}", e);
                    warn!(
                        "cloud credentials file at `{}` exists but failed to load: {}. \
                         Pi will appear unpaired; user must re-pair.",
                        self.credentials_path, msg,
                    );
                    *self.credentials_load_error.lock().await = Some(msg);
                    self.hub.broadcast(
                        "cloud_status_changed",
                        &serde_json::json!({
                            "paired": false,
                            "reason": "credentials_load_failed",
                        }),
                    );
                }
            }
        }
    }

    pub async fn snapshot_status(&self) -> CloudStatus {
        // Settings polls this every second while uploads are pending —
        // both counts run in one blocking-pool hop on the read pool
        // (pending count previously took the WRITER mutex inline on the
        // reactor), and the async guards are not held across it.
        let store = self.store.clone();
        let (pending_route_count, total_uploaded_route_count, last_upload_secs) =
            tokio::task::spawn_blocking(move || {
                let pending = store.with_read_conn(|conn| {
                    conn.query_row(
                        "SELECT count(*) FROM routes WHERE cloud_uploaded_at IS NULL",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                });
                let (total, last) = db_ext::upload_summary(&store);
                (pending, total, last)
            })
            .await
            .unwrap_or((0, 0, None));
        let last_upload_at = last_upload_secs
            .and_then(|s| chrono::DateTime::<Utc>::from_timestamp(s, 0));

        let creds_guard = self.creds.lock().await;
        let pairing_guard = self.pairing.lock().await;
        let last_upload_error = self.last_upload_error.lock().await.clone();
        let credentials_load_error = self.credentials_load_error.lock().await.clone();

        match creds_guard.as_ref() {
            Some(c) => CloudStatus {
                paired: true,
                user_id: Some(c.user_id.clone()),
                pi_id: Some(c.pi_id.clone()),
                paired_at: Some(c.paired_at),
                last_upload_at,
                last_upload_error,
                pending_route_count,
                total_uploaded_route_count,
                dek_rotation_generation: Some(c.dek_rotation_generation),
                cloud_base_url: c.cloud_base_url.clone(),
                pairing_state: pairing_guard.state,
                pairing_error: pairing_guard.error.clone(),
                credentials_load_error: None,
            },
            None => CloudStatus {
                paired: false,
                user_id: None,
                pi_id: None,
                paired_at: None,
                last_upload_at,
                last_upload_error,
                pending_route_count,
                total_uploaded_route_count,
                dek_rotation_generation: None,
                cloud_base_url: self.cloud_base_url.clone(),
                pairing_state: pairing_guard.state,
                pairing_error: pairing_guard.error.clone(),
                credentials_load_error,
            },
        }
    }

    pub async fn cancel_pairing(&self) {
        let cancel = self.pairing_cancel.lock().await.clone();
        if let Some(n) = cancel {
            n.notify_waiters();
        }
        let mut p = self.pairing.lock().await;
        if matches!(p.state, PairingState::Handshaking | PairingState::Polling) {
            *p = PairingProgress {
                state: PairingState::Idle,
                error: Some("cancelled".to_string()),
            };
        }
    }

    pub async fn unpair(&self) -> anyhow::Result<()> {
        let mut creds_guard = self.creds.lock().await;
        if creds_guard.is_some() {

            if let Err(e) =
                sentryusb_cloud_crypto::credentials::secure_delete(&self.credentials_path)
            {
                warn!("cloud unpair: secure_delete failed: {}", e);
            }
        }
        *creds_guard = None;
        drop(creds_guard);

        *self.last_upload_error.lock().await = None;
        *self.credentials_load_error.lock().await = None;

        self.hub.broadcast(
            "cloud_status_changed",
            &serde_json::json!({ "paired": false }),
        );
        Ok(())
    }

    pub async fn set_credentials(&self, new_creds: CloudCredentialsV1) -> anyhow::Result<()> {
        sentryusb_cloud_crypto::credentials::save_atomic(&self.credentials_path, &new_creds)?;
        let mut guard = self.creds.lock().await;
        *guard = Some(new_creds);
        drop(guard);
        *self.credentials_load_error.lock().await = None;
        self.hub.broadcast(
            "cloud_status_changed",
            &serde_json::json!({ "paired": true }),
        );
        Ok(())
    }

    pub async fn handle_remote_revoke(&self) {
        let mut guard = self.creds.lock().await;
        if guard.is_none() {
            return;
        }
        if let Err(e) =
            sentryusb_cloud_crypto::credentials::secure_delete(&self.credentials_path)
        {
            warn!("remote revoke: secure_delete failed: {}", e);
        }
        *guard = None;
        drop(guard);

        *self.last_upload_error.lock().await = Some("revoked".to_string());
        *self.credentials_load_error.lock().await = None;

        self.hub.broadcast(
            "cloud_status_changed",
            &serde_json::json!({ "paired": false, "reason": "revoked" }),
        );
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
