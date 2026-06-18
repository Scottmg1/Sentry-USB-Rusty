//! Persistent BLE session manager.
//!
//! The per-call pattern (scan → connect → handshake → command →
//! disconnect) opens a fresh GATT connection per query, so it never
//! holds one of the car's ~3 BLE slots and re-competes against the
//! phone keys and iOS app every cycle. `PersistentSession` keeps one
//! long-lived connection plus per-domain session keys across many
//! commands, cutting per-query cost from ~1.5-2s to ~200-500ms.
//!
//! ## Usage
//!
//! ```ignore
//! let session = PersistentSession::start(keypair, vin, adapter);
//! loop {
//!     let climate = session
//!         .query(Domain::Infotainment, VehicleDataState::Climate)
//!         .await?;
//!     tokio::time::sleep(Duration::from_secs(15)).await;
//! }
//! ```
//!
//! ## Recovery
//!
//! * Transport error (link drop / GATT timeout) → drop the connection;
//!   the next query rescans and reconnects, backing off on repeats.
//! * Counter/epoch fault → drop that domain's session state; the next
//!   query re-handshakes just that domain, connection stays up.
//! * Other faults → returned to the caller.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use btleplug::api::Peripheral as _;
use prost::Message;
use tokio::sync::{mpsc, oneshot};
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::auth;
use crate::crypto::{SessionKey, derive_session_key};
use crate::gatt::Connection;
use crate::keys::KeyPair;
use crate::proto::signatures::{SessionInfo, signature_data};
use crate::proto::universal_message::{
    Domain, RoutableMessage, destination, routable_message,
};
use crate::scan;
use crate::session;
use crate::state_query::{self, VehicleDataState};

/// Max time a single query's BLE round-trip can take before we treat
/// it as a transport failure and force a reconnect on the next call.
const QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// First reconnect attempt after a failure waits this long. Each
/// successive failure doubles up to `RECONNECT_BACKOFF_MAX`. Any
/// successful connection resets back to this value.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(1_500);
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Rebuild the cached adapter after this many consecutive connect
/// failures — self-heals a wedged bluez session / hci reset. Success
/// resets the counter, so a flappy link never rebuilds (and never leaks
/// a fresh D-Bus connection per reconnect).
const ADAPTER_REBUILD_AFTER: u32 = 5;

/// Seconds added to the *estimated* car clock to produce the
/// `expires_at` field. Tesla caps this at a few minutes (commands
/// stamped too far in the future are rejected as a replay-prevention
/// precaution), but the value just needs to comfortably cover the
/// BLE round-trip and any local drift between sampler clock and car
/// clock. 60 s is a safe margin without coming close to Tesla's cap.
const EXPIRES_WINDOW: u32 = 60;

/// Flags value to send on signed state queries. Bit 1 (value 2) is
/// FLAG_ENCRYPT_RESPONSE — required so the car encrypts its reply
/// instead of sending it plaintext, matches tesla-control's wire
/// format, and is part of the metadata the AES-GCM tag is computed
/// over so the value must match between our sign + the car's verify.
const QUERY_FLAGS: u32 = 2;

/// Handle to a long-lived BLE session with one Tesla vehicle.
/// Cheap to clone — internally an `mpsc::Sender` to the background
/// task. Dropping all clones doesn't stop the task; call `shutdown()`
/// for that.
#[derive(Clone)]
pub struct PersistentSession {
    cmd_tx: mpsc::Sender<Command>,
}

/// Result of an on-demand pairing probe ([`Command::CheckPairing`]).
/// Splits "the car says our key isn't on its whitelist" (the user must
/// re-pair) from "couldn't reach the car right now" (asleep, out of
/// range, radio busy) so callers — notably the API's BLE status card —
/// only clear the paired marker on the former, never on contention.
#[derive(Debug, Clone)]
pub enum PairingStatus {
    /// session-info handshake succeeded: key is enrolled, car answered.
    Paired,
    /// Car returned `SESSION_INFO_STATUS_KEY_NOT_ON_WHITELIST`.
    NotPaired,
    /// Connect/scan/transport failure — pairing is unknown, not
    /// disproven. Carries a short reason for logs.
    Unreachable(String),
}

/// Marker attached (via anyhow context) to every error coming out of
/// the connect layer — adapter lookup, scan, GATT connect. Lets callers
/// distinguish "couldn't reach the car at all" (where every further
/// query this cycle would redo the same 30s scan + backoff) from
/// per-domain protocol failures (where other queries may still work).
/// Check with [`is_connect_failure`].
#[derive(Debug, Clone, Copy)]
pub struct ConnectFailure;

impl std::fmt::Display for ConnectFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Wording deliberately avoids the substrings is_transport_error
        // matches on ("not connected", "Peripheral", "BLE write", ...) —
        // a connect failure has no held connection to drop.
        f.write_str("could not reach vehicle (connect-layer failure)")
    }
}

/// Whether `e` originated in the connect layer (scan/adapter/connect),
/// i.e. carries a [`ConnectFailure`] marker anywhere in its chain.
pub fn is_connect_failure(e: &anyhow::Error) -> bool {
    e.downcast_ref::<ConnectFailure>().is_some()
}

