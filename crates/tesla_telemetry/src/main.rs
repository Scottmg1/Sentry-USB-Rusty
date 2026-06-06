//! `sentryusb-tesla-telemetry` — BLE telemetry sampler daemon.
//!
//! Runs as a systemd service alongside `sentryusb.service`. Watches
//! the USB gadget LUN for clip writes (car-awake signal), takes
//! samples via `tesla-control`, and inserts them into the
//! `telemetry_samples` table.
//!
//! Design notes:
//!   * Sampling rate adapts to car state — 15 s while awake, 15 min
//!     while asleep (using the non-waking `body-controller-state`).
//!   * Holds the `/tmp/ble_radio_owner` lock while sampling so the
//!     keep-awake nudge and iOS GATT daemon serialize cleanly.
//!   * Stops `sentryusb-ble.service` (iOS GATT) while the lock is
//!     held, restarts it on release.
//!   * Re-reads `sentryusb.conf` on every loop iteration — toggling
//!     BLE off in settings stops sampling within ~15 s without a
//!     daemon restart.

mod action_socket;
mod clock_sync;
mod config;
mod db;
mod diag_log;
mod keep_accessory;
mod lock;
mod sample;
mod sample_ble;
mod usb_watch;

use std::time::{Duration, Instant};

use anyhow::Result;
use rusqlite::Connection;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config::BleConfig;
use crate::sample::Sample;
use crate::usb_watch::CarState;

/// Lock-owner string this daemon writes into `/tmp/ble_radio_owner`.
/// Coordinated with `awake_start`'s owner string ("keep_awake").
const OWNER: &str = "telemetry";

/// Active-mode tick cadence. `state drive` runs every tick (highest
/// priority — carries shiftState + location + odometer); slower
/// sub-samplers run on their own intervals (see `Schedule`).
const DRIVE_INTERVAL: Duration = Duration::from_secs(15);

/// How often to refresh climate (cabin/exterior temp, HVAC) in
/// Active mode. 60s is fine — these are slow-changing.
const CLIMATE_INTERVAL: Duration = Duration::from_secs(60);

/// How often to refresh charge (battery %) in Active mode. 60s,
/// staggered 30s after climate so the two big-payload calls don't
/// both fire on the same tick.
const CHARGE_INTERVAL: Duration = Duration::from_secs(60);
const CHARGE_INITIAL_OFFSET: Duration = Duration::from_secs(30);

/// Tire-pressure refresh in Active mode. 5 min — TPMS barely changes
/// mid-drive.
const TIRES_INTERVAL: Duration = Duration::from_secs(300);

/// `state closures` refresh in Active mode. 60s. Sole source of
/// `sentry_mode_state` for the quiet-mode gate, which only cares about
/// transitions — a remote sentry toggle reaches us within ~1 tick.
const CLOSURES_INTERVAL: Duration = Duration::from_secs(60);

// No separate location sampler: Tesla returns location_name in `state
// drive` responses but not in `state location`, so `sample_drive_ble`
// pulls the address from the drive response at the 15s drive cadence.

/// Quiet-mode cadence for sleep-safe `body-controller-state` calls.
/// 30s keeps wakeup latency low (user_presence promotes us back to
/// Active); these calls don't wake the car, so polling often is cheap.
const QUIET_INTERVAL: Duration = Duration::from_secs(30);

/// Retry interval after a sub-sampler fails. The car drops BLE
/// connections aggressively to save battery, so retry within seconds
/// to catch its brief acceptance window before other clients refill
/// its connection table.
const FAST_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// Consecutive fast retries before backing off to normal cadence.
/// ~9s of aggressive retry — enough to catch a reconnection window
/// without burning power on a dead link.
const MAX_FAST_RETRIES: u32 = 3;

/// `state climate` + `state charge` refresh while in Quiet mode but
/// the car is provably awake (recent clip writes). body-controller-state
/// alone doesn't carry battery/temps/HVAC, so without this the
/// parked-with-Sentry dashboard would show frozen values. 3 min keeps
/// the cards alive at minimal BLE load; safe since the car is awake.
const PARKED_AWAKE_REFRESH_INTERVAL: Duration = Duration::from_secs(180);

/// `state tire-pressure` poll while parked-awake. 30 min — TPMS doesn't
/// change while parked, but this keeps the card fresh for users who
/// rarely drive (otherwise tires only update in Active mode).
const PARKED_AWAKE_TPMS_INTERVAL: Duration = Duration::from_secs(1800);

/// Consecutive Park polls before dropping to Quiet mode. 3 @ 15s = 45s
/// — rides through a stop at a light but lets the car sleep soon after
/// parking.
const PARK_CONFIRMATIONS_BEFORE_QUIET: u32 = 3;

/// Cadence for the keep-accessory geofence `state location` poll. Raw
/// GPS isn't bundled in `state drive`, so this is its own round-trip —
/// kept coarse (home/away changes slowly) and only run when the
/// keep-accessory feature is enabled, to spare BLE air time.
const LOCATION_POLL_INTERVAL: Duration = Duration::from_secs(30);

// Software version isn't sampled: `state software-update` returns only
// the pending OTA version, never the installed `car_version`. Users
// enter the running version manually (see fsd_versions.rs).

/// Backoff when another owner holds the BLE radio lock. Short so we
/// resume quickly when the keep-awake nudge releases.
const RADIO_CONTENDED_BACKOFF: Duration = Duration::from_secs(5);

/// How long to sleep when BLE is disabled in settings. Doesn't need
/// to be aggressive — settings changes are infrequent.
const DISABLED_POLL: Duration = Duration::from_secs(60);

/// Per-command "next due" timestamps for the Active-mode scheduler.
/// Each tick walks the poll types in priority order and runs any due;
/// `state drive` goes first (shiftState + locationName + odometer).
/// charge is staggered 30s off climate so the two big-payload calls
/// don't stack on one tick.
struct Schedule {
    next_drive: Instant,
    next_climate: Instant,
    next_charge: Instant,
    next_tires: Instant,
    /// `state closures` — read only for sentry_mode_state, which the
    /// quiet-mode gate needs each cycle.
    next_closures: Instant,
    /// Consecutive-failure counters for the fast-retry pattern: reset on
    /// success, increment on failure to drive 3s retries until
    /// MAX_FAST_RETRIES, then back off to normal cadence.
    drive_failures: u32,
    climate_failures: u32,
    charge_failures: u32,
    tires_failures: u32,
    closures_failures: u32,
}

impl Schedule {
    fn new(now: Instant) -> Self {
        Self {
            // Fire immediately on the first tick for a baseline snapshot
            // (incl. the sentry_mode + charging_state the quiet-mode gate
            // needs). charge waits 30s to stagger off climate.
            next_drive: now,
            next_climate: now,
            next_charge: now + CHARGE_INITIAL_OFFSET,
            next_tires: now,
            next_closures: now,
            drive_failures: 0,
            climate_failures: 0,
            charge_failures: 0,
            tires_failures: 0,
            closures_failures: 0,
        }
    }
    fn drive_due(&self, now: Instant) -> bool { now >= self.next_drive }
    fn climate_due(&self, now: Instant) -> bool { now >= self.next_climate }
    fn charge_due(&self, now: Instant) -> bool { now >= self.next_charge }
    fn tires_due(&self, now: Instant) -> bool { now >= self.next_tires }
    fn closures_due(&self, now: Instant) -> bool { now >= self.next_closures }

