//! PTY over WebSocket for web terminal.
//!
//! Matches the Go `server/api/terminal.go` contract:
//!  1. Client sends `{"type":"auth","username":"...","password":"..."}` as first message.
//!  2. Server validates credentials against /etc/shadow via Perl `crypt(3)`.
//!  3. On success, spawns `su -l <user>` with a PTY and bridges I/O over the WebSocket.
//!  4. Client sends `{"type":"input","data":"..."}` for keystrokes.
//!  5. Client sends `{"type":"resize","cols":N,"rows":N}` for window resize.
//!  6. Failed auth attempts are rate-limited per remote IP (5 failures / 5 minutes).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{ConnectInfo, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::router::AppState;

const RATE_WINDOW: Duration = Duration::from_secs(5 * 60);
const RATE_MAX_FAILS: usize = 5;

fn rate_store() -> &'static Mutex<HashMap<String, Vec<Instant>>> {
    static STORE: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();
    STORE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn rate_limited(ip: &str) -> bool {
    let mut map = match rate_store().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let cutoff = Instant::now().checked_sub(RATE_WINDOW);
    if let Some(times) = map.get_mut(ip) {
        times.retain(|t| cutoff.map(|c| *t > c).unwrap_or(true));
        if times.is_empty() {
            map.remove(ip);
            return false;
        }
        return times.len() >= RATE_MAX_FAILS;
    }
    false
}

fn record_failure(ip: &str) {
    let mut map = match rate_store().lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    map.entry(ip.to_string()).or_default().push(Instant::now());
}