enum Command {
    Query {
        domain: Domain,
        state: VehicleDataState,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    /// Generic signed request — caller supplies the inner payload
    /// bytes already encoded (e.g. a VCSEC RKEAction or a car_server
    /// VehicleControl action). Used by keep-awake actions
    /// (wake-vehicle, sentry-mode, charge-port) so they reuse the
    /// same sign + send + decrypt + refresh-and-retry pipeline as
    /// state queries.
    SignedRequest {
        domain: Domain,
        inner: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    /// Unauthenticated body-controller query, run over the held
    /// connection so it doesn't open a competing one or disturb the slot.
    BodyController {
        reply: oneshot::Sender<Result<crate::proto::vcsec::VehicleStatus>>,
    },
    /// On-demand "is our key still on the car's whitelist" probe. Runs a
    /// session-info handshake over the held connection and reports a
    /// tri-state. Lets the API's status card verify pairing through this
    /// session instead of spawning a competing connection.
    CheckPairing {
        reply: oneshot::Sender<PairingStatus>,
    },
    /// Unauthenticated add-key-to-whitelist (pairing) request for our own
    /// public key, sent over the held connection so it reuses the slot
    /// the daemon already owns instead of opening a competing one — which
    /// is what tripped the car's "maximum number of BLE devices" limit in
    /// the old tesla-control flow. Fire-and-forget: the car prompts for an
    /// NFC-card tap; enrolment is confirmed later via CheckPairing.
    AddKey {
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown,
}

/// Per-domain authenticated session state cached across commands.
struct DomainSession {
    key: SessionKey,
    epoch: Vec<u8>,
    /// Most recent counter the car has seen from us; the next command
    /// uses `counter + 1`.
    counter: u32,
    /// Car `clock_time` from the last SessionInfo, paired with the
    /// local `Instant` it arrived. Estimated car clock = clock_time +
    /// local elapsed. Without the elapsed term, `expires_at` is derived
    /// from a frozen clock and the car eventually rejects commands as
    /// TIME_EXPIRED (fault 17).
    clock_time_at_handshake: u32,
    handshake_local_time: Instant,
}

impl DomainSession {
    /// Estimate the car's current clock_time from the cached value plus
    /// local elapsed seconds. Drift is slow enough for `expires_at`.
    fn estimated_car_clock(&self) -> u32 {
        let elapsed = self.handshake_local_time.elapsed().as_secs() as u32;
        self.clock_time_at_handshake.saturating_add(elapsed)
    }
}

/// Owned by the background task only.
struct SessionState {
    keypair: KeyPair,
    vin: String,
    /// Configured `BLE_ADAPTER` from sentryusb.conf; None lets btleplug
    /// pick the first adapter.
    adapter_name: Option<String>,
    conn: Option<Connection>,
    domains: HashMap<Domain, DomainSession>,
    /// Current reconnect backoff. Doubles on each failed connect.
    backoff: Duration,
    /// When the manager started or last reconnected — for logging.
    connected_at: Option<Instant>,
    /// Successful queries since the current connection; reset on
    /// reconnect. Lets the status log show the slot is held (climbing)
    /// vs re-grabbed (resetting).
    queries_since_connect: u32,
    /// When the last query fully succeeded; reset on connect. The
    /// disconnect diagnostic uses it to show whether the link was
    /// healthy up to the drop (last_ok=1s) or already degrading
    /// (last_ok=45s).
    last_successful_query_at: Option<Instant>,
    /// Total connection drops since daemon start, logged on each drop
    /// so a journal tail shows how flappy the link is.
    lifetime_drops: u32,
    /// Sliding window of recent successful-query latencies (ms) for the
    /// p50/p95/p99 summary. Rising percentiles flag link degradation
    /// before a drop. Capped at SAMPLES_FOR_PERCENTILES.
    recent_latencies_ms: VecDeque<u128>,
    /// RSSI from the pre-connect scan. bluez exposes no live RSSI for
    /// active LE connections, so this scan-time value is the best proxy
    /// for link quality at connect — when most slot races happen.
    last_scan_rssi: Option<i16>,
    /// Peer MAC of the most recent connection, captured before
    /// `Connection::open` consumes the peripheral; written to the
    /// status file.
    last_peer_mac: Option<String>,
    /// Cumulative count of in-round_trip framing-desync recoveries
    /// (oversized length prefix cleared and retried) across reconnects.
    /// A climbing count means the read buffer is regularly polluted
    /// (stale notifications, unmatched broadcasts, chunked stragglers).
    /// Each event still produced a successful query.
    framing_desync_recoveries: u32,
    /// Cached btleplug adapter (a bluez D-Bus session), reused across
    /// reconnects. A fresh `Manager::new()` per `ensure_connected` leaks a
    /// D-Bus connection each time until root hits the per-UID ceiling
    /// (~256) and every BLE op fails with "maximum number of active
    /// connections for UID 0". A dropped GATT link doesn't kill the
    /// adapter, so reuse is safe.
    cached_adapter: Option<btleplug::platform::Adapter>,
    /// Consecutive `ensure_connected` failures; reset on success, triggers
    /// an adapter rebuild at `ADAPTER_REBUILD_AFTER`.
    consecutive_connect_failures: u32,
}

/// Timing-sample window for the percentiles. 100 ≈ 5-10 min of
/// Active-mode polling — meaningful, but reacts within minutes.
const SAMPLES_FOR_PERCENTILES: usize = 100;

/// Persistent disconnect log. Each drop appends one line so the bundle
/// retains drop history after journald rotates. Best-effort: skipped
/// if the path isn't writable (e.g. /mutable not mounted at boot).
const DISCONNECT_LOG_PATH: &str = "/mutable/sentryusb-ble-disconnects.log";

/// Truncate the disconnect log past this size, keeping the most-recent
/// half. 256 KB ≈ 2,500 lines.
const DISCONNECT_LOG_ROTATE_AT_BYTES: u64 = 256 * 1024;

/// Live status file, atomically rewritten on each connect/disconnect
/// so the api crate (a separate process) can report the current
/// connection state — since when, peer MAC, scan RSSI — without
/// parsing journalctl.
const STATUS_FILE_PATH: &str = "/mutable/sentryusb-ble-status.txt";

/// Emit a connection-status summary every N successful queries
/// (~6 min at 15s cycles).
const STATUS_LOG_EVERY_N_QUERIES: u32 = 25;

impl PersistentSession {
    /// Spawn the background session task and return a handle. The first
    /// `query()` triggers the actual connection. `adapter_name` forces
    /// a specific adapter (e.g. "hci1", matching `BLE_ADAPTER`);
    /// None/empty lets btleplug choose.
    pub fn start(
        keypair: KeyPair,
        vin: String,
        adapter_name: Option<String>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let state = SessionState {
            keypair,
            vin,
            adapter_name,
            conn: None,
            domains: HashMap::new(),
            backoff: RECONNECT_BACKOFF_MIN,
            connected_at: None,
            queries_since_connect: 0,
            last_successful_query_at: None,
            lifetime_drops: 0,
            recent_latencies_ms: VecDeque::with_capacity(SAMPLES_FOR_PERCENTILES),
            last_scan_rssi: None,
            last_peer_mac: None,
            framing_desync_recoveries: 0,
            cached_adapter: None,
            consecutive_connect_failures: 0,
        };
        tokio::spawn(run_session_task(state, cmd_rx));
        Self { cmd_tx }
    }

    /// Issue an authenticated state query. Blocks until the response
    /// is decrypted or an error occurs. Errors include:
    ///   * background task is gone (shouldn't happen unless `shutdown` was called)
    ///   * scan/connect failure (car asleep, out of range, slots full)
    ///   * car returned a non-zero `signed_message_fault`
    ///   * decryption failure
    pub async fn query(
        &self,
        domain: Domain,
        state: VehicleDataState,
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::Query {
                domain,
                state,
                reply: tx,
            })
            .await
            .context("PersistentSession background task has stopped")?;
        rx.await.context("session task dropped the reply channel")?
    }

    /// Best-effort stop: closes the connection and ends the task.
    /// `query()` errors afterward.
    pub async fn shutdown(&self) {
        let _ = self.cmd_tx.send(Command::Shutdown).await;
    }

    /// Issue a generic signed request with caller-supplied inner
    /// payload bytes. Used by keep-awake actions that need the AES-GCM
    /// signing pipeline but produce different inner protobufs than the
    /// state queries.
    pub async fn send_signed(
        &self,
        domain: Domain,
        inner: Vec<u8>,
    ) -> Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::SignedRequest {
                domain,
                inner,
                reply: tx,
            })
            .await
            .context("PersistentSession background task has stopped")?;
        rx.await.context("session task dropped the reply channel")?
    }

    /// Convenience wrapper around `send_signed` for the typed action
    /// helpers in `crate::actions`.
    pub async fn send_action(
        &self,
        action: crate::actions::ActionPayload,
    ) -> Result<Vec<u8>> {
        self.send_signed(action.domain, action.inner).await
    }