    /// Next-due instant for a sub-sampler that just ran: normal interval
    /// on success, ~3s retry within MAX_FAST_RETRIES, else normal.
    fn next_after(now: Instant, success: bool, failures: u32, normal: Duration) -> Instant {
        if success {
            now + normal
        } else if failures <= MAX_FAST_RETRIES {
            now + FAST_RETRY_INTERVAL
        } else {
            now + normal
        }
    }

    fn mark_drive(&mut self, now: Instant, success: bool) {
        self.drive_failures = if success { 0 } else { self.drive_failures.saturating_add(1) };
        self.next_drive = Self::next_after(now, success, self.drive_failures, DRIVE_INTERVAL);
    }
    fn mark_climate(&mut self, now: Instant, success: bool) {
        self.climate_failures = if success { 0 } else { self.climate_failures.saturating_add(1) };
        self.next_climate = Self::next_after(now, success, self.climate_failures, CLIMATE_INTERVAL);
    }
    fn mark_charge(&mut self, now: Instant, success: bool) {
        self.charge_failures = if success { 0 } else { self.charge_failures.saturating_add(1) };
        self.next_charge = Self::next_after(now, success, self.charge_failures, CHARGE_INTERVAL);
    }
    fn mark_tires(&mut self, now: Instant, success: bool) {
        self.tires_failures = if success { 0 } else { self.tires_failures.saturating_add(1) };
        self.next_tires = Self::next_after(now, success, self.tires_failures, TIRES_INTERVAL);
    }
    fn mark_closures(&mut self, now: Instant, success: bool) {
        self.closures_failures = if success { 0 } else { self.closures_failures.saturating_add(1) };
        self.next_closures = Self::next_after(now, success, self.closures_failures, CLOSURES_INTERVAL);
    }

    /// When should the next tick fire? Min of all next-due timestamps
    /// across every sub-sampler. The main loop sleeps until this
    /// instant.
    fn next_due(&self) -> Instant {
        self.next_drive
            .min(self.next_climate)
            .min(self.next_charge)
            .min(self.next_tires)
            .min(self.next_closures)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_target(false)
        .init();

    info!("sentryusb-tesla-telemetry starting");

    // Migration recovery: previous versions stopped sentryusb-ble.service
    // during Active mode to claim exclusive hci0. Existing installs that
    // are upgrading FROM that behavior may have sentryusb-ble in a
    // stopped state right now (we stopped it on the last cycle before
    // the upgrade, and never started it again because we got killed).
    //
    // One-shot start ensures the iOS GATT daemon is running by the time
    // we hand control to the main loop. From here on out we don't touch
    // sentryusb-ble at all — they coexist via BLE multi-role.
    //
    // Best-effort with a short timeout so a hung systemctl doesn't
    // block startup; sentryusb-ble.service has Restart=always anyway
    // so systemd will recover on the next start attempt regardless.
    let _ = sentryusb_shell::run_with_timeout(
        Duration::from_secs(5),
        "systemctl",
        &["start", "sentryusb-ble"],
    )
    .await;

    // Brief startup wait for the clock (RTC or NTP) to dodge the first
    // cold-boot tick. 30s is enough because BLE clock sync (clock_sync.rs)
    // takes over once the first state response lands.
    wait_for_clock_sync(Duration::from_secs(30)).await;

    let conn = db::open()?;

    // Background diagnostic logger — one line/min to
    // /mutable/sentryusb-ble.log for the Logs → Bluetooth UI tab.
    // Independent of the main loop; own read-only DB handle.
    diag_log::spawn(sentryusb_drives::DEFAULT_DB_PATH.into());

    // IPC bridge for external BLE actions (sentryusb-ble-action from
    // run/awake_start), letting keep-awake nudges reuse our warm
    // PersistentSession instead of stopping us to grab the radio.
    let (action_tx, mut action_rx) = mpsc::channel::<action_socket::ActionRequest>(8);
    action_socket::spawn(action_tx);

    let mut held_radio = false;
    // Consecutive Park polls; crossing PARK_CONFIRMATIONS_BEFORE_QUIET
    // drops to sleep-safe body-controller polling. Reset by any non-Park
    // shift or a user_presence flip back to PRESENT.
    let mut parked_polls: u32 = 0;
    // Last user_presence from body-controller-state; detects "driver got
    // back in" to promote Quiet → Active on the next tick.
    let mut last_user_presence: Option<bool> = None;
    // Per-command Active-mode scheduler; persists across ticks. First
    // tick fires drive + climate + tires immediately, charge +30s.
    let mut schedule = Schedule::new(Instant::now());
    // Last parked-awake refresh (climate + charge while Quiet but
    // recording), keeping battery/temps fresh during Sentry/charging.
    let mut last_parked_awake_refresh: Option<Instant> = None;
    // Separate, rarer TPMS timer (30 min) — TPMS doesn't change while
    // parked. Bundled into the same Sample row when both fire together.
    let mut last_parked_awake_tpms_refresh: Option<Instant> = None;
    // Long-lived BLE session, lazy-spawned in tick() and reused to avoid
    // re-scan + re-handshake each cycle. Recreated if the VIN changes.
    let mut ble_session: Option<sample_ble::SessionHandle> = None;
    // Last charging_state from a successful `state charge` poll. Drives
    // the quiet-mode gate: active charging keeps the car awake, so
    // quieting would leave battery_pct stale. `None` → assume charging
    // (stay Active until proven otherwise).
    let mut last_charging_state: Option<sample::ChargingState> = None;
    // Same gate for sentry mode (from `state closures`): any non-Off
    // value keeps the car awake. `None` → assume on.
    let mut last_sentry_mode: Option<sample::SentryMode> = None;
    // Throttle for the staying-Active gate log (~1/min).
    let mut last_gate_log: Option<Instant> = None;
    // Last known GPS from the drive poll (held across ticks; parked
    // polls legitimately omit coords). Feeds the keep-accessory geofence.
    let mut last_lat: Option<f64> = None;
    let mut last_lon: Option<f64> = None;
    // Last known reverse-geocoded address from the drive poll, held across
    // ticks. Tesla returns locationName only on `state drive`, which doesn't
    // run every tick — so a short charge (no drive poll) would otherwise get
    // a sample with no address. Held + stamped like lat/lon so every sample
    // carries the last-known address.
    let mut last_location_name: Option<String> = None;
    // Throttle for the geofence `state location` poll (keep-accessory only).
    let mut last_location_poll: Option<Instant> = None;
    // Keep-Accessory-Power automation policy state (see keep_accessory.rs).
    let mut keep_accessory_state = keep_accessory::KeepAccessoryState::default();

    // SIGTERM handler — release the radio on shutdown so the iOS
    // GATT daemon can come back up cleanly.
    let mut sigterm = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::terminate(),
    )?;
    let mut sigint = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::interrupt(),
    )?;
    // SIGUSR1 = "do a full state poll now" — fired by the
    // /api/system/ble-force-poll endpoint when the user clicks
    // the "Poll now" button. Forces the next-due time of every
    // sub-sampler to "now" and resets the parked-awake refresh
    // timer, so the next tick runs everything regardless of the
    // current phase.
    let mut sigusr1 = tokio::signal::unix::signal(
        tokio::signal::unix::SignalKind::user_defined1(),
    )?;

    loop {
        tokio::select! {
            _ = sigterm.recv() => {
                info!("SIGTERM received, releasing radio and exiting");
                if held_radio { release_radio().await; }
                return Ok(());
            }
            _ = sigint.recv() => {
                info!("SIGINT received, releasing radio and exiting");
                if held_radio { release_radio().await; }
                return Ok(());
            }
            _ = sigusr1.recv() => {
                info!("SIGUSR1 received — forcing a full state poll on next tick (all sub-samplers due immediately)");
                let now = Instant::now();
                // Force every sub-sampler due now — no charge stagger, no
                // fast-retry gating. "Poll now" should return a full fresh
                // read in one cycle, not a battery field stale by 30s.
                schedule = Schedule::new(now);
                schedule.next_drive = now;
                schedule.next_climate = now;
                schedule.next_charge = now;
                schedule.next_closures = now;
                schedule.next_tires = now;
                last_parked_awake_refresh = None;
                last_parked_awake_tpms_refresh = None;
                // Reset parked_polls so the phase flips to Active even
                // from parked-confirmed Quiet (else we'd only fire a
                // body_controller call). Next Park observation re-ticks it.
                parked_polls = 0;
            }
            Some(req) = action_rx.recv() => {
                // IPC: an external process (sentryusb-ble-action) wants a
                // one-shot action through our PersistentSession. Serializes
                // naturally with the select.
                handle_action_request(
                    req,
                    &mut held_radio,
                    &mut ble_session,
                ).await;
            }
            sleep = tick(
                &conn,
                &mut held_radio,
                &mut parked_polls,
                &mut last_user_presence,
                &mut schedule,
                &mut last_parked_awake_refresh,
                &mut last_parked_awake_tpms_refresh,
                &mut ble_session,
                &mut last_charging_state,
                &mut last_sentry_mode,
                &mut last_gate_log,
                &mut last_lat,
                &mut last_lon,
                &mut last_location_name,
                &mut last_location_poll,
            ) => {
                // Keep-Accessory-Power automation runs after each tick
                // with the freshly-updated signals. Best-effort; gated
                // on the 12V flag + home geofence inside evaluate(), and
                // a no-op until both are configured.
                if let Ok(cfg) = config::BleConfig::load() {
                    if let Some(handle) = ble_session.as_ref() {
                        keep_accessory::evaluate(
                            &cfg.keep_accessory,
                            &handle.session,
                            &mut keep_accessory_state,
                            last_lat,
                            last_lon,
                            // "parked" must be HW3-robust: HW3 reports
                            // shift_state=Unknown when parked, so the
                            // shift-based counter never confirms. OR in
                            // car_truly_asleep (the daemon's own quiet
                            // signal) so home→OFF works on HW3 too.
                            parked_polls >= PARK_CONFIRMATIONS_BEFORE_QUIET
                                || usb_watch::observe() == CarState::Asleep,
                            lock::is_archive_active(),
                            held_radio,
                        )
                        .await;
                    }
                }
                tokio::time::sleep(sleep).await;
            }
        }
    }
}

