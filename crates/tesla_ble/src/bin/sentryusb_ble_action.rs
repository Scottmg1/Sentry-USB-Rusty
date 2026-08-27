//! One-shot CLI for keep-awake BLE actions.
//!
//! Replaces the `tesla-control wake|sentry-mode|charge-port-open|...`
//! shell-outs in `run/awake_start`. Each invocation tries two paths,
//! in this order:
//!
//!   1. **IPC fast-path** — connect to the running telemetry
//!      daemon's Unix socket at `/tmp/sentryusb-telemetry.sock`,
//!      send `"<verb>\n"`, read `"OK\n"` or `"ERR ...\n"`. The
//!      daemon dispatches the action through its already-warm
//!      PersistentSession. Zero new BLE connections, no slot
//!      handoff, telemetry polling never pauses. This is the
//!      preferred path whenever BLE telemetry is enabled.
//!
//!   2. **Direct-BLE fallback** — open our own PersistentSession,
//!      do the action, exit. Same path as before this binary
//!      learned about the IPC socket. Covers users who don't run
//!      the telemetry daemon (BLE telemetry disabled in settings),
//!      and cold paths where the daemon crashed and hasn't been
//!      restarted yet.
//!
//! Per-invocation overhead:
//!   * IPC path: ~50-300ms (daemon already has a connection)
//!   * Direct path: ~1-2s (scan + handshake + command)
//!
//! Usage:
//!   sentryusb-ble-action <verb>
//!
//! Verbs:
//!   wake               - VEHICLE_SECURITY RKE wake
//!   sentry-on          - turn Sentry Mode on
//!   sentry-off         - turn Sentry Mode off
//!   charge-port-open   - open the charge port
//!   charge-port-close  - close the charge port
//!   charge-start       - start charging
//!   charge-stop        - stop charging
//!   set-charging-amps:N - set charging current (1-80 A)
//!   set-charge-limit:N  - set charge limit (50-100%)
//!   keep-accessory-on  - turn Keep Accessory Power on
//!   keep-accessory-off - turn Keep Accessory Power off
//!   session-info       - pairing probe (see below)
//!   drive-state        - current gear query (see below)
//!   pair               - add-key-to-whitelist request (prompts for NFC tap)
//!   keygen             - generate the BLE P-256 keypair (replaces tesla-keygen)
//!
//! `session-info` is not an action: it prints exactly one stdout token
//! — `PAIRED`, `NOT_PAIRED`, or `UNREACHABLE` — and exits 0 for all
//! three. The API matches on that token (its shell helper only surfaces
//! stdout) and clears the paired marker only on `NOT_PAIRED`. Config
//! errors exit 2 with no token, which the API reads as "couldn't
//! verify", never "unpaired".
//!
//! `drive-state` is also a query, not an action: on success it prints
//! the gear token (`P`/`R`/`N`/`D`) to stdout and exits 0; on any
//! failure (car asleep/unreachable, no concrete gear, config/key error)
//! it exits non-zero with a stderr message. The lock-chime smart mode
//! reads this — exit 0 + `P` means "parked, safe to proceed".
//!
//! Exit codes:
//!   0 success (for session-info: a token was printed)
//!   1 invalid usage
//!   2 config error (missing VIN, missing key file)
//!   3 BLE error (scan/connect/handshake failed)
//!   4 action rejected by car (returns the fault code as stderr line)

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result};
use sentryusb_tesla_ble::{
    actions::{self, ActionPayload},
    keys::KeyPair,
    manager::{PairingStatus, PersistentSession},
    responses::shift_state_token,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{error, info, warn};

const KEY_FILE: &str = "/root/.ble/key_private.pem";
const CONFIG_FILE: &str = "/root/sentryusb.conf";
/// Must match `action_socket::SOCKET_PATH` in the telemetry daemon.
const IPC_SOCKET: &str = "/tmp/sentryusb-telemetry.sock";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,btleplug=warn".into()),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let verb = match std::env::args().nth(1) {
        Some(v) => v,
        None => {
            eprintln!(
                "usage: sentryusb-ble-action <wake|sentry-on|sentry-off|charge-port-open|charge-port-close|keep-accessory-on|keep-accessory-off|charge-start|charge-stop|set-charging-amps:N|set-charge-limit:N|session-info|drive-state|pair|keygen>"
            );
            return ExitCode::from(1);
        }
    };
    // `session-info` is a pairing probe, not an action — it reports a
    // stdout token rather than running a command. Handle it on its own
    // IPC-first / direct-fallback path before the action dispatch.
    if verb == "session-info" {
        return run_session_info().await;
    }
    // `drive-state` is a query too — prints the gear token and uses
    // distinct exit codes, so handle it before the action dispatch.
    if verb == "drive-state" {
        return run_drive_state().await;
    }
    // `pair` is a fire-and-forget add-key-to-whitelist request — it has
    // its own IPC-first / direct-fallback path and exit-code semantics
    // (0 = request delivered, tap your card; 3 = BLE/slot failure), so
    // handle it before the action dispatch too.
    if verb == "pair" {
        return run_pair().await;
    }
    // `keygen` generates the P-256 keypair natively (replaces
    // `tesla-keygen create`). No BLE — pure local key generation.
    if verb == "keygen" {
        return run_keygen();
    }

    // Try the IPC fast-path first. If the telemetry daemon is up,
    // it'll service the action via its already-warm session and
    // return in <300ms — without competing for the BLE slot. If the
    // socket isn't there (telemetry disabled) or the connect fails
    // (daemon crashed), fall back to direct BLE so users without
    // the telemetry daemon keep working.
    match try_via_ipc(verb.as_str()).await {
        Ok(()) => {
            info!("action via daemon IPC: {} OK", verb);
            return ExitCode::SUCCESS;
        }
        Err(IpcError::Unavailable(reason)) => {
            // Expected on systems where telemetry isn't running.
            // Not a warning — this is the design's intended fallback.
            info!(
                "telemetry IPC unavailable ({}), falling back to direct BLE",
                reason
            );
        }
        Err(IpcError::DaemonRejected(msg)) => {
            // Daemon is up but refused the action (e.g. BLE disabled
            // in settings, VIN missing, radio held by something
            // else). These would also fail on the direct path, so
            // exit with the error rather than thrashing the radio.
            error!("daemon refused action: {}", msg);
            return ExitCode::from(3);
        }
    }

    let action = match actions::parse_verb(verb.as_str()) {
        Ok(action) => action,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(1);
        }
    };

    match run(verb.as_str(), action).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!("{e:#}");
            // Map error categories to distinct exit codes so
            // awake_start can choose to retry vs log-and-skip.
            let msg = format!("{e:#}");
            if msg.contains("config") || msg.contains("TESLA_BLE_VIN") || msg.contains("key file")
            {
                ExitCode::from(2)
            } else if msg.contains("fault code") {
                ExitCode::from(4)
            } else {
                ExitCode::from(3)
            }
        }
    }
}

