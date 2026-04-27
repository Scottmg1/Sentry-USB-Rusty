//! Internal state and the public `CloudStatus` snapshot.
//!
//! State is held behind a `tokio::sync::Mutex` so the async background
//! tasks (sweep, pairing) and the local API handlers can all touch it
//! without busy-waiting. The mutex guard never crosses an `await` that
//! does network I/O — we explicitly drop it before any HTTP call.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex, Notify};
use tracing::warn;

use sentryusb_cloud_crypto::credentials::CloudCredentialsV1;
use sentryusb_drives::DriveStore;
use sentryusb_ws::Hub;

/// Pairing-machine state for `/api/cloud/pair/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingState {
    Idle,
    Handshaking,
    Polling,
    Complete,
    Error,
}

/// Snapshot returned by `GET /api/cloud/status`. Constructed on demand —
/// don't cache; counters move during the construction.
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
}

/// The mutex-protected slice of state. Held in `Arc<CloudStateInner>` so
/// every background task and API handler shares a single source of truth.
pub struct CloudStateInner {
    pub store: Arc<DriveStore>,
    pub hub: Hub,
    pub notify: Arc<Notify>,
    pub cloud_base_url: String,
    pub credentials_path: String,

    /// Loaded credentials when paired. None when unpaired.
    pub creds: Mutex<Option<CloudCredentialsV1>>,

    /// Pairing UI state.
    pub pairing: Mutex<PairingProgress>,

    /// Cancellation flag set by `/api/cloud/pair/cancel`. The pairing
    /// task polls this between awaitable boundaries.
    pub pairing_cancel: Mutex<Option<Arc<Notify>>>,

    /// Last successful sweep wall-clock time (millis since UNIX epoch).
    pub last_upload_at_ms: AtomicI64,

    /// Last sweep error string (interned via Mutex<String>).
    pub last_upload_error: Mutex<Option<String>>,

    /// Total routes successfully uploaded since process start. Display
    /// hint only — the source of truth is `cloud_uploaded_at IS NOT NULL`
    /// in the local DB.
    pub total_uploaded: AtomicU64,
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
    ) -> Self {
        CloudStateInner {
            store,
            hub,
            notify,
            cloud_base_url,
            credentials_path,
            creds: Mutex::new(None),
            pairing: Mutex::new(PairingProgress::default()),
            pairing_cancel: Mutex::new(None),
            last_upload_at_ms: AtomicI64::new(0),
            last_upload_error: Mutex::new(None),
            total_uploaded: AtomicU64::new(0),
        }
    }

    /// On boot, attempt to read `cloud-credentials.json`. Sets the paired
    /// state if successful; otherwise stays unpaired and the user has to
    /// run pairing.
    pub async fn bootstrap_load_credentials(&self) {
        match sentryusb_cloud_crypto::credentials::load(&self.credentials_path) {
            Ok(creds) => {
                let mut guard = self.creds.lock().await;
                *guard = Some(creds);
            }
            Err(_) => {
                // File missing or unparseable. Stay unpaired silently —
                // a fresh install hits this on every boot.
            }
        }
    }

    /// Cheap-snapshot status assembly.
    pub async fn snapshot_status(&self) -> CloudStatus {
        let creds_guard = self.creds.lock().await;
        let pairing_guard = self.pairing.lock().await;

        let pending_route_count = self
            .store
            .with_locked_conn(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM routes WHERE cloud_uploaded_at IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
            });

        let last_upload_ms = self.last_upload_at_ms.load(Ordering::Relaxed);
        let last_upload_at = if last_upload_ms > 0 {
            chrono::DateTime::<Utc>::from_timestamp_millis(last_upload_ms)
        } else {
            None
        };

        let last_upload_error = self.last_upload_error.lock().await.clone();

        match creds_guard.as_ref() {
            Some(c) => CloudStatus {
                paired: true,
                user_id: Some(c.user_id.clone()),
                pi_id: Some(c.pi_id.clone()),
                paired_at: Some(c.paired_at),
                last_upload_at,
                last_upload_error,
                pending_route_count,
                total_uploaded_route_count: self.total_uploaded.load(Ordering::Relaxed) as i64,
                dek_rotation_generation: Some(c.dek_rotation_generation),
                cloud_base_url: c.cloud_base_url.clone(),
                pairing_state: pairing_guard.state,
                pairing_error: pairing_guard.error.clone(),
            },
            None => CloudStatus {
                paired: false,
                user_id: None,
                pi_id: None,
                paired_at: None,
                last_upload_at,
                last_upload_error,
                pending_route_count,
                total_uploaded_route_count: self.total_uploaded.load(Ordering::Relaxed) as i64,
                dek_rotation_generation: None,
                cloud_base_url: self.cloud_base_url.clone(),
                pairing_state: pairing_guard.state,
                pairing_error: pairing_guard.error.clone(),
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
            // Best-effort: secure-delete the file.
            if let Err(e) =
                sentryusb_cloud_crypto::credentials::secure_delete(&self.credentials_path)
            {
                warn!("cloud unpair: secure_delete failed: {}", e);
            }
        }
        *creds_guard = None;
        drop(creds_guard);

        // Reset cached counters / errors.
        self.last_upload_at_ms.store(0, Ordering::Relaxed);
        *self.last_upload_error.lock().await = None;
        self.total_uploaded.store(0, Ordering::Relaxed);

        self.hub.broadcast(
            "cloud_status_changed",
            &serde_json::json!({ "paired": false }),
        );
        Ok(())
    }

    /// Replace credentials atomically (used by pairing-success and rekey).
    pub async fn set_credentials(&self, new_creds: CloudCredentialsV1) -> anyhow::Result<()> {
        sentryusb_cloud_crypto::credentials::save_atomic(&self.credentials_path, &new_creds)?;
        let mut guard = self.creds.lock().await;
        *guard = Some(new_creds);
        self.hub.broadcast(
            "cloud_status_changed",
            &serde_json::json!({ "paired": true }),
        );
        Ok(())
    }

    /// Wipe credentials in response to a 401/403 from the cloud — the Pi
    /// has been revoked from the user's `/settings → Devices`. Same end
    /// state as user-initiated unpair, but emits a different ws event so
    /// the UI can distinguish "you clicked unpair" from "the cloud said
    /// goodbye."
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

        self.last_upload_at_ms.store(0, Ordering::Relaxed);
        *self.last_upload_error.lock().await = Some("revoked".to_string());

        self.hub.broadcast(
            "cloud_status_changed",
            &serde_json::json!({ "paired": false, "reason": "revoked" }),
        );
    }
}

/// Helper: now() in millis since UNIX epoch.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