#[derive(Deserialize)]
struct AuthMsg {
    #[serde(rename = "type")]
    ty: String,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct ClientMsg {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    cols: Option<u16>,
    #[serde(default)]
    rows: Option<u16>,
}

/// GET /api/terminal — PTY over WebSocket
pub async fn handle_terminal(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(_state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_terminal_ws(socket, addr))
}

fn send_msg_text(ty: &str, data: &str) -> Message {
    Message::Text(
        serde_json::json!({"type": ty, "data": data})
            .to_string()
            .into(),
    )
}

async fn handle_terminal_ws(socket: WebSocket, addr: SocketAddr) {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let ip = addr.ip().to_string();
    let (mut sender, mut receiver) = socket.split();

    if rate_limited(&ip) {
        warn!("[terminal] Rate limited auth attempt from {}", ip);
        let _ = sender
            .send(send_msg_text(
                "error",
                "Too many failed attempts. Try again later.",
            ))
            .await;
        return;
    }

    // Step 1: wait for auth
    let auth_raw = match receiver.next().await {
        Some(Ok(Message::Text(t))) => t,
        _ => {
            let _ = sender
                .send(send_msg_text("error", "Failed to read auth message"))
                .await;
            return;
        }
    };
    let auth: AuthMsg = match serde_json::from_str(&auth_raw) {
        Ok(a) => a,
        Err(_) => {
            let _ = sender
                .send(send_msg_text("error", "Invalid auth message"))
                .await;
            return;
        }
    };
    let username = auth.username.unwrap_or_default();
    let password = auth.password.unwrap_or_default();
    if auth.ty != "auth" || username.is_empty() || password.is_empty() {
        let _ = sender
            .send(send_msg_text("error", "Invalid auth message"))
            .await;
        return;
    }

    // Step 2: validate credentials
    if !validate_credentials(&username, &password).await {
        record_failure(&ip);
        warn!("[terminal] Failed auth for user {:?} from {}", username, ip);
        let _ = sender
            .send(send_msg_text("auth_failed", "Invalid username or password"))
            .await;
        return;
    }

    let _ = sender.send(send_msg_text("auth_ok", "")).await;

    // Step 3: spawn PTY
    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            warn!("[terminal] openpty failed: {}", e);
            let _ = sender
                .send(send_msg_text(
                    "error",
                    &format!("Failed to start terminal: {}", e),
                ))
                .await;
            return;
        }
    };

    let mut cmd = CommandBuilder::new("su");
    cmd.arg("-l");
    cmd.arg(&username);
    cmd.env("TERM", "xterm-256color");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            warn!("[terminal] spawn failed: {}", e);
            let _ = sender
                .send(send_msg_text(
                    "error",
                    &format!("Failed to start terminal: {}", e),
                ))
                .await;
            return;
        }
    };

    // Drop the slave fd from the parent so that closing the master hangs up the
    // session (portable_pty drops it when we drop pair.slave).
    drop(pair.slave);

    info!("[terminal] session started for {} from {}", username, ip);

    let mut reader = match pair.master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            warn!("[terminal] try_clone_reader failed: {}", e);
            let _ = child.kill();
            return;
        }
    };
    let mut writer = match pair.master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            warn!("[terminal] take_writer failed: {}", e);
            let _ = child.kill();
            return;
        }
    };
    let master = std::sync::Arc::new(std::sync::Mutex::new(pair.master));

    // Channel: blocking PTY reader thread -> async WS sender
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(32);

    let read_handle = tokio::task::spawn_blocking(move || {
        use std::io::Read;
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Forward PTY -> WebSocket
    let send_task = tokio::spawn(async move {
        while let Some(chunk) = out_rx.recv().await {
            let text = String::from_utf8_lossy(&chunk).into_owned();
            if sender.send(send_msg_text("output", &text)).await.is_err() {
                break;
            }
        }
        // Best-effort close frame
        let _ = sender
            .send(send_msg_text("exit", "Terminal session ended"))
            .await;
    });

    // WebSocket -> PTY + resize
    let master_for_recv = master.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(t) => {
                    let parsed: ClientMsg = match serde_json::from_str(&t) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    match parsed.ty.as_str() {
                        "input" => {
                            if let Some(data) = parsed.data {
                                use std::io::Write;
                                if writer.write_all(data.as_bytes()).is_err() {
                                    break;
                                }
                            }
                        }
                        "resize" => {
                            if let (Some(cols), Some(rows)) = (parsed.cols, parsed.rows) {
                                if let Ok(m) = master_for_recv.lock() {
                                    let _ = m.resize(portable_pty::PtySize {
                                        rows,
                                        cols,
                                        pixel_width: 0,
                                        pixel_height: 0,
                                    });
                                }
                            }
                        }
                        "ping" => {
                            // Pong is handled by the WS send task via output channel;
                            // no-op here matches the Go server's heartbeat semantics.
                        }
                        _ => {}
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // When either side finishes (client disconnect, PTY EOF), tear down:
    //  - kill child (sends SIGHUP via PTY teardown)
    //  - drop master (closes PTY, wakes blocking reader)
    tokio::select! {
        _ = send_task => {}
        _ = recv_task => {}
    }

    let _ = child.kill();
    let _ = child.wait();
    // Drop master explicitly to unblock the reader thread if it's still alive.
    drop(master);
    let _ = read_handle.await;

    info!("[terminal] session ended for {} from {}", username, ip);
}

// Perl script reads password from stdin, verifies against /etc/shadow via crypt(3).
// Username passed as $ARGV[0]. Matches Go implementation byte-for-byte.
const VERIFY_PASSWORD_SCRIPT: &str = r#"use strict;
use warnings;
my $username = $ARGV[0];
my $password = <STDIN>;
chomp $password;
open(my $fh, '<', '/etc/shadow') or exit 1;
while (<$fh>) {
    chomp;
    my @parts = split(/:/, $_, -1);
    if ($parts[0] eq $username) {
        my $stored = $parts[1];
        exit 1 if !$stored || $stored eq '*' || $stored eq '!!' || $stored =~ /^!/;
        exit(crypt($password, $stored) eq $stored ? 0 : 1);
    }
}
exit 1;"#;

async fn validate_credentials(username: &str, password: &str) -> bool {
    use tokio::io::AsyncWriteExt;
    use tokio::process::Command;

    // Reject shell-metacharacter bait in username; `su` will reject anyway but
    // fail fast and keep logs clean.
    if username.is_empty() || username.contains(|c: char| c.is_whitespace() || c == ':') {
        return false;
    }

    let mut cmd = Command::new("perl");
    cmd.arg("-e")
        .arg(VERIFY_PASSWORD_SCRIPT)
        .arg(username)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            warn!("[terminal] perl spawn failed: {}", e);
            return false;
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let input = format!("{}\n", password);
        if stdin.write_all(input.as_bytes()).await.is_err() {
            return false;
        }
        drop(stdin);
    }

    let status = match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(s)) => s,
        _ => {
            warn!("[terminal] credential check timed out for {:?}", username);
            return false;
        }
    };
    status.success()
}