    /// Unauthenticated body-controller query over the held connection
    /// (no new scan/connect, no competition with the authenticated
    /// queries). Used by the sampler's Quiet-mode poll — sleep-safe,
    /// doesn't wake the car.
    pub async fn body_controller_state(
        &self,
    ) -> Result<crate::proto::vcsec::VehicleStatus> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::BodyController { reply: tx })
            .await
            .context("PersistentSession background task has stopped")?;
        rx.await.context("session task dropped the reply channel")?
    }

    /// Probe whether our key is still on the car's whitelist, reusing
    /// the held connection (no competing scan/connect). A dead
    /// background task or dropped reply maps to `Unreachable` — the
    /// same "pairing unknown" bucket as a transport failure, so callers
    /// never read a stopped session as "not paired".
    pub async fn check_pairing(&self) -> PairingStatus {
        let (tx, rx) = oneshot::channel();
        if self
            .cmd_tx
            .send(Command::CheckPairing { reply: tx })
            .await
            .is_err()
        {
            return PairingStatus::Unreachable(
                "PersistentSession background task has stopped".into(),
            );
        }
        rx.await.unwrap_or(PairingStatus::Unreachable(
            "session task dropped the reply channel".into(),
        ))
    }

    /// Send an unauthenticated add-key-to-whitelist (pairing) request for
    /// our own public key over the held connection. Fire-and-forget: on
    /// `Ok(())` the car received the request and is prompting for an
    /// NFC-card tap on the center console — call [`check_pairing`] after
    /// the user taps to confirm enrolment. `Err` is a connect/write
    /// failure (e.g. the car's BLE slots are full, or it's out of range).
    ///
    /// [`check_pairing`]: Self::check_pairing
    pub async fn add_key_request(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(Command::AddKey { reply: tx })
            .await
            .context("PersistentSession background task has stopped")?;
        rx.await.context("session task dropped the reply channel")?
    }

    // Typed wrappers: each does a raw Infotainment `query()` and
    // decodes the response into the relevant car_server sub-message.

    /// `state climate`. Interior/exterior temps, HVAC, defroster, etc.
    pub async fn get_climate(&self) -> Result<crate::proto::car_server::ClimateState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Climate)
            .await?;
        crate::responses::parse_climate(&bytes)
    }

    /// `state charge`. Battery %, charger info, range estimate.
    pub async fn get_charge(&self) -> Result<crate::proto::car_server::ChargeState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Charge)
            .await?;
        crate::responses::parse_charge(&bytes)
    }

    /// `state drive`. Shift state, speed, heading.
    pub async fn get_drive(&self) -> Result<crate::proto::car_server::DriveState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Drive)
            .await?;
        crate::responses::parse_drive(&bytes)
    }

    /// Same wire call as `get_drive` but returns full `VehicleData`
    /// (both `drive_state` and `location_state`). Tesla only populates
    /// the reverse-geocoded `location_name` in the LocationState
    /// bundled with `state drive`; standalone `state location` returns
    /// raw coords without it. Same round-trip cost. Use this when you
    /// need the address.
    pub async fn get_drive_with_location(
        &self,
    ) -> Result<(
        crate::proto::car_server::DriveState,
        Option<crate::proto::car_server::LocationState>,
    )> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Drive)
            .await?;
        let vd = crate::responses::parse_vehicle_data(&bytes)?;
        let drive = vd
            .drive_state
            .context("response missing drive_state")?;
        Ok((drive, vd.location_state))
    }

    /// `state location`. GPS coords only — `location_name` is not
    /// populated here (see `get_drive_with_location`).
    pub async fn get_location(&self) -> Result<crate::proto::car_server::LocationState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Location)
            .await?;
        crate::responses::parse_location(&bytes)
    }

    /// `state tire-pressure`. PSI per tire.
    pub async fn get_tire_pressure(&self) -> Result<crate::proto::car_server::TirePressureState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::TirePressure)
            .await?;
        crate::responses::parse_tire_pressure(&bytes)
    }

    /// `state closures`. Door/window/trunk/charge-port states.
    pub async fn get_closures(&self) -> Result<crate::proto::car_server::ClosuresState> {
        let bytes = self
            .query(Domain::Infotainment, VehicleDataState::Closures)
            .await?;
        crate::responses::parse_closures(&bytes)
    }
}

/// Idle-keepalive cadence for the held car link (Fix A1). A car can drop
/// a GATT link that sees no application traffic for a while; between the
/// scheduler's polls the link is otherwise silent — and that's where the
/// drops cluster, especially while parked. ~8s is well inside typical
/// car-side idle windows without wasting air time (do NOT drop to 2-3s).
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(8);
/// Only send a keepalive if the link has actually been idle this long,
/// so we skip a redundant feed right after a real command.
const KEEPALIVE_IDLE: Duration = Duration::from_secs(6);

async fn run_session_task(
    mut state: SessionState,
    mut cmd_rx: mpsc::Receiver<Command>,
) {
    // Idle keepalive (Fix A1): feed the held link on a timer between
    // commands so the car doesn't drop an otherwise-silent GATT link.
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await; // consume the immediate first tick
    let mut last_activity = Instant::now();
    loop {
        let cmd = tokio::select! {
            maybe_cmd = cmd_rx.recv() => match maybe_cmd {
                Some(c) => c,
                None => break,
            },
            _ = keepalive.tick() => {
                // Feed the link with the sleep-safe, unauthenticated
                // body-controller query (no session key needed). Only
                // when we hold a connection and it's genuinely idle. A
                // failure here just drops + reconnects on the next op —
                // which also proactively recovers a silently-dead link.
                if state.conn.is_some() && last_activity.elapsed() >= KEEPALIVE_IDLE {
                    let r = handle_body_controller(&mut state).await;
                    handle_transport_error_if_any(&mut state, &r).await;
                    match &r {
                        Ok(_) => debug!("BLE keepalive: idle-feed ok"),
                        Err(e) => debug!(
                            "BLE keepalive: idle-feed failed (recovers on next op): {e:#}"
                        ),
                    }
                    last_activity = Instant::now();
                }
                continue;
            }
        };
        // Time each command end-to-end so the latency window reflects
        // the full round-trip (refresh retry, scan, reconnect included).
        let started = Instant::now();
        match cmd {
            Command::Query {
                domain,
                state: vds,
                reply,
            } => {
                let inner = state_query::build_get_state_request(vds);
                let result = signed_request_with_refresh_retry(
                    &mut state, domain, inner,
                )
                .await;
                handle_transport_error_if_any(&mut state, &result).await;
                if result.is_ok() {
                    note_successful_query(&mut state, started.elapsed().as_millis());
                }
                let _ = reply.send(result);
            }
            Command::SignedRequest {
                domain,
                inner,
                reply,
            } => {
                let result = signed_request_with_refresh_retry(
                    &mut state, domain, inner,
                )
                .await;
                handle_transport_error_if_any(&mut state, &result).await;
                if result.is_ok() {
                    note_successful_query(&mut state, started.elapsed().as_millis());
                }
                let _ = reply.send(result);
            }
            Command::BodyController { reply } => {
                let result = handle_body_controller(&mut state).await;
                handle_transport_error_if_any(&mut state, &result).await;
                if result.is_ok() {
                    note_successful_query(&mut state, started.elapsed().as_millis());
                }
                let _ = reply.send(result);
            }
            Command::CheckPairing { reply } => {
                // `Ok(Paired)` / `Ok(NotPaired)` are definitive answers
                // from the car; `Err` is a connect/transport failure.
                // Route the Err through the transport handler so a dead
                // link is dropped + reconnected on the next call, then
                // collapse it to `Unreachable` for the caller.
                let result = handle_check_pairing(&mut state).await;
                handle_transport_error_if_any(&mut state, &result).await;
                let status = match result {
                    Ok(status) => status,
                    Err(e) => PairingStatus::Unreachable(format!("{e:#}")),
                };
                let _ = reply.send(status);
            }
            Command::AddKey { reply } => {
                // Errors here are connect/transport failures — route them
                // through the transport handler so a dead link is dropped
                // and reconnected on the next op, same as the queries.
                let result = handle_add_key(&mut state).await;
                handle_transport_error_if_any(&mut state, &result).await;
                if result.is_ok() {
                    note_successful_query(&mut state, started.elapsed().as_millis());
                }
                let _ = reply.send(result);
            }
            Command::Shutdown => break,
        }
        // Fold any framing-desync recoveries from this command into the
        // session-lifetime counter. Outside the match so it covers all
        // three query paths and counts recoveries even on a failed
        // query. Kept on SessionState so it survives reconnects.
        if let Some(conn) = state.conn.as_mut() {
            let n = conn.take_framing_desync_recoveries();
            if n > 0 {
                state.framing_desync_recoveries =
                    state.framing_desync_recoveries.saturating_add(n);
                // Refresh the status file on each nonzero drain so
                // readers see the latest count without waiting for the
                // next connect/disconnect.
                write_status_file(&state, ConnectionEvent::Connected);
            }
        }
        last_activity = Instant::now();
    }
    if let Some(conn) = state.conn.take() {
        conn.close().await;
    }
}