/// Whether the gate may drop to sleep-permitting (Quiet) polling.
/// Quiet means "let the car sleep": permitted only when the car is
/// parked-or-asleep AND nothing has a reason to hold it awake — not
/// charging, sentry off, and no keep-awake (archive cycle or web-UI /
/// manual nudge) in effect.
///
/// The keep-awake term is the fix for letting a parked car sleep
/// mid-archive: an archive disconnects the USB gadget, so the car both
/// confirms Park (drive computer powers down → Unknown shift) and later
/// trips `car_truly_asleep` (cam_disk stops updating). The override has to
/// sit on the whole decision, not just one path, or a >5min archive slips
/// through the asleep door.
fn should_enter_quiet(
    car_truly_asleep: bool,
    parked_confirmed: bool,
    actively_charging: bool,
    sentry_on: bool,
    keep_awake_active: bool,
) -> bool {
    (car_truly_asleep || parked_confirmed)
        && !actively_charging
        && !sentry_on
        && !keep_awake_active
}

/// One main-loop iteration; returns how long to sleep before the next.
///
/// Two phases, decided each tick:
///   * **Active** — clip writes happening and shift_state not
///     confirmed-Park. Full `state` polls, radio held continuously.
///   * **Quiet** — car asleep OR Park for PARK_CONFIRMATIONS_BEFORE_QUIET
///     polls. Sleep-safe body-controller-state polls; radio released
///     between deep-asleep polls (for iOS GATT) but held during
///     parked-with-Sentry (cadence too fast to cycle GATT).
///
/// Active → Quiet when parked_polls hits the count; Quiet → Active when
/// user_presence flips NOT_PRESENT → PRESENT.
async fn tick(
    conn: &Connection,
    held_radio: &mut bool,
    parked_polls: &mut u32,
    last_user_presence: &mut Option<bool>,
    schedule: &mut Schedule,
    last_parked_awake_refresh: &mut Option<Instant>,
    last_parked_awake_tpms_refresh: &mut Option<Instant>,
    ble_session: &mut Option<sample_ble::SessionHandle>,
    last_charging_state: &mut Option<sample::ChargingState>,
    last_sentry_mode: &mut Option<sample::SentryMode>,
    last_gate_log: &mut Option<Instant>,
    last_lat: &mut Option<f64>,
    last_lon: &mut Option<f64>,
    last_location_name: &mut Option<String>,
    last_location_poll: &mut Option<Instant>,
) -> Duration {
    let cfg = match BleConfig::load() {
        Ok(c) => c,
        Err(e) => {
            warn!("failed to load BLE config: {e}");
            return DISABLED_POLL;
        }
    };

    // Lazy-spawn / recreate-on-VIN-change the persistent BLE session.
    // Cheap when it exists (a VIN compare); the first call does the
    // key-load + scan + connect + handshake.
    if let Err(e) = sample_ble::ensure_session_for(ble_session, &cfg.vin, Some(&cfg.adapter)) {
        warn!("could not start PersistentSession (will retry next tick): {e:#}");
        return Duration::from_secs(5);
    }
    let session = &ble_session
        .as_ref()
        .expect("ensure_session_for set it on success")
        .session;

    if !cfg.enabled {
        if *held_radio {
            info!("BLE disabled in settings — releasing radio");
            release_radio().await;
            *held_radio = false;
        }
        *parked_polls = 0;
        *last_user_presence = None;
        return DISABLED_POLL;
    }
    if cfg.vin.is_empty() {
        debug!("no TESLA_BLE_VIN configured, idling");
        if *held_radio {
            release_radio().await;
            *held_radio = false;
        }
        *parked_polls = 0;
        *last_user_presence = None;
        return DISABLED_POLL;
    }

    let observation = usb_watch::observe();
    let car_truly_asleep = observation == CarState::Asleep;
    let parked_confirmed = *parked_polls >= PARK_CONFIRMATIONS_BEFORE_QUIET;

    // Conservative defaults: with no successful charge/closures poll yet,
    // assume charging / sentry-on to stay Active. Costs a brief Active
    // burst (~30-60s) at cold start before the first polls confirm the
    // car can sleep.
    let actively_charging = last_charging_state
        .map(|s| s.is_active_charging())
        .unwrap_or(true);
    let sentry_on = last_sentry_mode.map(|s| s.is_on()).unwrap_or(true);

    // A keep-awake in effect — an archive cycle or any nudge loop (web-UI,
    // drive processing) — must pin us Active. Dropping to sleep-safe
    // polling mid-archive lets the car sleep, which cuts USB power and
    // aborts the copy. Self-clears when the work finishes, so it can't
    // wedge the car permanently awake (see lock::keep_awake_requested).
    let keep_awake_active = lock::keep_awake_requested();

    // Two paths to quiet mode: car_truly_asleep (no recent clip writes)
    // or parked_confirmed (Park 3+ polls). Both also require the car isn't
    // charging, running sentry, or being held awake for an archive — those
    // keep it awake, and quieting would leave battery_pct/sentry stale or
    // break the copy.
    let in_quiet_mode = should_enter_quiet(
        car_truly_asleep,
        parked_confirmed,
        actively_charging,
        sentry_on,
        keep_awake_active,
    );

    // Diagnostic: the car is parked/asleep but we're staying Active
    // because charging or sentry says it has a reason to be awake. This
    // is the #1 "why won't my car sleep?" question — surface which
    // signal pinned us Active and, crucially, whether it's a real
    // reading or the conservative unknown-default (`None` → assume on).
    // A persistent `[DEFAULTED]` here means Tesla isn't reporting that
    // field over BLE (it drops optional fields), so the car can never
    // qualify for Quiet. Throttled to ~once/min.
    if !in_quiet_mode {
        let now = Instant::now();
        let due = last_gate_log
            .map(|t| now.duration_since(t) >= Duration::from_secs(60))
            .unwrap_or(true);
        if due {
            *last_gate_log = Some(now);
            let sentry_src = if last_sentry_mode.is_some() {
                "read"
            } else {
                "DEFAULTED: unread"
            };
            let charge_src = if last_charging_state.is_some() {
                "read"
            } else {
                "DEFAULTED: unread"
            };
            // Quiet needs (asleep || parked_confirmed) && !charge &&
            // !sentry && !keep_awake. Log all five so a stuck-Active gate
            // is diagnosable; keep_awake_active=true is an archive/nudge
            // hold (the deliberate stay-awake during a copy).
            info!(
                "gate: staying Active — car_truly_asleep={}, parked_polls={}/{}, \
                 sentry_on={} [{}], actively_charging={} [{}], keep_awake_active={}",
                car_truly_asleep,
                *parked_polls,
                PARK_CONFIRMATIONS_BEFORE_QUIET,
                sentry_on,
                sentry_src,
                actively_charging,
                charge_src,
                keep_awake_active,
            );
        }
    }

    if in_quiet_mode {
        // Sleep-safe path: acquire the radio for the brief BC call, then
        // release if truly asleep (so iOS GATT returns). Keep it held in
        // parked-confirmed — cycling GATT at the poll cadence is wasteful.
        let acquired = if *held_radio {
            true
        } else {
            match lock::try_acquire(OWNER) {
                Ok(true) => {
                    *held_radio = true;
                    stop_ios_gatt().await;
                    true
                }
                Ok(false) => {
                    // info-level: a held radio (e.g. archiveloop's
                    // keep_awake during archive cycles) is a common reason
                    // quiet samples go missing — surface it as "waiting",
                    // not "broken".
                    info!(
                        "radio held by {:?} during quiet poll, skipping",
                        lock::current_owner()
                    );
                    false
                }
                Err(e) => {
                    warn!("failed to acquire radio lock for quiet poll: {e}");
                    false
                }
            }
        };

        if acquired {
            // Always probe body-controller first — it's the
            // canonical source of user_presence and is sleep-safe.
            let presence_now = match sample_ble::sample_body_controller_ble(session).await {
                Ok(bc) => {
                    let p = bc.user_presence;
                    persist(conn, bc.sample);
                    p
                }
                Err(e) => {
                    warn!("sample_body_controller failed: {e}");
                    *last_user_presence
                }
            };

            // Driver-got-back-in: user_presence NOT_PRESENT → PRESENT.
            // Promote to Active immediately (short Duration → state poll
            // next tick instead of a full QUIET_INTERVAL).
            if *last_user_presence == Some(false) && presence_now == Some(true) {
                info!("user_presence flipped PRESENT — resuming full state polls");
                *parked_polls = 0;
                *last_user_presence = presence_now;
                if car_truly_asleep {
                    release_radio().await;
                    *held_radio = false;
                }
                // 1s so the OS scheduler gets a moment; effectively
                // immediate next tick → state poll.
                return Duration::from_secs(1);
            }

            // When the user is in the car AND we're in Quiet
            // (because shift_state was Park last we checked), also
            // poll `state drive` to catch a shift change. This
            // covers the "user sat in parked car for a while then
            // drove away" case where user_presence never flips.
            // Drive-only (not the full telemetry batch) because
            // we just need shiftState here — the full Active mode
            // scheduler kicks in on the next tick if we detect a
            // shift change.
            if presence_now == Some(true) {
                match sample_ble::sample_drive_ble(session).await {
                    Ok(d) => {
                        if cfg.experimental {
                            sample_ble::log_drive_detail(&d);
                        }
                        // Self-correct the Pi's clock if it's
                        // significantly off — uses Tesla's
                        // GPS-derived timestamp from the response.
                        // No-op when local clock is already close.
                        try_sync_clock(d.meta);
                        let shift_changed_to_drive = d
                            .shift_state
                            .map_or(false, |s| !s.is_park() && s != sample::ShiftState::Unknown);
                        // Persist whatever the drive probe got
                        // (location + odometer freshness even
                        // while parked-with-Sentry).
                        let probe_sample = Sample {
                            ts: sample::now_secs(),
                            location_name: d.location_name,
                            odometer_mi: d.odometer_mi,
                            source: "state".into(),
                            ..Sample::default()
                        };
                        persist(conn, probe_sample);
                        if shift_changed_to_drive {
                            info!(
                                "shift_state non-Park while user in car — resuming full state polls"
                            );
                            *parked_polls = 0;
                            *last_user_presence = presence_now;
                            // Reset schedule so Active starts fresh
                            // with a full snapshot.
                            *schedule = Schedule::new(Instant::now());
                            return Duration::from_secs(1);
                        }
                    }
                    Err(e) => {
                        warn!("state drive probe in quiet+present failed: {e}");
                    }
                }
            }

            // Parked-awake state refresh: when the car is parked
            // (Quiet mode) but actively recording dashcam clips
            // (observation == Awake), do a periodic climate +
            // charge poll so battery/temps don't go indefinitely
            // stale during Sentry sessions or AC charging. Safe
            // because the car is already awake — we add no
            // wake-up drain. Only fires every 3 min to keep BLE
            // load minimal.
            //
            // Skipped when car_truly_asleep (let it sleep) or
            // when the user is in the car (the drive probe above
            // already covers that path and a state transition is
            // imminent).
            if observation == CarState::Awake && presence_now != Some(true) {
                // Two independent timers in this branch:
                //   * `refresh_due`   — climate + charge every 3 min
                //   * `tpms_due`      — tire pressure every 30 min
                // Either firing opens this poll cycle; both can fire
                // in the same tick and get bundled into one Sample.
                let refresh_due = last_parked_awake_refresh
                    .map(|t| t.elapsed() >= PARKED_AWAKE_REFRESH_INTERVAL)
                    .unwrap_or(true);
                let tpms_due = last_parked_awake_tpms_refresh
                    .map(|t| t.elapsed() >= PARKED_AWAKE_TPMS_INTERVAL)
                    .unwrap_or(true);

                if refresh_due || tpms_due {
                    let mut refresh = Sample {
                        ts: sample::now_secs(),
                        source: "state".into(),
                        ..Sample::default()
                    };
                    let mut any_ok = false;

                    if refresh_due {
                        match sample_ble::sample_climate_ble(session).await {
                            Ok(c) => {
                                if cfg.experimental {
                                    sample_ble::log_climate_detail(&c);
                                }
                                try_sync_clock(c.meta);
                                refresh.interior_temp_c = c.interior_temp_c;
                                refresh.exterior_temp_c = c.exterior_temp_c;
                                refresh.hvac_on = c.hvac_on;
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake climate refresh failed: {e}"),
                        }
                        match sample_ble::sample_charge_ble(session).await {
                            Ok(c) => {
                                if cfg.experimental {
                                    sample_ble::log_charge_detail(&c);
                                }
                                try_sync_clock(c.meta);
                                refresh.battery_pct = c.battery_pct;
                                // Also refresh the gate input so a
                                // charge that starts mid-quiet bumps
                                // us back to Active on the next tick.
                                if let Some(cs) = c.charging_state {
                                    *last_charging_state = Some(cs);
                                }
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake charge refresh failed: {e}"),
                        }
                        // Closures refresh — gives us a sentry_mode
                        // update so a remotely-enabled sentry session
                        // also bumps us back to Active. No persisted
                        // fields, so this doesn't affect `any_ok`.
                        match sample_ble::sample_closures_ble(session).await {
                            Ok(c) => {
                                if cfg.experimental {
                                    sample_ble::log_closures_detail(&c);
                                }
                                try_sync_clock(c.meta);
                                if let Some(sm) = c.sentry_mode {
                                    *last_sentry_mode = Some(sm);
                                }
                            }
                            Err(e) => warn!("parked-awake closures refresh failed: {e}"),
                        }
                        // Location not refreshed: Tesla only returns
                        // location_name in `state drive`, which Quiet
                        // doesn't call. Fine — parked means not moving.
                        *last_parked_awake_refresh = Some(Instant::now());
                    }

                    if tpms_due {
                        // TPMS rarely changes while parked, but
                        // periodic checks confirm sensors still
                        // report and feed the dashboard's TPMS card.
                        match sample_ble::sample_tires_ble(session).await {
                            Ok(t) => {
                                try_sync_clock(t.meta);
                                refresh.tire_fl_psi = t.tire_fl_psi;
                                refresh.tire_fr_psi = t.tire_fr_psi;
                                refresh.tire_rl_psi = t.tire_rl_psi;
                                refresh.tire_rr_psi = t.tire_rr_psi;
                                any_ok = true;
                            }
                            Err(e) => warn!("parked-awake tires refresh failed: {e}"),
                        }
                        *last_parked_awake_tpms_refresh = Some(Instant::now());
                    }

                    if any_ok {
                        persist(conn, refresh);
                    }
                }
            }

            *last_user_presence = presence_now;
            if car_truly_asleep {
                // Deep sleep + no user → hand the radio back to
                // iOS GATT between polls.
                release_radio().await;
                *held_radio = false;
            }
        }
        QUIET_INTERVAL
    } else {
        // Active mode — scheduler-driven multi-poll. Each tick runs the
        // overdue sub-samplers in priority order, `state drive` first
        // (shiftState + location + odometer); the rest on slower cadences.
        if !*held_radio {
            match lock::try_acquire(OWNER) {
                Ok(true) => {
                    *held_radio = true;
                    stop_ios_gatt().await;
                }
                Ok(false) => {
                    info!(
                        "radio held by {:?}, backing off {}s",
                        lock::current_owner(),
                        RADIO_CONTENDED_BACKOFF.as_secs()
                    );
                    return RADIO_CONTENDED_BACKOFF;
                }
                Err(e) => {
                    warn!("failed to acquire radio lock: {e}");
                    return RADIO_CONTENDED_BACKOFF;
                }
            }
        }

        let tick_now = Instant::now();
        // First tick after a long Quiet period: next_drive is very stale
        // (Quiet never calls mark_drive). Reset so the stagger returns
        // and all sub-samplers fire now for a fresh snapshot.
        if tick_now.duration_since(schedule.next_drive)
            > Duration::from_secs(2 * DRIVE_INTERVAL.as_secs())
        {
            *schedule = Schedule::new(tick_now);
        }
        // One Sample built across the sub-samplers that ran this tick;
        // unran/failed fields stay None (schema + aggregator handle NULLs).
        let mut sample = Sample {
            ts: sample::now_secs(),
            source: "state".into(),
            ..Sample::default()
        };
        let mut shift_state_observed: Option<sample::ShiftState> = None;
        let mut any_call_ran = false;

        // ── 1. DRIVE (priority) ── shiftState, locationName, odometer.
        if schedule.drive_due(tick_now) {
            let success = match sample_ble::sample_drive_ble(session).await {
                Ok(d) => {
                    if cfg.experimental {
                        sample_ble::log_drive_detail(&d);
                    }
                    try_sync_clock(d.meta);
                    sample.odometer_mi = d.odometer_mi;
                    // Hold the last-known address across ticks (the stamp
                    // below applies it to every sample, so a charge-only
                    // tick still carries the address).
                    if d.location_name.is_some() {
                        *last_location_name = d.location_name.clone();
                    }
                    shift_state_observed = d.shift_state;
                    // Hold last-known GPS across ticks (parked polls omit
                    // coords); feeds the keep-accessory geofence.
                    if d.lat.is_some() {
                        *last_lat = d.lat;
                    }
                    if d.lon.is_some() {
                        *last_lon = d.lon;
                    }
                    true
                }
                Err(e) => {
                    warn!("sample_drive failed: {e}");
                    false
                }
            };
            // Fast-retry on failure (~3s), normal interval on
            // success. See Schedule::next_after for the pattern.
            schedule.mark_drive(tick_now, success);
            any_call_ran = true;
        }

        // ── 1b. LOCATION (raw GPS) ── `state drive` returns the address
        // name but NOT raw coords when parked, so lat/lon need their own
        // `state location` round-trip. Two consumers: the keep-accessory
        // home geofence, and the charging-map pin (which needs coords, not
        // just the address). Run it when keep-accessory is on OR the car is
        // actively charging — so a charge gets a map pin without polling
        // GPS on every idle parked car. Coarse cadence; pure input (not a
        // DB sample) — doesn't set any_call_ran.
        let charging_now = last_charging_state
            .as_ref()
            .map(|s| s.is_active_charging())
            .unwrap_or(false);
        if cfg.keep_accessory.enabled || charging_now {
            let due = last_location_poll
                .map(|t| tick_now.duration_since(t) >= LOCATION_POLL_INTERVAL)
                .unwrap_or(true);
            if due {
                match sample_ble::sample_location_ble(session).await {
                    Ok((la, lo)) => {
                        if la.is_some() {
                            *last_lat = la;
                        }
                        if lo.is_some() {
                            *last_lon = lo;
                        }
                        *last_location_poll = Some(tick_now);
                        info!(
                            "state-poll: location gps=({}, {})",
                            la.map(|v| format!("{v:.6}")).unwrap_or_else(|| "?".into()),
                            lo.map(|v| format!("{v:.6}")).unwrap_or_else(|| "?".into()),
                        );
                        // Expose the current fix for the web UI's "Use
                        // current location" button (sets the home geofence).
                        // Best-effort; tiny JSON on the writable /mutable.
                        if let (Some(la), Some(lo)) = (la, lo) {
                            let json = format!(
                                "{{\"lat\":{la},\"lon\":{lo},\"ts\":{}}}\n",
                                sample::now_secs()
                            );
                            let _ = std::fs::write("/mutable/keep_accessory_gps.json", json);
                        }
                    }
                    Err(e) => warn!("state-poll: location failed: {e}"),
                }
            }
        }

        // ── 2. CLIMATE (every 60s) ──
        if schedule.climate_due(tick_now) {
            let success = match sample_ble::sample_climate_ble(session).await {
                Ok(c) => {
                    if cfg.experimental {
                        sample_ble::log_climate_detail(&c);
                    }
                    try_sync_clock(c.meta);
                    sample.interior_temp_c = c.interior_temp_c;
                    sample.exterior_temp_c = c.exterior_temp_c;
                    sample.hvac_on = c.hvac_on;
                    true
                }
                Err(e) => {
                    warn!("sample_climate failed: {e}");
                    false
                }
            };
            schedule.mark_climate(tick_now, success);
            any_call_ran = true;
        }

        // ── 3. CHARGE (every 60s, offset 30s from climate) ──
        if schedule.charge_due(tick_now) {
            let success = match sample_ble::sample_charge_ble(session).await {
                Ok(c) => {
                    if cfg.experimental {
                        sample_ble::log_charge_detail(&c);
                    }
                    // Persist the charging detail (v11 columns). Charging is a
                    // standard feature now (graduated off the experimental
                    // flag), so these are always written — NULL only when the
                    // car isn't reporting charge data.
                    let d = &c.detail;
                    sample.charger_power_kw = d.charger_power_kw;
                    sample.charger_actual_current_a = d.charger_actual_current_a;
                    sample.charger_voltage_v = d.charger_voltage_v;
                    sample.charge_rate_mph = d.charge_rate_mph;
                    sample.charge_energy_added_kwh = d.charge_energy_added_kwh;
                    sample.charge_limit_soc = d.charge_limit_soc;
                    sample.battery_range_mi = d.battery_range_mi;
                    sample.charge_minutes_to_full = d.minutes_to_full_charge;
                    try_sync_clock(c.meta);
                    sample.battery_pct = c.battery_pct;
                    // Refresh the gate input on success; keep the previous
                    // value on failure (don't force an Active burst on one
                    // transient miss).
                    if let Some(cs) = c.charging_state {
                        *last_charging_state = Some(cs);
                    }
                    true
                }
                Err(e) => {
                    warn!("sample_charge failed: {e}");
                    false
                }
            };
            schedule.mark_charge(tick_now, success);
            any_call_ran = true;
        }

        // ── 4. CLOSURES (every 60s) ── consumed only for sentry_mode
        // (the quiet-mode gate); door/window/port state is in the same
        // response if the UI ever needs it.
        if schedule.closures_due(tick_now) {
            let success = match sample_ble::sample_closures_ble(session).await {
                Ok(c) => {
                    if cfg.experimental {
                        sample_ble::log_closures_detail(&c);
                    }
                    try_sync_clock(c.meta);
                    if let Some(sm) = c.sentry_mode {
                        *last_sentry_mode = Some(sm);
                    }
                    true
                }
                Err(e) => {
                    warn!("sample_closures failed: {e}");
                    false
                }
            };
            schedule.mark_closures(tick_now, success);
            any_call_ran = true;
        }

        // ── 5. TIRES (every 5 min) ──
        if schedule.tires_due(tick_now) {
            let success = match sample_ble::sample_tires_ble(session).await {
                Ok(t) => {
                    try_sync_clock(t.meta);
                    sample.tire_fl_psi = t.tire_fl_psi;
                    sample.tire_fr_psi = t.tire_fr_psi;
                    sample.tire_rl_psi = t.tire_rl_psi;
                    sample.tire_rr_psi = t.tire_rr_psi;
                    true
                }
                Err(e) => {
                    warn!("sample_tires failed: {e}");
                    false
                }
            };
            schedule.mark_tires(tick_now, success);
            any_call_ran = true;
        }

        // Parked = not in a driving gear. Park or Unknown both count:
        // Intel-MCU Teslas report the gear as Invalid/SNA (→ Unknown) once
        // the drive computer powers down on parking, while a moving car
        // always reports a real gear. Drive/Reverse/Neutral reset; None
        // (drive poll didn't run / failed) leaves the counter alone.
        match shift_state_observed {
            Some(
                sample::ShiftState::Drive
                | sample::ShiftState::Reverse
                | sample::ShiftState::Neutral,
            ) => {
                *parked_polls = 0;
            }
            Some(_) => {
                // Park, or Unknown (drive computer asleep).
                *parked_polls = parked_polls.saturating_add(1);
                if *parked_polls == PARK_CONFIRMATIONS_BEFORE_QUIET {
                    // One-shot on first confirm; charge/sentry/keep-awake
                    // decide the next tick.
                    if actively_charging || sentry_on || keep_awake_active {
                        info!(
                            "{} consecutive parked observations — but staying Active \
                             (actively_charging={}, sentry_on={}, keep_awake_active={}); \
                             car is awake for a reason, quiet polling would freeze \
                             battery/sentry signals or break an in-flight archive",
                            PARK_CONFIRMATIONS_BEFORE_QUIET,
                            actively_charging,
                            sentry_on,
                            keep_awake_active,
                        );
                    } else {
                        info!(
                            "{} consecutive parked observations — dropping to body-controller polling so the car can sleep",
                            PARK_CONFIRMATIONS_BEFORE_QUIET
                        );
                    }
                }
            }
            None => {
                // Drive poll didn't run / failed — leave the counter alone.
            }
        }

        // Clear user_presence so the next Quiet entry starts from a
        // fresh baseline before the "got back in" check.
        *last_user_presence = None;

        // Persist whatever this tick collected; sparse rows (e.g.
        // drive-only) are handled downstream.
        if any_call_ran {
            // Stamp the held last-known GPS onto the row so a parked-and-
            // charging sample carries the charger's location for the
            // charging-view map pin. Parked polls omit fresh coords, so
            // `*last_lat`/`*last_lon` (held across ticks) is the right
            // source. Always written now (charging graduated off the flag);
            // NULL until a location poll has supplied coords.
            sample.latitude = *last_lat;
            sample.longitude = *last_lon;
            // Stamp the held address too (same rationale as lat/lon): a
            // charge-only tick where the drive poll didn't run still gets
            // the last-known address, so a short charge shows it. Only
            // overwrite from the held value when this tick didn't already
            // set one.
            if sample.location_name.is_none() {
                sample.location_name = last_location_name.clone();
            }
            persist(conn, sample);
        }

        // Live gate snapshot for the BLE card (not the DB).
        write_gate_status_file(*last_sentry_mode, *last_charging_state, shift_state_observed);

        // Sleep until the next sub-sampler is due (usually drive, 15s).
        let next = schedule.next_due();
        let after = Instant::now();
        if next > after {
            next.duration_since(after)
        } else {
            // Already overdue — tick again immediately.
            Duration::from_millis(100)
        }
    }
}

/// Block until the clock looks correct (year >= 2025 or timesyncd's
/// synced marker), or `timeout` elapses. Without an RTC battery the Pi
/// boots with a years-off clock until NTP catches up, and samples with
/// bad timestamps fall outside any drive window and are unrecoverable —
/// so don't sample until the clock is sane.
async fn wait_for_clock_sync(timeout: Duration) {
    if clock_is_sane() {
        debug!("clock looks sane on startup; no wait needed");
        return;
    }
    info!(
        "system clock is not synced yet — pausing sampler until \
         NTP catches up (max {}s). Install an RTC battery on the \
         Pi's BAT pin to avoid this on subsequent boots.",
        timeout.as_secs()
    );
    let deadline = std::time::Instant::now() + timeout;
    let mut last_log = std::time::Instant::now();
    let log_every = Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(5)).await;
        if clock_is_sane() {
            info!("system clock is now synced; resuming sampler");
            return;
        }
        if last_log.elapsed() >= log_every {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            info!(
                "still waiting for clock sync ({}s remaining)",
                remaining.as_secs()
            );
            last_log = std::time::Instant::now();
        }
    }
    warn!(
        "clock sync timeout reached — starting sampler anyway. \
         Telemetry written before NTP eventually syncs may not \
         match drives correctly."
    );
}