/// Two-axis result for the IPC attempt:
///   * `Unavailable` → no daemon listening; fall back to direct BLE
///   * `DaemonRejected` → daemon is up but said no; surface the
///                       error instead of retrying via direct
///                       (same failure mode would just repeat)
#[derive(Debug)]
enum IpcError {
    Unavailable(String),
    DaemonRejected(String),
}

/// Connect to the telemetry daemon's Unix socket, send the verb,
/// read the response. Tight timeouts on every step so a hung
/// daemon doesn't make us wait minutes before falling back.
async fn try_via_ipc(verb: &str) -> Result<(), IpcError> {
    // Connect with a 1s timeout. The daemon either accepts
    // immediately (it's already in its accept() loop) or doesn't
    // exist — any failure means "not available, fall back."
    let stream = match tokio::time::timeout(
        Duration::from_millis(1000),
        UnixStream::connect(IPC_SOCKET),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {}: {}",
                IPC_SOCKET, e
            )));
        }
        Err(_) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {} timed out",
                IPC_SOCKET
            )));
        }
    };

    let (read_half, mut write_half) = stream.into_split();

    // Send the verb as one line.
    let cmd = format!("{}\n", verb);
    if let Err(e) = write_half.write_all(cmd.as_bytes()).await {
        return Err(IpcError::Unavailable(format!(
            "writing verb: {}",
            e
        )));
    }

    // Read one line of response. 90s wall-clock — generous because
    // the daemon's own 60s per-request timeout plus connect /
    // handshake overhead can push this close to that, but bounded
    // so a stuck daemon doesn't wedge us forever.
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    let read_result = tokio::time::timeout(
        Duration::from_secs(90),
        reader.read_line(&mut line),
    )
    .await;
    match read_result {
        Ok(Ok(0)) => Err(IpcError::Unavailable(
            "daemon closed connection without response".into(),
        )),
        Ok(Ok(_)) => {
            let line = line.trim();
            if line == "OK" {
                Ok(())
            } else if let Some(rest) = line.strip_prefix("ERR ") {
                Err(IpcError::DaemonRejected(rest.to_string()))
            } else {
                Err(IpcError::DaemonRejected(format!(
                    "unexpected response: {:?}",
                    line
                )))
            }
        }
        Ok(Err(e)) => Err(IpcError::Unavailable(format!(
            "read response: {}",
            e
        ))),
        Err(_) => Err(IpcError::Unavailable(
            "daemon response timed out after 90s".into(),
        )),
    }
}