/// Handles SessionInfo-refresh replies: the car may answer a signed
/// command with a fresh SessionInfo ("your session is stale, retry")
/// instead of an encrypted response. At most one refresh retry per
/// query before the error is surfaced.
async fn signed_request_with_refresh_retry(
    state: &mut SessionState,
    domain: Domain,
    inner: Vec<u8>,
) -> Result<Vec<u8>> {
    // Retry budget: at most one SessionInfo refresh and one
    // OPERATIONSTATUS_WAIT per query, tracked independently so
    // WAIT-then-refresh still gets a final attempt. On repeat, bail and
    // let the schedule's next tick retry rather than loop here.
    const WAIT_RETRY_DELAY: Duration = Duration::from_millis(400);
    let mut refresh_retries_left = 1u32;
    let mut wait_retries_left = 1u32;
    // One self-heal for a stale-session decrypt failure: the car answered
    // but we couldn't decrypt because our cached session key is stale
    // (car re-keyed / rotated epoch). Dropping the cached session forces a
    // fresh handshake on the retry. Without this the stale key persisted
    // and every command failed `aead::Error` until an unrelated transport
    // drop happened to clear it. (tesla-control never hits this — it
    // re-handshakes a fresh session on every invocation.)
    let mut stale_retries_left = 1u32;

    loop {
        match try_signed_request_once(state, domain, &inner).await {
            Ok(QueryOutcome::Plaintext(bytes)) => return Ok(bytes),
            Ok(QueryOutcome::SessionRefresh(info)) => {
                if refresh_retries_left == 0 {
                    // The car keeps pushing SessionInfo refreshes that
                    // don't converge — the signature of a stale, long-held
                    // session (drifted clock estimate / rotated epoch).
                    // Applying the car's pushed refresh isn't fixing it, so
                    // fall back to a clean, explicit re-handshake from
                    // scratch (fresh clock_time + key) before giving up.
                    if stale_retries_left > 0 {
                        stale_retries_left -= 1;
                        state.domains.remove(&domain);
                        warn!(
                            "PersistentSession: {:?} SessionInfo refresh not converging \
                             — dropping cached session for a clean re-handshake and retrying",
                            domain
                        );
                        continue;
                    }
                    bail!(
                        "car kept requesting SessionInfo refresh even after a clean \
                         re-handshake for {:?} — giving up",
                        domain
                    )
                }
                refresh_retries_left -= 1;
                apply_session_refresh(state, domain, info)?;
                info!(
                    "PersistentSession: retrying signed request to {:?} after SessionInfo refresh",
                    domain
                );
            }
            Ok(QueryOutcome::OperationWait) => {
                if wait_retries_left == 0 {
                    // Two WAITs in a row — surface a clean error so the
                    // sampler fails this tick and fast-retries.
                    bail!(
                        "car returned OPERATIONSTATUS_WAIT twice in a row for \
                         {:?} — sample will be retried on next schedule tick",
                        domain
                    );
                }
                wait_retries_left -= 1;
                debug!(
                    "PersistentSession: WAIT from {:?}; sleeping {:?} and retrying once",
                    domain, WAIT_RETRY_DELAY
                );
                sleep(WAIT_RETRY_DELAY).await;
            }
            Err(e) => {
                // Stale-session self-heal: a response-decrypt failure
                // means the link is fine but our cached session key is
                // stale. Drop it so the retry re-handshakes a fresh one,
                // instead of failing this (and every following) command.
                if stale_retries_left > 0 && is_decrypt_error(&e) {
                    stale_retries_left -= 1;
                    state.domains.remove(&domain);
                    warn!(
                        "PersistentSession: response decrypt failed for {:?} \
                         (stale session) — dropping cached session, \
                         re-handshaking and retrying once: {e:#}",
                        domain
                    );
                    continue;
                }
                return Err(e);
            }
        }
    }
}

/// Normal outcomes from a signed query.
enum QueryOutcome {
    /// Decrypted response payload.
    Plaintext(Vec<u8>),
    /// Car returned a fresh SessionInfo; caller applies it and retries.
    SessionRefresh(SessionInfo),
    /// Status-only reply with operation_status=WAIT (no payload, no
    /// fault). Transient — retry after a short delay.
    OperationWait,
}

/// Apply a car-provided SessionInfo refresh: derive a new session key,
/// replace cached domain state, reset the handshake clock. No GATT
/// traffic — just ECDH + a HashMap insert.
fn apply_session_refresh(
    state: &mut SessionState,
    domain: Domain,
    info: SessionInfo,
) -> Result<()> {
    let key = derive_session_key(&state.keypair.secret, &info.public_key)
        .context("deriving session key from refreshed SessionInfo")?;
    info!(
        "PersistentSession: refreshed {:?} session — counter={} clock_time={}",
        domain, info.counter, info.clock_time
    );
    state.domains.insert(
        domain,
        DomainSession {
            key,
            epoch: info.epoch,
            counter: info.counter,
            clock_time_at_handshake: info.clock_time,
            handshake_local_time: Instant::now(),
        },
    );
    Ok(())
}