/// "Is the system clock plausibly correct?" — two signals, either
/// one is enough:
///   1. systemd-timesyncd has set its synchronized marker
///   2. The year is >= 2025 (anything in or after the year this
///      code was written; rules out the typical 1970 / 2000 / 2014
///      fallback values that show up on a Pi without RTC)
fn clock_is_sane() -> bool {
    // systemd-timesyncd marker — touched the moment a successful NTP
    // exchange happens, persists across reboots if the rootfs is
    // writable.
    if std::path::Path::new("/run/systemd/timesync/synchronized").exists() {
        return true;
    }
    // Year sanity check — a Pi with an RTC battery will pass this
    // immediately on boot even before NTP runs.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 2025-01-01 00:00:00 UTC = 1735689600.
    secs > 1_735_689_600
}

/// Helper: feed a successful response's metadata into the clock-sync
/// machinery. No-op if the response didn't include a vehicle
/// timestamp (e.g. body-controller-state) or if our clock is already
/// within tolerance. Called from every success branch in tick() so
/// any working sub-sample can fix the clock.
fn try_sync_clock(meta: sample::ResponseMeta) {
    if let (Some(vehicle_ts_ms), Some(started)) =
        (meta.vehicle_ts_ms, meta.request_started_at)
    {
        clock_sync::maybe_set_clock_from_vehicle(vehicle_ts_ms, started);
    }
}