/// `keygen` verb. Generates the P-256 BLE keypair under /root/.ble
/// (PKCS#8 private + SPKI public PEM) via the native generator — the
/// drop-in replacement for `tesla-keygen … create`. Idempotent and
/// safe: a valid pair is left untouched, a missing/malformed public key is
/// derived again from the existing private key, and an invalid private key is
/// never clobbered automatically.
/// Marks the keys freshly generated so the UI knows pairing is pending.
///
/// Exit codes: 0 keys present/created, 2 filesystem/keygen error.
fn run_keygen() -> ExitCode {
    let dir = Path::new("/root/.ble");
    if sentryusb_tesla_ble::keys::load_keypair(dir).is_ok() {
        info!(
            "keygen: keypair already present at {} — leaving it untouched",
            dir.display()
        );
        println!("OK");
        return ExitCode::SUCCESS;
    }
    if dir.join("key_private.pem").exists() {
        return match sentryusb_tesla_ble::keys::ensure_keypair_files(dir) {
            Ok(_) => {
                info!(
                    "keygen: repaired public key from existing private key at {}",
                    dir.display()
                );
                println!("OK");
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!("keygen: existing private key is invalid; refusing to overwrite it: {e:#}");
                ExitCode::from(2)
            }
        };
    }
    if let Err(e) = std::fs::create_dir_all(dir) {
        error!("keygen: creating {}: {e:#}", dir.display());
        return ExitCode::from(2);
    }
    match sentryusb_tesla_ble::keys::generate_keypair(dir) {
        Ok(_) => {
            // Mark not-yet-paired so the web UI doesn't falsely show
            // "BLE Paired" on a fresh keypair (matches the wizard).
            let _ = std::fs::write(dir.join("key_pending_pairing"), "");
            info!("keygen: generated BLE keypair under {}", dir.display());
            println!("OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("keygen: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// `session-info` verb. Prints one pairing token to stdout and exits 0
/// (config errors exit 2 with no token). IPC-first so the probe reuses
/// the telemetry daemon's warm connection; direct fallback covers a
/// disabled/crashed daemon.
async fn run_session_info() -> ExitCode {
    match session_info_via_ipc().await {
        Ok(token) => {
            println!("{token}");
            return ExitCode::SUCCESS;
        }
        Err(IpcError::Unavailable(reason)) => {
            info!(
                "telemetry IPC unavailable ({}), checking pairing via direct BLE",
                reason
            );
        }
        Err(IpcError::DaemonRejected(msg)) => {
            // Daemon answered with something we don't recognise. Don't
            // fall through to a direct attempt (it would just wake the
            // car again and likely repeat) — report unknown as
            // unreachable so the API leaves the marker alone.
            warn!("daemon returned unexpected session-info reply: {}", msg);
            println!("UNREACHABLE");
            return ExitCode::SUCCESS;
        }
    }

    // Direct fallback: open our own one-shot session.
    let (vin, adapter) = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::from(2);
        }
    };
    let keypair = match KeyPair::load(Path::new(KEY_FILE)) {
        Ok(k) => k,
        Err(e) => {
            error!("loading BLE key file {KEY_FILE}: {e:#}");
            return ExitCode::from(2);
        }
    };
    let session = PersistentSession::start(keypair, vin, adapter);
    let status = session.check_pairing().await;
    session.shutdown().await;
    let token = match status {
        PairingStatus::Paired => "PAIRED",
        PairingStatus::NotPaired => "NOT_PAIRED",
        PairingStatus::Unreachable(reason) => {
            info!("session-info direct probe unreachable: {reason}");
            "UNREACHABLE"
        }
    };
    println!("{token}");
    ExitCode::SUCCESS
}

/// `pair` verb. Sends an unauthenticated add-key-to-whitelist request for
/// our own public key, so the car prompts for an NFC-card tap on the
/// center console. Fire-and-forget: success (exit 0, prints `OK`) means
/// the request was delivered — the caller then waits for the tap and
/// confirms enrolment with `session-info`. Replaces the old
/// `tesla-control add-key-request` shell-out.
///
/// IPC-first: routes through the telemetry daemon's already-open
/// connection, reusing the BLE slot the daemon holds. This is the key
/// fix for the "maximum number of BLE devices" failure — the old flow
/// stopped the daemon and opened a *new* connection before the car freed
/// the slot. Direct fallback (own one-shot session) covers a
/// disabled/crashed daemon.
///
/// Exit codes: 0 request delivered, 2 config/key error, 3 BLE/slot error.
async fn run_pair() -> ExitCode {
    match try_via_ipc("pair").await {
        Ok(()) => {
            println!("OK");
            info!("add-key-request delivered via daemon IPC — tap your card on the console");
            return ExitCode::SUCCESS;
        }
        Err(IpcError::Unavailable(reason)) => {
            info!(
                "telemetry IPC unavailable ({}), sending add-key-request via direct BLE",
                reason
            );
        }
        Err(IpcError::DaemonRejected(msg)) => {
            // Daemon is up but the add-key write failed (slot full, car
            // out of range/asleep). A direct attempt would open a *new*
            // connection — exactly what hits the car's slot limit — so
            // surface the daemon's error rather than thrashing the radio.
            eprintln!("{msg}");
            return ExitCode::from(3);
        }
    }

    // Direct fallback: open our own one-shot session and send the request.
    let (vin, adapter) = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::from(2);
        }
    };
    let keypair = match KeyPair::load(Path::new(KEY_FILE)) {
        Ok(k) => k,
        Err(e) => {
            error!("loading BLE key file {KEY_FILE}: {e:#}");
            return ExitCode::from(2);
        }
    };
    let session = PersistentSession::start(keypair, vin, adapter);
    let result = tokio::time::timeout(Duration::from_secs(60), session.add_key_request()).await;
    session.shutdown().await;
    match result {
        Ok(Ok(())) => {
            println!("OK");
            info!("add-key-request delivered via direct BLE — tap your card on the console");
            ExitCode::SUCCESS
        }
        Ok(Err(e)) => {
            error!("{e:#}");
            ExitCode::from(3)
        }
        Err(_) => {
            error!("pair (add-key) timed out after 60s");
            ExitCode::from(3)
        }
    }
}