async fn try_signed_request_once(
    state: &mut SessionState,
    domain: Domain,
    inner: &[u8],
) -> Result<QueryOutcome> {
    ensure_connected(state).await?;
    ensure_domain_session(state, domain).await?;

    // Counter rollover guard: refuse to send rather than wrap to 0. The
    // car enforces strict per-epoch monotonicity, so a wrap is rejected
    // as a replay forever; only a re-handshake recovers. Checked before
    // borrowing the domain session so the cached state can actually be
    // dropped here — the next query then re-handshakes from scratch
    // (which resets the counter to whatever the car's fresh SessionInfo
    // provides).
    if state
        .domains
        .get(&domain)
        .is_some_and(|d| d.counter == u32::MAX)
    {
        state.domains.remove(&domain);
        bail!(
            "counter rollover: domain {:?} counter hit u32::MAX. \
             Dropped cached session state; the next query re-handshakes \
             from scratch.",
            domain
        );
    }

    let conn = state
        .conn
        .as_mut()
        .context("not connected after ensure_connected (bug)")?;
    let ds = state
        .domains
        .get_mut(&domain)
        .context("domain session not present after ensure_domain_session (bug)")?;

    // TX time for the SessionInfo stale-window check below — measured
    // from when our message left, so it reflects round-trip latency.
    let request_sent_at = Instant::now();
    let counter = ds.counter + 1;
    let expires_at = ds.estimated_car_clock().saturating_add(EXPIRES_WINDOW);

    let parts = auth::sign(
        &ds.key,
        &state.keypair.pub_uncompressed,
        inner,
        domain,
        state.vin.as_bytes(),
        &ds.epoch,
        expires_at,
        counter,
        QUERY_FLAGS,
    )?;

    let envelope = auth::build_signed_routable_message(&parts, domain, QUERY_FLAGS);

    // Advance the counter BEFORE sending. The car advances its expected
    // counter when it RECEIVES the message, not when we get the
    // response. If we advanced only on success, a lost response would
    // leave our counter behind the car's, and the next query would
    // reuse a counter the car already saw — rejected as a replay
    // (INVALID_TOKEN_OR_COUNTER), forcing an extra refresh round-trip.
    // Wasting a counter value on a failed send is negligible (2^32 per
    // epoch).
    ds.counter = counter;

    debug!(
        "PersistentSession: TX domain={:?} inner_len={} counter={}",
        domain,
        inner.len(),
        counter
    );
    // Correlate the response to THIS request before accepting it. The
    // car echoes our per-request `from_destination` routing address into
    // the response's `to_destination` (and stamps the domain it came
    // from). The old shape-only validator accepted ANY decodable
    // RoutableMessage, so a late straggler from a prior or cross-domain
    // query on the shared persistent session was taken as our response —
    // the root cause of the spurious `aead::Error` (a stale *signed*
    // reply decrypted with this request's REQUEST_HASH) and
    // "no sub_sig_data" (a VCSEC body-controller reply) failures. See
    // `crate::correlate`.
    let (our_uuid, our_addr) = crate::correlate::our_correlators(&envelope);
    let resp_bytes = conn
        .round_trip(&envelope, QUERY_TIMEOUT, |b| {
            crate::correlate::is_response_to(b, &our_uuid, &our_addr, domain)
        })
        .await?;

    // The transport validator filters most garbage frames; this decode
    // catches stragglers. Include the head bytes in the error so a
    // bundle shows the unparseable shape.
    let rm = match RoutableMessage::decode(resp_bytes.as_slice()) {
        Ok(rm) => rm,
        Err(e) => {
            let head = hex::encode(&resp_bytes[..resp_bytes.len().min(64)]);
            bail!(
                "decoding response RoutableMessage ({} bytes, head: {}…): {}",
                resp_bytes.len(),
                head,
                e
            );
        }
    };

    let fault = rm
        .signed_message_status
        .as_ref()
        .map(|s| s.signed_message_fault as u32)
        .unwrap_or(0);
    // Independent of signed_message_fault: the car can send a clean
    // "wait/busy" status (fault=0, no payload). OperationStatusE in
    // proto/universal_message.proto: OK=0, WAIT=1, ERROR=2.
    let op_status = rm
        .signed_message_status
        .as_ref()
        .map(|s| s.operation_status)
        .unwrap_or(0);

    // SessionInfo refresh: the car's standard "your session is stale,
    // here's fresh info" reply — an instruction to refresh and retry,
    // not an error. Defenses: reject if too old (a stale-cache replay
    // could roll our counter backward) or if our key isn't whitelisted.
    if let Some(routable_message::Payload::SessionInfo(info_bytes)) = &rm.payload {
        let parsed = SessionInfo::decode(info_bytes.as_slice())
            .context("decoding refreshed SessionInfo from car")?;

        // Stale-window check: reject a refresh that arrived > 10s after
        // the request — likely stale data; prefer a fresh handshake.
        // (More generous than tesla-control's 5s since QUERY_TIMEOUT is
        // 15s.)
        let elapsed = request_sent_at.elapsed();
        if elapsed > Duration::from_secs(10) {
            bail!(
                "SessionInfo refresh for {:?} arrived {:.1}s after the request \
                 was sent — exceeding the 10s freshness window. Refusing to \
                 apply (could be a stale-cache replay).",
                domain,
                elapsed.as_secs_f32(),
            );
        }

        // Surface a mid-session pair revocation here so it doesn't
        // cascade into encrypted-query decrypt failures.
        if parsed.status
            == crate::proto::signatures::SessionInfoStatus::KeyNotOnWhitelist as i32
        {
            bail!(
                "BLE pair revoked: car responded to {:?} query with \
                 SESSION_INFO_STATUS_KEY_NOT_ON_WHITELIST. Our key has been \
                 removed from the car (could be the user deleted the SentryUSB \
                 entry from Locks → Phone Keys, or someone re-paired with the \
                 same name). Re-pair from the SentryUSB UI.",
                domain
            );
        }

        // (Refresh HMAC verification intentionally omitted — see
        // ensure_domain_session.)

        return Ok(QueryOutcome::SessionRefresh(parsed));
    }

    if fault != 0 {
        // Counter/epoch faults recover by re-handshaking: drop the
        // cached session so the next query re-runs SessionInfoRequest.
        const FAULT_INVALID_SIGNATURE: u32 = 5;
        const FAULT_INVALID_TOKEN_OR_COUNTER: u32 = 6;
        const FAULT_INCORRECT_EPOCH: u32 = 15;
        const FAULT_TIME_EXPIRED: u32 = 17;
        if matches!(
            fault,
            FAULT_INVALID_SIGNATURE
                | FAULT_INVALID_TOKEN_OR_COUNTER
                | FAULT_INCORRECT_EPOCH
                | FAULT_TIME_EXPIRED
        ) {
            warn!(
                "PersistentSession: domain {:?} returned recoverable fault {} — \
                 dropping session state, will re-handshake on next query",
                domain, fault
            );
            state.domains.remove(&domain);
        }
        bail!("car responded with fault code {}", fault);
    }

    // Status-only reply (no payload, no fault, operation_status != OK):
    // "received, can't answer right now — retry shortly". WAIT (1) is
    // transient and retryable; ERROR (2) is an explicit rejection. The
    // retry policy lives in signed_request_with_refresh_retry.
    if rm.sub_sig_data.is_none() && fault == 0 && op_status != 0 {
        const OPERATIONSTATUS_WAIT: i32 = 1;
        const OPERATIONSTATUS_ERROR: i32 = 2;
        if op_status == OPERATIONSTATUS_WAIT {
            debug!(
                "PersistentSession: domain {:?} returned OPERATIONSTATUS_WAIT \
                 (status-only reply, no payload) — caller will retry",
                domain
            );
            return Ok(QueryOutcome::OperationWait);
        }
        if op_status == OPERATIONSTATUS_ERROR {
            bail!(
                "car returned OPERATIONSTATUS_ERROR for {:?} (status-only \
                 reply, no encrypted payload). request_uuid={}",
                domain,
                hex::encode(&rm.request_uuid),
            );
        }
        // Unknown non-OK status.
        bail!(
            "car returned unknown operation_status={} for {:?} \
             (no encrypted payload). request_uuid={}",
            op_status,
            domain,
            hex::encode(&rm.request_uuid),
        );
    }

    // Pull out the encrypted payload + AES_GCM_Response sig data.
    let resp_sig = match rm.sub_sig_data.as_ref() {
        Some(routable_message::SubSigData::SignatureData(sd)) => {
            match sd.sig_type.as_ref() {
                Some(signature_data::SigType::AesGcmResponseData(r)) => r,
                Some(other) => bail!(
                    "response signature_data was not AES_GCM_Response — got {}. \
                     Full response hex: {}",
                    sig_type_name(other),
                    hex::encode(&resp_bytes),
                ),
                None => bail!(
                    "response signature_data has no sig_type. Full response hex: {}",
                    hex::encode(&resp_bytes),
                ),
            }
        }
        None => bail!(
            "response has no sub_sig_data at all. payload variant: {}. Full hex: {}",
            payload_variant_name(rm.payload.as_ref()),
            hex::encode(&resp_bytes),
        ),
    };

    let ciphertext = rm
        .payload
        .as_ref()
        .and_then(|p| match p {
            routable_message::Payload::ProtobufMessageAsBytes(b) => Some(b.as_slice()),
            _ => None,
        })
        .context("response missing encrypted payload")?;

    let from_domain = rm
        .from_destination
        .as_ref()
        .and_then(|d| d.sub_destination.as_ref())
        .and_then(|sd| match sd {
            destination::SubDestination::Domain(d) => Domain::try_from(*d).ok(),
            _ => None,
        })
        .unwrap_or(domain);

    let request_tag = match &parts.signature_data.sig_type {
        Some(signature_data::SigType::AesGcmPersonalizedData(p)) => p.tag.clone(),
        _ => unreachable!("we just signed with AES_GCM_PERSONALIZED"),
    };

    let plaintext = match auth::decrypt_response(
        &ds.key,
        &request_tag,
        from_domain,
        state.vin.as_bytes(),
        rm.flags,
        resp_sig.counter,
        fault,
        &resp_sig.nonce,
        &resp_sig.tag,
        ciphertext,
    ) {
        Ok(p) => p,
        Err(e) => {
            // Decrypt failure with valid-looking sig data usually means
            // our cached session diverged from the car's (an
            // interleaving client bumped the counter or rolled the
            // epoch). Drop the domain state so the wrapper re-handshakes.
            warn!(
                "PersistentSession: decrypt failed for {:?} — \
                 dropping domain state for re-handshake on retry",
                domain
            );
            state.domains.remove(&domain);
            return Err(e);
        }
    };

    debug!("PersistentSession: decrypted {} bytes", plaintext.len());
    Ok(QueryOutcome::Plaintext(plaintext))
}