fn persist(conn: &Connection, sample: Sample) {
    let ts = sample.ts;
    let source = sample.source.clone();
    if let Err(e) = db::insert(conn, &sample) {
        error!("failed to insert telemetry sample (ts={ts}): {e}");
    } else {
        debug!("inserted telemetry sample (ts={ts}, source={source})");
    }
}

/// Live gate inputs for the BLE card, overwritten each Active tick (not
/// persisted). `unknown` = no value read from the car yet.
const GATE_STATUS_PATH: &str = "/mutable/sentryusb-ble-gate.txt";

fn write_gate_status_file(
    sentry: Option<sample::SentryMode>,
    charging: Option<sample::ChargingState>,
    shift: Option<sample::ShiftState>,
) {
    let sentry_s = sentry
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "unknown".into());
    let charging_s = charging
        .map(|c| format!("{c:?}"))
        .unwrap_or_else(|| "unknown".into());
    // `absent` = drive poll succeeded but shift_state omitted.
    let shift_s = shift
        .map(|s| format!("{s:?}"))
        .unwrap_or_else(|| "absent".into());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let body = format!(
        "sentry_mode={sentry_s}\ncharging_state={charging_s}\nshift_state={shift_s}\nupdated={now}\n"
    );
    let _ = std::fs::write(GATE_STATUS_PATH, body);
}

