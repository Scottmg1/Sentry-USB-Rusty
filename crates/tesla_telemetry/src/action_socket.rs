//! IPC bridge so external processes (`sentryusb-ble-action`) can ask
//! the running telemetry daemon to perform a one-shot BLE action via
//! its existing `PersistentSession` instead of opening a separate
//! connection.
//!
//! ## Why this exists
//!
//! Before this socket, every `awake_start` keep-awake cycle had to:
//!   1. Stop the telemetry daemon (`systemctl stop sentryusb-telemetry`)
//!      so its persistent BLE connection didn't fight ble-action for
//!      the car's slot.
//!   2. Run `sentryusb-ble-action <verb>`, which opens its own BLE
//!      connection, handshakes, sends the action, disconnects.
//!   3. Restart telemetry (at the end of the archive cycle via
//!      `awake_stop`).
//!
//! Effect: live data went dark for the entire archive duration (could
//! be 30+ minutes during a big upload). Users complained, reasonably.
//!
//! With the rewrite both telemetry and ble-action are now in-process
//! Rust on top of btleplug, so we can collapse the two into a single
//! BLE owner — the daemon. ble-action checks for this socket first;
//! if present, it pipes the verb in, the daemon dispatches via its
//! already-warm PersistentSession (no reconnect, no slot juggling),
//! and ble-action exits with the result.
//!
//! ## Fallback path preserved
//!
//! If the daemon isn't running (BLE telemetry not enabled, or daemon
//! crashed), the socket file is absent and ble-action falls back to
//! the original direct-BLE path. Users who don't run telemetry keep
//! working exactly as before.
//!
//! ## Protocol
//!
//! Line-delimited text — trivial to debug with `nc -U` from a root
//! shell:
//! ```text
//!   client → server : "<verb>\n"
//!   server → client : "OK\n"  or  "OK <payload>\n"  or  "ERR <message>\n"
//! ```
//!
//! Where `<verb>` is one of:
//!   `wake`, `sentry-on`, `sentry-off`, `charge-port-open`,
//!   `charge-port-close`, `keep-accessory-on`, `keep-accessory-off`,
//!   `charge-start`, `charge-stop`, `set-charging-amps:<n>`,
//!   `set-charge-limit:<n>`,
//!   `session-info`, `drive-state`.
//!
//! Most verbs are fire-and-forget actions that reply `OK` / `ERR …`.
//! Two are *query* verbs that return data over the same connection,
//! handled in `main::handle_action_request` (not `parse_verb`, which
//! only maps action verbs):
//!   * `session-info` — pairing probe: `OK` (paired),
//!     `ERR NOT_PAIRED` (car rejected our key), or
//!     `ERR UNREACHABLE: <why>` (couldn't reach the car).
//!   * `drive-state` — current gear: `OK P` / `OK R` / `OK N` / `OK D`,
//!     or `ERR UNREACHABLE: <why>` when the car is asleep / unreachable
//!     or reported no concrete gear.
//!
//! Socket lives at `/tmp/sentryusb-telemetry.sock` with mode 0600 —
//! only root can connect (matches `sentryusb-ble-action`'s usual
//! invocation context).

use std::time::Duration;

use anyhow::{Context, Result};
use sentryusb_tesla_ble::actions::{self, ActionPayload};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Socket path. Picked /tmp because:
///   * `/var/run` may be tmpfs without write access early in boot.
///   * `/tmp` is guaranteed writable; lives across the daemon's
///     lifetime (we clean it up on next start).
///   * Matches the existing `/tmp/ble_radio_owner` lock-file pattern.
pub const SOCKET_PATH: &str = "/tmp/sentryusb-telemetry.sock";

/// One IPC request received from the socket. The `reply` oneshot is
/// the daemon's main loop's way to send the per-action result back
/// to the socket connection handler. The `Ok` payload is the line
/// written after `OK ` — empty for fire-and-forget actions, the gear
/// token for `drive-state`, etc.
pub struct ActionRequest {
    pub verb: String,
    pub reply: oneshot::Sender<Result<String>>,
}

/// Spawn the socket listener. Cleans up any stale socket from a
/// previous (crashed) run, binds fresh, chmods to 0600, and accepts
/// in a loop. Each accepted connection runs in its own task that
/// forwards a single request to the main loop via `action_tx`.
pub fn spawn(action_tx: mpsc::Sender<ActionRequest>) {
    // Stale socket from a previous run: remove. UnixListener::bind
    // refuses to bind an existing path.
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

    // 0o600 — only root can connect. Both halves of the IPC
    // (`sentryusb-ble-action` invoked from `awake_start`, and us)
    // run as root, so this is correct. Locks out any non-root
    // process (e.g. webui shouldn't be issuing keep-awake actions
    // directly — they go via the API which already runs as root).
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
                    // Brief sleep so we don't busy-loop on a
                    // permanently-broken socket. The most likely
                    // cause of repeated accept failure is fd
                    // exhaustion or someone deleting the socket
                    // file; either way, 1s is a fine retry rate.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            };
            let tx = action_tx.clone();
            tokio::spawn(handle_connection(stream, tx));
        }
    });
}

/// Process one client connection: read the verb, forward it through
/// the action channel, write the reply, hang up.
async fn handle_connection(stream: UnixStream, action_tx: mpsc::Sender<ActionRequest>) {
    if let Err(e) = process(stream, action_tx).await {
        debug!("action socket connection ended: {:#}", e);
    }
}

async fn process(stream: UnixStream, action_tx: mpsc::Sender<ActionRequest>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // One line in, one line out. We don't keep the connection open
    // for additional requests — each action is its own connection,
    // matching how ble-action invokes us.
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

    // 60 s is generous but bounded — covers worst-case cold-session
    // setup (scan + connect + handshake + action) on a busy car.
    // ble-action's own outer timeout will catch hangs beyond this.
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
            // Empty payload → bare "OK" (fire-and-forget actions and
            // session-info "paired"). Non-empty → "OK <payload>" (e.g.
            // drive-state's gear token). The payload is a single short
            // token, so no newline-stripping needed.
            let line = if payload.is_empty() {
                "OK\n".to_string()
            } else {
                format!("OK {}\n", payload)
            };
            let _ = write_half.write_all(line.as_bytes()).await;
        }
        Err(e) => {
            // Compress the error to one line — protocol is
            // line-delimited and embedded newlines would confuse the
            // client.
            let msg = format!("ERR {:#}\n", e).replace('\n', " ");
            // The above replace turns OUR trailing \n into a space.
            // Re-add it.
            let msg = format!("{}\n", msg.trim_end());
            let _ = write_half.write_all(msg.as_bytes()).await;
        }
    }
    Ok(())
}

/// Translate a wire verb string into the matching typed
/// `ActionPayload` from `tesla_ble::actions`. Single source of truth
/// for the verb→action mapping — also referenced (in spirit) by the
/// CLI's argument parser via `actions::parse_verb`.
pub fn parse_verb(verb: &str) -> Result<ActionPayload> {
    actions::parse_verb(verb)
}