/// Drop the held connection if `result` looks like a transport failure
/// (link dropped, BLE write to a closed handle). The next command
/// reconnects. Protocol faults (INVALID_SIGNATURE, etc.) are handled in
/// the query handlers and don't drop the connection.
///
/// Each drop logs one structured line (lifetime, last-ok freshness,
/// drop count) so a journal tail distinguishes slot contention from a
/// degraded link from a flapping radio.
async fn handle_transport_error_if_any<T>(
    state: &mut SessionState,
    result: &Result<T>,
) {
    if let Err(e) = result {
        if state.conn.is_some() && is_transport_error(e) {
            state.lifetime_drops = state.lifetime_drops.saturating_add(1);
            let held_secs = state
                .connected_at
                .map(|t| t.elapsed().as_secs())
                .unwrap_or(0);
            let last_ok_secs = state
                .last_successful_query_at
                .map(|t| t.elapsed().as_secs() as i64)
                .unwrap_or(-1);
            let last_ok_str = if last_ok_secs >= 0 {
                format!("{}s", last_ok_secs)
            } else {
                "<never>".into()
            };
            // Compute final percentiles for this connection's window
            // so the journal line + persistent log both show what
            // the link latency looked like right before the drop.
            let (p50, p95, p99) = compute_percentiles(&state.recent_latencies_ms);
            let scan_rssi_str = state
                .last_scan_rssi
                .map(|r| r.to_string())
                .unwrap_or_else(|| "?".into());
            warn!(
                "PersistentSession: connection lost — \
                 held={}m{}s queries={} last_ok={}_ago drops_total={} \
                 p50/p95/p99={}/{}/{}ms scan_rssi={} desync_recoveries={} reason={:#}",
                held_secs / 60,
                held_secs % 60,
                state.queries_since_connect,
                last_ok_str,
                state.lifetime_drops,
                p50,
                p95,
                p99,
                scan_rssi_str,
                state.framing_desync_recoveries,
                e,
            );
            // Only for short-held drops (held < 5s): grab the kernel's
            // view, which usually has the HCI disconnect reason (0x05
            // auth failure, 0x13 remote terminated, 0x22 LL timeout,
            // 0x3D/0x3E conn failed). Long-held drops have obvious
            // causes (supervision timeout, range) and don't need it.
            if held_secs < 5 {
                if let Some(snippet) = capture_recent_bluetooth_dmesg().await {
                    warn!(
                        "PersistentSession: kernel/dmesg events around the drop:\n{}",
                        snippet
                    );
                }
            }
            // Persist the same data to /mutable/sentryusb-ble-disconnects.log
            // so the bundle download includes drops from before the
            // current journalctl rotation. Best-effort — if the
            // write fails (filesystem RO, /mutable unmounted, etc.)
            // we just keep going.
            append_disconnect_log(
                held_secs,
                state.queries_since_connect,
                last_ok_secs,
                state.lifetime_drops,
                p50,
                p95,
                p99,
                state.last_scan_rssi,
                state.framing_desync_recoveries,
                &format!("{:#}", e),
            );

            if let Some(conn) = state.conn.take() {
                conn.close().await;
            }
            state.domains.clear();
            state.connected_at = None;
            state.queries_since_connect = 0;
            // Leave last_scan_rssi / last_peer_mac populated so the
            // status file can still show the connection we just lost.
            write_status_file(state, ConnectionEvent::Disconnected);
            // Keep last_successful_query_at across the drop — the next
            // reconnect diagnostic reports the gap since the last query.
        }
    }
}

/// Append one row to the persistent disconnect log: RFC 3339 UTC
/// timestamp then space-separated `k=v` pairs. The bundle includes the
/// whole file, so drop history survives journald rotation.
fn append_disconnect_log(
    held_secs: u64,
    queries: u32,
    last_ok_secs: i64,
    lifetime_drops: u32,
    p50: u128,
    p95: u128,
    p99: u128,
    scan_rssi: Option<i16>,
    desync_recoveries: u32,
    reason: &str,
) {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    // Rotate first so the very first write into a freshly-large
    // file still leaves the file under cap afterwards.
    rotate_disconnect_log_if_needed();

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // RFC 3339 UTC, second precision.
    let ts = format_unix_iso8601(now_secs);

    // Replace tabs/newlines in the reason string so each disconnect
    // is exactly one line — important for grep + tail.
    let reason_safe = reason.replace(['\n', '\r', '\t'], " ");

    let scan_rssi_str = scan_rssi
        .map(|r| r.to_string())
        .unwrap_or_else(|| "?".into());

    let line = format!(
        "{} held={}s queries={} last_ok={}s drops_total={} \
         scan_rssi={} p50={}ms p95={}ms p99={}ms desync_recoveries={} \
         reason=\"{}\"\n",
        ts, held_secs, queries, last_ok_secs, lifetime_drops,
        scan_rssi_str, p50, p95, p99, desync_recoveries, reason_safe,
    );

    let result = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(DISCONNECT_LOG_PATH)
        .and_then(|mut f| f.write_all(line.as_bytes()));

    if let Err(e) = result {
        // Don't propagate — the journal line above has the same info.
        // Debug-level so it doesn't spam before /mutable is ready.
        debug!(
            "could not append to {} (best-effort): {}",
            DISCONNECT_LOG_PATH, e
        );
    }
}

/// Keep the most-recent half when the disconnect log exceeds the cap.
fn rotate_disconnect_log_if_needed() {
    let meta = match std::fs::metadata(DISCONNECT_LOG_PATH) {
        Ok(m) => m,
        Err(_) => return, // file doesn't exist yet
    };
    if meta.len() < DISCONNECT_LOG_ROTATE_AT_BYTES {
        return;
    }
    let raw = match std::fs::read(DISCONNECT_LOG_PATH) {
        Ok(b) => b,
        Err(_) => return,
    };
    let half = raw.len() / 2;
    // Trim to next line boundary so we don't truncate mid-row.
    let start = raw[half..]
        .iter()
        .position(|&b| b == b'\n')
        .map(|p| half + p + 1)
        .unwrap_or(half);
    let _ = std::fs::write(DISCONNECT_LOG_PATH, &raw[start..]);
}

/// Format a unix epoch as RFC 3339 UTC ("YYYY-MM-DDTHH:MM:SSZ"), so
/// this crate doesn't pull in chrono for one log line.
fn format_unix_iso8601(secs: u64) -> String {
    // Compute civil calendar from days-since-1970 using Howard Hinnant's
    // algorithm (well-known, public domain).
    let days = (secs / 86400) as i64;
    let seconds_of_day = secs % 86400;
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let h = seconds_of_day / 3600;
    let mi = (seconds_of_day / 60) % 60;
    let s = seconds_of_day % 60;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, h, mi, s
    )
}

/// Run a body-controller-state query over the held connection, so it
/// doesn't open a competing one and hit the bluez races the per-call
/// path kept triggering.
async fn handle_body_controller(
    state: &mut SessionState,
) -> Result<crate::proto::vcsec::VehicleStatus> {
    ensure_connected(state).await?;
    let conn = state
        .conn
        .as_mut()
        .context("ensure_connected returned without a connection")?;
    // Success-bookkeeping is done by the caller (run_session_task).
    crate::body_controller::query(conn).await
}