/// No-op, kept for call-site stability.
///
/// This used to `systemctl stop sentryusb-ble` before each Active poll
/// to claim exclusive hci0, cycling the iOS GATT server every 30-60s.
/// Unnecessary: BLE LE multi-role lets one controller act as central
/// (us → Tesla) and peripheral (sentryusb-ble → iOS app) at once, and
/// all shipped chips support it. The inter-process radio lock still
/// serializes our Rust processes; sentryusb-ble now runs continuously.
/// (The pair flow in api/system.rs still stops it briefly — tesla-control
/// wants exclusive bluez access for the add-key handshake.)
async fn stop_ios_gatt() {
    debug!("stop_ios_gatt: no-op (sentryusb-ble + telemetry coexist via BLE multi-role)");
}

/// Service one IPC action request from `sentryusb-ble-action`: acquire
/// the radio lock if needed, then dispatch through the PersistentSession.
/// Doesn't release the radio afterward — the next tick likely wants it,
/// and thrashing would defeat the point of routing actions through us.
async fn handle_action_request(
    req: action_socket::ActionRequest,
    held_radio: &mut bool,
    ble_session: &mut Option<sample_ble::SessionHandle>,
) {
    let verb = req.verb.clone();
    info!("action_socket: IPC request received — verb={}", verb);

    // Same enable/VIN gate as the rest of the daemon — refuse the action
    // if BLE is off so ble-action can fall back.
    let cfg = match crate::config::BleConfig::load() {
        Ok(c) => c,
        Err(e) => {
            let _ = req.reply.send(Err(anyhow::anyhow!(
                "load BLE config: {e}"
            )));
            return;
        }
    };
    if !cfg.enabled {
        let _ = req.reply.send(Err(anyhow::anyhow!(
            "BLE is disabled in settings"
        )));
        return;
    }
    if cfg.vin.is_empty() {
        let _ = req.reply.send(Err(anyhow::anyhow!(
            "TESLA_BLE_VIN not configured"
        )));
        return;
    }

    // Resolve the verb before any BLE work (saves the radio handoff on
    // a typo). "session-info" is the pairing probe — `None` here, then
    // dispatched to check_pairing() below; every other verb must
    // resolve to a typed ActionPayload.
    let action = if verb == "session-info" {
        None
    } else {
        match action_socket::parse_verb(&verb) {
            Ok(a) => Some(a),
            Err(e) => {
                let _ = req.reply.send(Err(e));
                return;
            }
        }
    };

    // Lazy-spawn or reuse the PersistentSession on the configured
    // VIN/adapter — exactly the same call the tick loop uses.
    if let Err(e) = sample_ble::ensure_session_for(
        ble_session,
        &cfg.vin,
        Some(&cfg.adapter),
    ) {
        let _ = req.reply.send(Err(anyhow::anyhow!(
            "PersistentSession start failed: {e:#}"
        )));
        return;
    }

    // Acquire the radio if not already held (same as the Active tick;
    // not shared because the early-return handling differs).
    if !*held_radio {
        match lock::try_acquire(OWNER) {
            Ok(true) => {
                *held_radio = true;
                stop_ios_gatt().await;
            }
            Ok(false) => {
                let _ = req.reply.send(Err(anyhow::anyhow!(
                    "radio held by {:?} — cannot service action right now",
                    lock::current_owner()
                )));
                return;
            }
            Err(e) => {
                let _ = req.reply.send(Err(anyhow::anyhow!(
                    "could not acquire radio lock: {e}"
                )));
                return;
            }
        }
    }

    let session = &ble_session
        .as_ref()
        .expect("ensure_session_for left session populated")
        .session;

    let started = Instant::now();
    let result = match action {
        // Pairing probe: reuse this session's held connection. Map the
        // tri-state onto the line protocol's OK/ERR contract —
        // Paired => "OK", NotPaired => "ERR NOT_PAIRED", Unreachable =>
        // "ERR UNREACHABLE: …". sentryusb-ble-action parses these tokens
        // and the API clears the paired marker only on NOT_PAIRED.
        None => {
            use sentryusb_tesla_ble::manager::PairingStatus;
            let status = session.check_pairing().await;
            info!(
                "action_socket: verb=session-info -> {:?} ({}ms)",
                status,
                started.elapsed().as_millis()
            );
            match status {
                PairingStatus::Paired => Ok(()),
                PairingStatus::NotPaired => Err(anyhow::anyhow!("NOT_PAIRED")),
                PairingStatus::Unreachable(reason) => {
                    Err(anyhow::anyhow!("UNREACHABLE: {reason}"))
                }
            }
        }
        Some(action) => {
            let result = session.send_action(action).await;
            let elapsed_ms = started.elapsed().as_millis();
            match &result {
                Ok(bytes) => info!(
                    "action_socket: verb={} ok ({}ms, {} bytes decrypted response)",
                    verb,
                    elapsed_ms,
                    bytes.len()
                ),
                Err(e) => warn!(
                    "action_socket: verb={} failed after {}ms: {:#}",
                    verb, elapsed_ms, e
                ),
            }
            result.map(|_| ())
        }
    };
    let _ = req.reply.send(result);
}

