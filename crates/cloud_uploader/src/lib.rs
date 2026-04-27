//! SentryCloud upload pipeline for the Pi.
//!
//! Lifecycle: `CloudUploader::spawn(...)` returns a clonable `Arc<CloudUploader>`
//! that the daemon stores in `AppState`. Background tasks run on the existing
//! tokio runtime: an upload-sweep loop driven by `tokio::sync::Notify` (woken
//! by `Processor::do_process` completion) plus a 600 s safety-net timer.
//!
//! Pairing, encryption, rekey, and revoke-detection are coordinated through
//! this single handle so the local `/api/cloud/*` routes don't need direct
//! access to internal modules.
//!
//! See the project plan and `ENCRYPTION.md` for the full design.

pub mod client;
pub mod credentials_store;
pub mod db_ext;
pub mod encrypt;
pub mod pairing;
pub mod rekey;
pub mod state;
pub mod uploader;

pub use sentryusb_cloud_crypto as crypto;

use std::sync::Arc;
use tokio::sync::Notify;

use sentryusb_drives::DriveStore;
use sentryusb_ws::Hub;

pub use crate::state::{CloudStatus, CloudStateInner, PairingState};

/// Default cloud base URL. Overridable via `CloudUploader::spawn_with_base_url`.
pub const DEFAULT_CLOUD_BASE_URL: &str = "https://sentryusb.com";

/// Default credentials path on the Pi.
pub const DEFAULT_CREDENTIALS_PATH: &str = "/root/.sentryusb/cloud-credentials.json";

/// Public handle the daemon holds. Cloneable Arc.
#[derive(Clone)]
pub struct CloudUploader {
    inner: Arc<state::CloudStateInner>,
}

impl CloudUploader {
    /// Spawn the background tasks. `on_complete` is the same `Notify` clone
    /// passed to `Processor::new(...)`; the processor calls
    /// `notify.notify_one()` after `do_process` finishes, waking our sweep.
    pub fn spawn(store: Arc<DriveStore>, hub: Hub, on_complete: Arc<Notify>) -> Arc<Self> {
        Self::spawn_with_options(store, hub, on_complete, SpawnOptions::default())
    }

    /// Test/dev entry point that lets callers override the cloud base URL
    /// and credentials path. Production calls `spawn(...)` which uses the
    /// `DEFAULT_*` constants.
    pub fn spawn_with_options(
        store: Arc<DriveStore>,
        hub: Hub,
        on_complete: Arc<Notify>,
        opts: SpawnOptions,
    ) -> Arc<Self> {
        let inner = Arc::new(state::CloudStateInner::new(
            store,
            hub,
            on_complete,
            opts.cloud_base_url,
            opts.credentials_path,
        ));
        let me = Arc::new(CloudUploader { inner: inner.clone() });

        // Boot path: try to load an existing credentials file. If present,
        // we transition to Paired and the uploader sweep can run.
        me.inner.bootstrap_load_credentials();

        // Spawn the upload-sweep loop. Owns its own clone of the state.
        let inner_for_sweep = inner.clone();
        tokio::spawn(async move {
            uploader::run_sweep_loop(inner_for_sweep).await;
        });

        me
    }

    /// Snapshot of the current cloud-pairing + upload status. Cheap; reads
    /// a few atomics + the credentials lock.
    pub async fn status(&self) -> CloudStatus {
        self.inner.snapshot_status().await
    }

    /// Begin a pairing handshake using the 6-digit code displayed in the
    /// browser's "Pair a Pi" modal. Errors if already paired (caller should
    /// unpair first), if a pairing is already in flight, or if the cloud
    /// rejects the code.
    pub async fn pair_begin(&self, code: &str) -> anyhow::Result<()> {
        pairing::run(self.inner.clone(), code.to_string()).await
    }

    /// Cancel an in-flight pairing attempt. Idempotent.
    pub async fn pair_cancel(&self) {
        self.inner.cancel_pairing().await;
    }

    /// Forget the cloud pairing. Overwrites + unlinks `cloud-credentials.json`
    /// and transitions to Unpaired. The cloud's bearer auth keeps working
    /// until the user revokes from `/settings → Devices`; the uploader's
    /// 401/403 handler closes that loop on its own if revoked first.
    pub async fn unpair(&self) -> anyhow::Result<()> {
        self.inner.unpair().await
    }

    /// Wake the upload-sweep loop now (out-of-band of the Notify wired into
    /// `Processor`). Used by the dev-only `POST /api/cloud/upload-now`.
    pub fn nudge(&self) {
        self.inner.notify.notify_one();
    }
}

/// Optional overrides for `spawn_with_options`.
pub struct SpawnOptions {
    pub cloud_base_url: String,
    pub credentials_path: String,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        SpawnOptions {
            cloud_base_url: DEFAULT_CLOUD_BASE_URL.to_string(),
            credentials_path: DEFAULT_CREDENTIALS_PATH.to_string(),
        }
    }
}