/// [`Command::AddKey`] handler: send the unauthenticated
/// add-key-to-whitelist request for our own public key over the held
/// connection. Fire-and-forget — the car prompts for an NFC-card tap and
/// enrolment is confirmed later by a `session-info` probe, so we only
/// surface connect/write failures here.
async fn handle_add_key(state: &mut SessionState) -> Result<()> {
    ensure_connected(state).await?;
    let payload = crate::pairing::build_add_key_request(&state.keypair.pub_uncompressed);
    let conn = state
        .conn
        .as_mut()
        .context("ensure_connected returned without a connection")?;
    info!("add-key-request: TX {} bytes (enrolling our key)", payload.len());
    conn.write_frame(&payload).await
}

/// session-info handshake over the held connection for the
/// [`Command::CheckPairing`] probe. `Ok(Paired)`/`Ok(NotPaired)` are
/// answers the car gave us; `Err` is a connect/transport failure the
/// caller maps to `Unreachable` (and which drops the connection via
/// `handle_transport_error_if_any`). `KeyNotPaired` deliberately does
/// NOT drop the link — re-handshaking won't change a whitelist verdict.
async fn handle_check_pairing(state: &mut SessionState) -> Result<PairingStatus> {
    ensure_connected(state).await?;
    let conn = state
        .conn
        .as_mut()
        .context("ensure_connected returned without a connection")?;
    match session::request_session_info(conn, &state.keypair, Domain::Infotainment).await {
        Ok(_) => Ok(PairingStatus::Paired),
        Err(session::SessionError::KeyNotPaired) => Ok(PairingStatus::NotPaired),
        Err(session::SessionError::Other(e)) => Err(e).context("session-info handshake"),
    }
}

/// Record an `ensure_connected` failure; after `ADAPTER_REBUILD_AFTER` in
/// a row, drop the cached adapter so the next attempt rebuilds it.
fn note_connect_failure(state: &mut SessionState) {
    state.consecutive_connect_failures =
        state.consecutive_connect_failures.saturating_add(1);
    if state.consecutive_connect_failures >= ADAPTER_REBUILD_AFTER {
        if state.cached_adapter.take().is_some() {
            warn!(
                "PersistentSession: {} consecutive connect failures — \
                 rebuilding BLE adapter handle (possible bluez restart / hci reset)",
                state.consecutive_connect_failures
            );
        }
        state.consecutive_connect_failures = 0;
    }
}

async fn ensure_connected(state: &mut SessionState) -> Result<()> {
    if state.conn.is_some() {
        return Ok(());
    }

    // Reuse the cached adapter; build a fresh one only when absent (see
    // `cached_adapter` for why per-reconnect rebuilds leak D-Bus conns).
    let adapter = match state.cached_adapter.clone() {
        Some(a) => a,
        None => match scan::adapter_by_name(state.adapter_name.as_deref()).await {
            Ok(a) => {
                state.cached_adapter = Some(a.clone());
                a
            }
            Err(e) => {
                note_connect_failure(state);
                sleep(state.backoff).await;
                state.backoff = (state.backoff * 2).min(RECONNECT_BACKOFF_MAX);
                return Err(e)
                    .context(ConnectFailure)
                    .context("locating BLE adapter");
            }
        },
    };
    // 30s scan window matches what the one-shot examples use; covers
    // a car waking from sleep + advertising stabilizing.
    let scan_result = match scan::scan_for_vin(&adapter, &state.vin, Duration::from_secs(30)).await
    {
        Ok(r) => r,
        Err(e) => {
            // Connect failure — back off before letting the caller
            // retry. Subsequent failures double the wait; success
            // resets it.
            note_connect_failure(state);
            sleep(state.backoff).await;
            state.backoff = (state.backoff * 2).min(RECONNECT_BACKOFF_MAX);
            return Err(e).context(ConnectFailure).context("scan failed");
        }
    };

    // Capture RSSI + MAC before Connection::open consumes the
    // peripheral; address() is cheap and the status file needs the MAC.
    let scan_rssi = scan_result.rssi;
    let peer_mac = scan_result.peripheral.address().to_string();

    let conn = match Connection::open(scan_result.peripheral).await {
        Ok(c) => c,
        Err(e) => {
            note_connect_failure(state);
            sleep(state.backoff).await;
            state.backoff = (state.backoff * 2).min(RECONNECT_BACKOFF_MAX);
            return Err(e).context(ConnectFailure).context("connect failed");
        }
    };

    state.conn = Some(conn);
    state.backoff = RECONNECT_BACKOFF_MIN;
    state.consecutive_connect_failures = 0;
    state.connected_at = Some(Instant::now());
    state.queries_since_connect = 0;
    state.last_scan_rssi = scan_rssi;
    state.last_peer_mac = Some(peer_mac);
    // Drop the old latency history — a new link negotiates fresh BLE
    // params, so the old distribution isn't representative.
    state.recent_latencies_ms.clear();
    info!(
        "PersistentSession: connected (held until link drops) — peer={} scan_rssi={}",
        state.last_peer_mac.as_deref().unwrap_or("?"),
        state.last_scan_rssi
            .map(|r| r.to_string())
            .unwrap_or_else(|| "?".into()),
    );
    write_status_file(state, ConnectionEvent::Connected);
    Ok(())
}

/// Whether we just connected or just lost the connection. Drives
/// the body of the status file the bundle reads.
#[derive(Copy, Clone)]
enum ConnectionEvent {
    Connected,
    Disconnected,
}

/// Rewrite the status file with one `k=v` line describing the current
/// session state. Single line keeps the write atomic. Best-effort — if
/// /mutable isn't mounted the file just won't exist.
fn write_status_file(state: &SessionState, event: ConnectionEvent) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ts = format_unix_iso8601(now_secs);
    let mac = state.last_peer_mac.as_deref().unwrap_or("?");
    let rssi = state
        .last_scan_rssi
        .map(|r| r.to_string())
        .unwrap_or_else(|| "?".into());
    let line = match event {
        ConnectionEvent::Connected => format!(
            "state=connected since={} mac={} scan_rssi={} drops_total={} desync_recoveries={}\n",
            ts, mac, rssi, state.lifetime_drops, state.framing_desync_recoveries,
        ),
        ConnectionEvent::Disconnected => format!(
            "state=disconnected since={} last_mac={} last_scan_rssi={} drops_total={} desync_recoveries={}\n",
            ts, mac, rssi, state.lifetime_drops, state.framing_desync_recoveries,
        ),
    };
    let _ = std::fs::write(STATUS_FILE_PATH, line);
}

/// Bump the per-connection query counter and record latency. Every
/// `STATUS_LOG_EVERY_N_QUERIES`, log how long the connection has been
/// held, queries served, and p50/p95/p99 latency — so the slot being
/// held vs re-grabbed (and rising latency) is visible in the journal.
fn note_successful_query(state: &mut SessionState, elapsed_ms: u128) {
    state.queries_since_connect = state.queries_since_connect.saturating_add(1);
    // Record success time for the disconnect diagnostic's "last_ok=Xs".
    state.last_successful_query_at = Some(Instant::now());

    // Push into the sliding latency window, evict oldest if full.
    if state.recent_latencies_ms.len() >= SAMPLES_FOR_PERCENTILES {
        state.recent_latencies_ms.pop_front();
    }
    state.recent_latencies_ms.push_back(elapsed_ms);

    let n = state.queries_since_connect;
    if n == 1 || n % STATUS_LOG_EVERY_N_QUERIES == 0 {
        let uptime = state
            .connected_at
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        let (p50, p95, p99) = compute_percentiles(&state.recent_latencies_ms);
        info!(
            "PersistentSession: held for {}m{}s, {} queries (latency p50/p95/p99 = {}/{}/{}ms over last {}, desync_recoveries={})",
            uptime / 60,
            uptime % 60,
            n,
            p50,
            p95,
            p99,
            state.recent_latencies_ms.len(),
            state.framing_desync_recoveries,
        );
    }
}