/// Send `session-info` over the daemon IPC socket and map the reply to
/// a stdout token. `Unavailable` means no daemon is listening (caller
/// falls back to direct); `DaemonRejected` means an unrecognised reply.
async fn session_info_via_ipc() -> Result<&'static str, IpcError> {
    let stream = match tokio::time::timeout(
        Duration::from_millis(1000),
        UnixStream::connect(IPC_SOCKET),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {}: {}",
                IPC_SOCKET, e
            )));
        }
        Err(_) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {} timed out",
                IPC_SOCKET
            )));
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    if let Err(e) = write_half.write_all(b"session-info\n").await {
        return Err(IpcError::Unavailable(format!("writing verb: {}", e)));
    }

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    match tokio::time::timeout(Duration::from_secs(90), reader.read_line(&mut line)).await {
        Ok(Ok(0)) => Err(IpcError::Unavailable(
            "daemon closed connection without response".into(),
        )),
        Ok(Ok(_)) => {
            let line = line.trim();
            let rest = line.strip_prefix("ERR ").map(str::trim);
            if line == "OK" {
                Ok("PAIRED")
            } else if rest.is_some_and(|r| r.starts_with("NOT_PAIRED")) {
                Ok("NOT_PAIRED")
            } else if rest.is_some_and(|r| r.starts_with("UNREACHABLE")) {
                Ok("UNREACHABLE")
            } else {
                Err(IpcError::DaemonRejected(line.to_string()))
            }
        }
        Ok(Err(e)) => Err(IpcError::Unavailable(format!("read response: {}", e))),
        Err(_) => Err(IpcError::Unavailable(
            "daemon response timed out after 90s".into(),
        )),
    }
}