/// Release our radio-lock entry. Called on radio-release transitions
/// and SIGTERM.
async fn release_radio() {
    // Just release the lock — the sync semantic between our Rust
    // processes (telemetry, ble-action, pair). sentryusb-ble doesn't
    // check it, and there's nothing to restart now that stop_ios_gatt
    // is a no-op and sentryusb-ble runs continuously.
    if let Err(e) = lock::release(OWNER) {
        warn!("failed to release radio lock: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `should_enter_quiet` is the gate's sleep decision. Quiet = "let the
    // car sleep"; permitted only when the car is parked or asleep AND
    // nothing has a reason to hold it awake. Args, in order:
    // (car_truly_asleep, parked_confirmed, actively_charging, sentry_on,
    //  keep_awake_active).

    #[test]
    fn parked_idle_car_is_allowed_to_sleep() {
        // Parked, sentry off, not charging, nothing running → may sleep.
        // The behavior we preserve for a genuinely idle parked car.
        assert!(should_enter_quiet(false, true, false, false, false));
    }

    #[test]
    fn keep_awake_pins_active_while_parked() {
        // The regression (commit fba51ce): parked + sentry off + not
        // charging would quiet, but an archive / keep-awake nudge is
        // running, so the car must NOT be allowed to sleep mid-archive.
        assert!(!should_enter_quiet(false, true, false, false, true));
    }

    #[test]
    fn keep_awake_pins_active_even_when_car_looks_asleep() {
        // A long archive disconnects the USB gadget, so cam_disk.bin stops
        // updating and the car trips "truly asleep" after 5 min even though
        // it's awake. The override must cover this second path too — hence
        // it sits on the whole decision, not just the parked-polls counter.
        assert!(should_enter_quiet(true, false, false, false, false));
        assert!(!should_enter_quiet(true, false, false, false, true));
    }

    #[test]
    fn charging_or_sentry_still_pins_active() {
        assert!(!should_enter_quiet(false, true, true, false, false)); // charging
        assert!(!should_enter_quiet(false, true, false, true, false)); // sentry on
    }

    #[test]
    fn moving_car_never_quiets() {
        // Not parked-confirmed and not asleep (e.g. driving).
        assert!(!should_enter_quiet(false, false, false, false, false));
    }
}