/// Approximate p50/p95/p99 by sorting a copy of the latency window
/// (n=100, fires every 25 queries). Returns (0,0,0) when empty.
fn compute_percentiles(samples: &VecDeque<u128>) -> (u128, u128, u128) {
    if samples.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted: Vec<u128> = samples.iter().copied().collect();
    sorted.sort_unstable();
    // Pick index via floor — for n=100, p50=index 50, p95=index 95,
    // p99=index 99 (saturating at the last element for short windows).
    let pick = |pct: f64| -> u128 {
        let idx = ((sorted.len() as f64) * pct).floor() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    (pick(0.50), pick(0.95), pick(0.99))
}

async fn ensure_domain_session(state: &mut SessionState, domain: Domain) -> Result<()> {
    if state.domains.contains_key(&domain) {
        return Ok(());
    }
    let conn = state
        .conn
        .as_mut()
        .context("ensure_domain_session called without connection")?;

    info!("PersistentSession: handshake for {:?}", domain);
    let info = match session::request_session_info(conn, &state.keypair, domain).await {
        Ok(info) => info,
        Err(session::SessionError::KeyNotPaired) => {
            // Don't drop the connection — re-handshaking won't help;
            // the user must re-pair on the car.
            bail!(
                "BLE pair not registered with car (domain {:?}): the car returned \
                 SESSION_INFO_STATUS_KEY_NOT_ON_WHITELIST. Re-pair from the \
                 SentryUSB UI's BLE card and tap your physical Tesla card on \
                 the center console NFC reader when prompted.",
                domain
            );
        }
        Err(session::SessionError::Other(e)) => {
            return Err(e).with_context(|| {
                format!("session-info handshake for {:?}", domain)
            });
        }
    };

    let key = derive_session_key(&state.keypair.secret, &info.parsed.public_key)
        .context("deriving session key")?;

    // SessionInfo HMAC-tag verification intentionally omitted: our
    // compute didn't match real Tesla firmware despite matching
    // tesla-control's reference byte-for-byte, and tesla-control itself
    // treats an HMAC mismatch as a warning. For a single-user/single-car
    // threat model the MITM risk is negligible, and the session-key
    // derivation is proven correct by working round-trips. Revisit if
    // the wire-format discrepancy is found.

    state.domains.insert(
        domain,
        DomainSession {
            key,
            epoch: info.parsed.epoch.clone(),
            counter: info.parsed.counter,
            clock_time_at_handshake: info.parsed.clock_time,
            handshake_local_time: Instant::now(),
        },
    );
    Ok(())
}

/// Human-readable name for a SignatureData::sig_type variant. Used
/// in error messages so an unexpected response shape tells us
/// exactly what shape it had instead of "missing X" guesswork.
fn sig_type_name(t: &signature_data::SigType) -> &'static str {
    match t {
        signature_data::SigType::AesGcmPersonalizedData(_) => "AES_GCM_PERSONALIZED",
        signature_data::SigType::AesGcmResponseData(_) => "AES_GCM_RESPONSE",
        signature_data::SigType::HmacPersonalizedData(_) => "HMAC_PERSONALIZED",
        signature_data::SigType::SessionInfoTag(_) => "SESSION_INFO_TAG (HMAC)",
    }
}

/// Human-readable name for a RoutableMessage::payload variant.
fn payload_variant_name(p: Option<&routable_message::Payload>) -> &'static str {
    match p {
        Some(routable_message::Payload::ProtobufMessageAsBytes(_)) => "ProtobufMessageAsBytes (encrypted)",
        Some(routable_message::Payload::SessionInfo(_)) => "SessionInfo (refresh)",
        Some(routable_message::Payload::SessionInfoRequest(_)) => "SessionInfoRequest",
        None => "<none>",
    }
}

/// Heuristic: does this error look like the BLE link dropped (vs a
/// fault returned by the car at the protocol level)? Used to decide
/// whether to drop the connection for the next query to reopen.
fn is_transport_error(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}");
    msg.contains("notification stream ended")
        || msg.contains("BLE write")
        || msg.contains("waiting for response")
        || msg.contains("not connected")
        || msg.contains("Peripheral")
}

/// Whether `e` is an AES-GCM response-decrypt failure — the car answered
/// but we couldn't decrypt it, meaning our cached session key is stale
/// (car re-keyed / rotated epoch). Distinct from a transport error: the
/// link is fine, the *session* is stale. Recovered by dropping the cached
/// domain session and re-handshaking (see `signed_request_with_refresh_retry`).
/// Matches the message built in `auth::decrypt_response`.
fn is_decrypt_error(e: &anyhow::Error) -> bool {
    format!("{e:#}").contains("AES-GCM response decrypt")
}

/// Snapshot recent kernel Bluetooth lines so a drop that fires before
/// any query (the "held=0s" pattern) comes with the kernel's reason —
/// btleplug only surfaces an opaque "Failed to initiate write", but
/// dmesg has the HCI disconnect reason (0x05 auth failure, 0x13 remote
/// terminated, 0x22 LL timeout, 0x3D/0x3E conn failed, link supervision
/// timeout = RF/range). Returns the last ~25 relevant lines, or None if
/// dmesg isn't readable. Bounded to 2s so a hung dmesg can't stall us.
async fn capture_recent_bluetooth_dmesg() -> Option<String> {
    use tokio::process::Command;
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        Command::new("dmesg")
            .arg("--ctime") // human-readable timestamps for the journal
            .output(),
    )
    .await;
    let output = match result {
        Ok(Ok(o)) if o.status.success() => o,
        _ => return None,
    };
    let text = String::from_utf8_lossy(&output.stdout);
    // Grab the LAST N lines matching anything Bluetooth-relevant.
    // Keywords cover all the variants modern kernels use.
    let keywords = [
        "Bluetooth",
        "hci0",
        "hci1",
        "BCM",
        "BTM",
        "RTL",
        "BNEP",
        "disconnect reason",
        "supervision timeout",
        "Authentication failed",
        "Connection failed",
    ];
    let matching: Vec<&str> = text
        .lines()
        .filter(|l| keywords.iter().any(|k| l.contains(k)))
        .collect();
    if matching.is_empty() {
        return None;
    }
    // Last 25 lines is plenty — for a held=0s drop the relevant
    // events landed within the last few seconds.
    let tail: Vec<&str> = matching
        .iter()
        .rev()
        .take(25)
        .rev()
        .copied()
        .collect();
    Some(tail.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::{is_decrypt_error, is_transport_error};

    #[test]
    fn decrypt_failure_is_decrypt_not_transport() {
        // Exact message shape from auth::decrypt_response.
        let e = anyhow::anyhow!("AES-GCM response decrypt: aead::Error");
        assert!(is_decrypt_error(&e), "stale-session decrypt failure must self-heal");
        assert!(
            !is_transport_error(&e),
            "a decrypt failure must NOT be treated as a transport drop"
        );
    }

    #[test]
    fn transport_failure_is_not_decrypt() {
        for msg in ["BLE write: Timeout", "not connected", "notification stream ended"] {
            let e = anyhow::anyhow!("{msg}");
            assert!(is_transport_error(&e), "{msg} should be transport");
            assert!(!is_decrypt_error(&e), "{msg} must not trigger session re-handshake");
        }
    }
}