/// `drive-state` verb. Prints the gear token (`P`/`R`/`N`/`D`) to stdout
/// and exits 0 on success; exits non-zero on any failure (so the
/// lock-chime caller's `run_with_timeout` sees an Err). IPC-first so the
/// query reuses the telemetry daemon's warm connection; direct fallback
/// covers a disabled/crashed daemon.
async fn run_drive_state() -> ExitCode {
    match drive_state_via_ipc().await {
        Ok(token) => {
            println!("{token}");
            return ExitCode::SUCCESS;
        }
        Err(IpcError::Unavailable(reason)) => {
            info!(
                "telemetry IPC unavailable ({}), reading drive state via direct BLE",
                reason
            );
        }
        Err(IpcError::DaemonRejected(msg)) => {
            // Daemon is up but couldn't read the gear (car asleep /
            // unreachable / no concrete gear). A direct attempt would
            // just repeat against the same car, so surface the failure
            // instead of thrashing the radio.
            eprintln!("{msg}");
            return ExitCode::from(3);
        }
    }

    // Direct fallback: open our own one-shot session.
    let (vin, adapter) = match load_config() {
        Ok(c) => c,
        Err(e) => {
            error!("{e:#}");
            return ExitCode::from(2);
        }
    };
    let keypair = match KeyPair::load(Path::new(KEY_FILE)) {
        Ok(k) => k,
        Err(e) => {
            error!("loading BLE key file {KEY_FILE}: {e:#}");
            return ExitCode::from(2);
        }
    };
    let session = PersistentSession::start(keypair, vin, adapter);
    let result = tokio::time::timeout(Duration::from_secs(60), session.get_drive()).await;
    session.shutdown().await;
    match result {
        Ok(Ok(drive)) => match shift_state_token(&drive) {
            Some(tok) => {
                println!("{tok}");
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("car reported no gear (may be asleep)");
                ExitCode::from(3)
            }
        },
        Ok(Err(e)) => {
            error!("{e:#}");
            ExitCode::from(3)
        }
        Err(_) => {
            error!("drive-state timed out after 60s");
            ExitCode::from(3)
        }
    }
}

