//! Root-only IPC for running one-shot BLE actions through the telemetry
//! daemon's existing `PersistentSession`. Clients fall back to direct BLE when
//! the socket is unavailable.
//!
//! Line-delimited protocol:
//! ```text
//!   client → server : "<verb>\n"
//!   server → client : "OK\n"  or  "OK <payload>\n"  or  "ERR <message>\n"
//! ```
//!
//! Action verbs:
//!   `wake`, `sentry-on`, `sentry-off`, `charge-port-open`,
//!   `charge-port-close`, `keep-accessory-on`, `keep-accessory-off`,
//!   `charge-start`, `charge-stop`, `set-charging-amps:<n>`,
//!   `set-charge-limit:<n>`,
//!   `session-info`, `drive-state`.
//!
//! `session-info` returns pairing status; `drive-state` returns `P`, `R`, `N`,
//! or `D`. The socket is mode 0600.

use std::time::Duration;

use anyhow::{Context, Result};
use sentryusb_tesla_ble::actions::{self, ActionPayload};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Socket path, removed and recreated on daemon startup.
pub const SOCKET_PATH: &str = "/tmp/sentryusb-telemetry.sock";

/// IPC request. An empty success payload produces a bare `OK` response.
pub struct ActionRequest {
    pub verb: String,
    pub reply: oneshot::Sender<Result<String>>,
}

/// Spawns the mode-0600 listener and forwards one request per connection.
pub fn spawn(action_tx: mpsc::Sender<ActionRequest>) {
    // `UnixListener::bind` refuses stale paths.
    let _ = std::fs::remove_file(SOCKET_PATH);

    let listener = match UnixListener::bind(SOCKET_PATH) {
        Ok(l) => l,
        Err(e) => {
            warn!(
                "could not bind {} — keep-awake IPC unavailable, ble-action will fall back to direct BLE: {}",
                SOCKET_PATH, e
            );
            return;
        }
    };

    // Both peers run as root; other processes must not issue BLE actions.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) = std::fs::set_permissions(
            SOCKET_PATH,
            std::fs::Permissions::from_mode(0o600),
        ) {
            warn!("could not chmod {} to 0600: {}", SOCKET_PATH, e);
        }
    }

    info!(
        "action socket listening at {} (BLE actions via IPC enabled)",
        SOCKET_PATH
    );

    tokio::spawn(async move {
        loop {
            let (stream, _peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    warn!("action socket accept() failed: {}", e);
                    // Avoid a busy loop on persistent accept failures.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let tx = action_tx.clone();
            tokio::spawn(handle_connection(stream, tx));
        }
    });
}

/// Reads one verb, forwards it, writes one reply, and closes.
async fn handle_connection(stream: UnixStream, action_tx: mpsc::Sender<ActionRequest>) {
    if let Err(e) = process(stream, action_tx).await {
        debug!("action socket connection ended: {:#}", e);
    }
}

async fn process(stream: UnixStream, action_tx: mpsc::Sender<ActionRequest>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // One request per connection.
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("read verb from client")?;
    let verb = line.trim().to_string();
    if verb.is_empty() {
        let _ = write_half.write_all(b"ERR empty verb\n").await;
        return Ok(());
    }

    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = action_tx
        .send(ActionRequest {
            verb: verb.clone(),
            reply: reply_tx,
        })
        .await
    {
        let msg = format!("ERR submit failed: {}\n", e);
        let _ = write_half.write_all(msg.as_bytes()).await;
        return Ok(());
    }

    // Covers cold scan, connection, handshake, and action setup.
    let response = match tokio::time::timeout(Duration::from_secs(60), reply_rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => Err(anyhow::anyhow!(
            "daemon dropped the reply channel (probably mid-shutdown)"
        )),
        Err(_) => Err(anyhow::anyhow!(
            "daemon did not respond within 60s"
        )),
    };

    match response {
        Ok(payload) => {
            // Empty payloads produce bare `OK`; query payloads follow it.
            let line = if payload.is_empty() {
                "OK\n".to_string()
            } else {
                format!("OK {}\n", payload)
            };
            let _ = write_half.write_all(line.as_bytes()).await;
        }
        Err(e) => {
            // Keep line-delimited errors on one line.
            let msg = format!("ERR {:#}\n", e).replace('\n', " ");
            let msg = format!("{}\n", msg.trim_end());
            let _ = write_half.write_all(msg.as_bytes()).await;
        }
    }
    Ok(())
}

/// Converts a wire verb to its typed action payload.
pub fn parse_verb(verb: &str) -> Result<ActionPayload> {
    actions::parse_verb(verb)
}
