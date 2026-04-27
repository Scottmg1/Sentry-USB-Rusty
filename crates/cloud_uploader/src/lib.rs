

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

pub const DEFAULT_CLOUD_BASE_URL: &str = "https://sentryusb.com";

pub const DEFAULT_CREDENTIALS_PATH: &str = "/root/.sentryusb/cloud-credentials.json";

#[derive(Clone)]
pub struct CloudUploader {
    inner: Arc<state::CloudStateInner>,
}

impl CloudUploader {

    pub async fn spawn(store: Arc<DriveStore>, hub: Hub, on_complete: Arc<Notify>) -> Arc<Self> {
        Self::spawn_with_options(store, hub, on_complete, SpawnOptions::default()).await
    }

    pub async fn spawn_with_options(
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

        me.inner.bootstrap_load_credentials().await;

        let inner_for_sweep = inner.clone();
        tokio::spawn(async move {
            uploader::run_sweep_loop(inner_for_sweep).await;
        });

        me
    }

    pub async fn status(&self) -> CloudStatus {
        self.inner.snapshot_status().await
    }

    pub async fn pair_begin(&self, code: &str) -> anyhow::Result<()> {
        pairing::run(self.inner.clone(), code.to_string()).await
    }

    pub async fn pair_cancel(&self) {
        self.inner.cancel_pairing().await;
    }

    pub async fn unpair(&self) -> anyhow::Result<()> {
        self.inner.unpair().await
    }

    pub fn nudge(&self) {
        self.inner.notify.notify_one();
    }

    pub fn pending_queue(&self, limit: i64) -> anyhow::Result<Vec<db_ext::QueueEntry>> {
        db_ext::pending_queue(&self.inner.store, limit)
    }
}

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