/// Send `drive-state` over the daemon IPC socket and map the reply to a
/// gear token. `Unavailable` means no daemon is listening (caller falls
/// back to direct); `DaemonRejected` carries the daemon's error line.
async fn drive_state_via_ipc() -> Result<String, IpcError> {
    let stream = match tokio::time::timeout(
        Duration::from_millis(1000),
        UnixStream::connect(IPC_SOCKET),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {}: {}",
                IPC_SOCKET, e
            )));
        }
        Err(_) => {
            return Err(IpcError::Unavailable(format!(
                "connect to {} timed out",
                IPC_SOCKET
            )));
        }
    };

    let (read_half, mut write_half) = stream.into_split();
    if let Err(e) = write_half.write_all(b"drive-state\n").await {
        return Err(IpcError::Unavailable(format!("writing verb: {}", e)));
    }

    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    match tokio::time::timeout(Duration::from_secs(90), reader.read_line(&mut line)).await {
        Ok(Ok(0)) => Err(IpcError::Unavailable(
            "daemon closed connection without response".into(),
        )),
        Ok(Ok(_)) => {
            let line = line.trim();
            if let Some(tok) = line.strip_prefix("OK ") {
                Ok(tok.trim().to_string())
            } else if let Some(rest) = line.strip_prefix("ERR ") {
                Err(IpcError::DaemonRejected(rest.trim().to_string()))
            } else {
                // Bare "OK" (no token) or anything unexpected — the
                // daemon should always include a gear token here.
                Err(IpcError::DaemonRejected(format!(
                    "unexpected drive-state reply: {:?}",
                    line
                )))
            }
        }
        Ok(Err(e)) => Err(IpcError::Unavailable(format!("read response: {}", e))),
        Err(_) => Err(IpcError::Unavailable(
            "daemon response timed out after 90s".into(),
        )),
    }
}

async fn run(verb: &str, action: ActionPayload) -> Result<()> {
    let (vin, adapter) = load_config()?;
    info!(
        "sentryusb-ble-action: verb={} domain={:?} inner={} bytes vin={}…{}",
        verb,
        action.domain,
        action.inner.len(),
        &vin[..3],
        &vin[vin.len() - 4..]
    );

    let keypair = KeyPair::load(Path::new(KEY_FILE))
        .with_context(|| format!("loading BLE key file {KEY_FILE}"))?;
    let session = PersistentSession::start(keypair, vin, adapter);

    // One-shot — wrap in an outer timeout so the script doesn't hang
    // indefinitely if the car never advertises.
    let resp = tokio::time::timeout(
        Duration::from_secs(60),
        session.send_action(action),
    )
    .await
    .context("BLE action timed out after 60s")?;
    session.shutdown().await;

    match resp {
        Ok(bytes) => {
            info!("action OK; decrypted response = {} bytes", bytes.len());
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Read TESLA_BLE_VIN and (optionally) BLE_ADAPTER from sentryusb.conf.
/// Returns (vin, Some(adapter)) or fails if VIN missing.
fn load_config() -> Result<(String, Option<String>)> {
    let raw = std::fs::read_to_string(CONFIG_FILE)
        .with_context(|| format!("reading {CONFIG_FILE}"))?;
    let mut vin: Option<String> = None;
    let mut adapter: Option<String> = None;
    for line in raw.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("export TESLA_BLE_VIN=") {
            vin = Some(unquote(rest).to_uppercase());
        } else if let Some(rest) = trimmed.strip_prefix("export BLE_ADAPTER=") {
            adapter = Some(unquote(rest));
        }
    }
    let vin = vin.context("TESLA_BLE_VIN not set in /root/sentryusb.conf")?;
    if vin.len() != 17 {
        anyhow::bail!("TESLA_BLE_VIN must be 17 chars, got {}", vin.len());
    }
    Ok((vin, adapter))
}

fn unquote(s: &str) -> String {
    let t = s.trim();
    // len >= 2 so a value that is a single quote character can't
    // underflow the slice below.
    if t.len() >= 2
        && ((t.starts_with('"') && t.ends_with('"'))
            || (t.starts_with('\'') && t.ends_with('\'')))
    {
        t[1..t.len() - 1].to_string()
    } else {
        t.to_string()
    }
}
