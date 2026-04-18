//! Setup event emitter — replaces the raw `Fn(&str)` progress callback
//! with a richer interface that distinguishes between:
//!
//! * `progress(msg)` — free-form log lines, streamed to the log file and
//!   broadcast as `setup_progress` WebSocket events.
//! * `begin_phase(id, label)` — announces that a new user-visible phase has
//!   started doing actual work. Phases that no-op (e.g. an already-configured
//!   dwc2 overlay on re-run) never announce themselves, so the UI only lists
//!   phases that are actually being executed this run.
//!
//! A phase callback additionally persists the phase to
//! `/sentryusb/setup-phases.jsonl` so the UI can reconstruct the list across
//! a reboot-triggered WebSocket disconnect.
//!
//! The split keeps the runner's top-level "what big thing am I doing now"
//! signal separate from the noisy per-line detail log.

use std::sync::Arc;

#[derive(Clone)]
pub struct SetupEmitter {
    progress: Arc<dyn Fn(&str) + Send + Sync>,
    phase: Arc<dyn Fn(&str, &str) + Send + Sync>,
}

impl SetupEmitter {
    pub fn new(
        progress: impl Fn(&str) + Send + Sync + 'static,
        phase: impl Fn(&str, &str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            progress: Arc::new(progress),
            phase: Arc::new(phase),
        }
    }

    /// Log a free-form message (writes to log file + broadcasts to WS).
    pub fn progress(&self, msg: &str) {
        (self.progress)(msg);
    }

    /// Announce the start of a user-visible phase.
    ///
    /// Call this only when the phase is actually going to do work — it
    /// drives the wizard's live phase list. `id` must be stable across
    /// reboots for the UI to deduplicate, `label` is the human-readable
    /// string shown to the user.
    pub fn begin_phase(&self, id: &str, label: &str) {
        (self.phase)(id, label);
    }
}
