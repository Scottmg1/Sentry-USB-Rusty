use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// A broadcast message sent to all connected WebSocket clients.
#[derive(Debug, Clone, Serialize)]
pub struct Message {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub data: serde_json::Value,
}

/// WebSocket hub for broadcasting messages to all connected clients.
///
/// Uses `tokio::sync::broadcast` internally — clients subscribe and receive
/// messages without the Go-style register/unregister channel dance.
#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<Vec<u8>>,
    client_count: Arc<AtomicUsize>,
}

impl Hub {
    /// Creates a new Hub with a 256-message broadcast buffer.
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(256);
        Hub {
            tx,
            client_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Broadcasts a typed message to all connected clients.
    pub fn broadcast(&self, msg_type: &str, data: impl Serialize) {
        let msg = Message {
            msg_type: msg_type.to_string(),
            data: match serde_json::to_value(data) {
                Ok(v) => v,
                Err(e) => {
                    warn!("failed to serialize broadcast message: {}", e);
                    return;
                }
            },
        };
        let bytes = match serde_json::to_vec(&msg) {
            Ok(b) => b,
            Err(e) => {
                warn!("failed to marshal broadcast message: {}", e);
                return;
            }
        };
        // Ignore send errors (no receivers = nobody to send to)
        let _ = self.tx.send(bytes);
    }

    /// Subscribe to receive broadcast messages. Returns a receiver that yields
    /// serialized JSON bytes for each broadcast.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<u8>> {
        self.client_count.fetch_add(1, Ordering::Relaxed);
        let count = self.client_count.load(Ordering::Relaxed);
        debug!("WebSocket client connected ({} total)", count);
        self.tx.subscribe()
    }

    /// Called when a client disconnects to track the count.
    pub fn client_disconnected(&self) {
        let prev = self.client_count.fetch_sub(1, Ordering::Relaxed);
        debug!("WebSocket client disconnected ({} total)", prev - 1);
    }

    /// Returns the current number of connected clients.
    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::Relaxed)
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}
