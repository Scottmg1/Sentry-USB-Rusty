//! Charging history derived on-demand from `telemetry_samples`.
//!
//! Charge sessions are not a stored table — they are grouped at query
//! time from the per-sample charge columns the experimental sampler
//! writes (`charger_power_kw`, `charge_rate_mph`, ...). A session is a
//! contiguous run of actively-charging samples; a gap longer than
//! `SESSION_GAP_SECS` starts a new one. Energy reported by the car is
//! cumulative within a plug-in and resets to zero on unplug, so the
//! per-session total is the span between the first and last reading.
//!
//! When the experimental flag is off the charge columns are NULL for
//! every row, so the grouping yields nothing and both endpoints return
//! empty results. The flag is also checked up front so a normal install
//! does no query work and surfaces no charging UI.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Serialize;

use crate::router::AppState;

/// A gap larger than this between consecutive charging samples ends the
/// session. The sampler polls charge state well inside this window while
/// a car is plugged in; 30 minutes tolerates a missed poll or two
/// without merging two genuinely separate plug-ins.
const SESSION_GAP_SECS: i64 = 30 * 60;

/// One row pulled from `telemetry_samples`, already filtered to samples
/// where the car was actively charging (see `is_actively_charging`).
struct ChargeRow {
    ts: i64,
    power_kw: Option<i64>,
    current_a: Option<i64>,
    voltage_v: Option<i64>,
    rate_mph: Option<f64>,
    energy_added_kwh: Option<f64>,
    limit_soc: Option<i64>,
    range_mi: Option<f64>,
    battery_pct: Option<f64>,
    battery_temp_c: Option<f64>,
    interior_temp_c: Option<f64>,
    exterior_temp_c: Option<f64>,
    location: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    /// Persisted Tesla charge phase (v14+, lowercase). `None` on pre-v14
    /// rows or when the sampler couldn't decode it that tick. When
    /// present, this is the authoritative signal — see
    /// `is_actively_charging`.
    charging_state: Option<String>,
}

/// Summary of one charge session for the list view.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargeSessionSummary {
    /// Session id == start timestamp in unix seconds. Stable and
    /// directly usable as the detail-endpoint key.
    id: i64,
    start_ms: i64,
    end_ms: i64,
    duration_secs: i64,
    location: Option<String>,
    location_lat: Option<f64>,
    location_lon: Option<f64>,
    energy_added_kwh: Option<f64>,
    peak_power_kw: Option<i64>,
    start_soc: Option<f64>,
    end_soc: Option<f64>,
    start_range_mi: Option<f64>,
    end_range_mi: Option<f64>,
    charge_limit_soc: Option<i64>,
}

/// One point on the detail charts. Carries every per-sample series the
/// charging view plots — all sourced from columns the sampler already
/// records, so adding them costs nothing extra on the device.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargePoint {
    ts: i64,
    power_kw: Option<i64>,
    current_a: Option<i64>,
    voltage_v: Option<i64>,
    rate_mph: Option<f64>,
    soc: Option<f64>,
    range_mi: Option<f64>,
    energy_added_kwh: Option<f64>,
    battery_temp_c: Option<f64>,
    interior_temp_c: Option<f64>,
    exterior_temp_c: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargeSessionDetail {
    #[serde(flatten)]
    summary: ChargeSessionSummary,
    avg_power_kw: Option<f64>,
    peak_current_a: Option<i64>,
    avg_current_a: Option<f64>,
    peak_voltage_v: Option<i64>,
    avg_voltage_v: Option<f64>,
    peak_rate_mph: Option<f64>,
    avg_battery_temp_c: Option<f64>,
    points: Vec<ChargePoint>,
}

/// Mean of an iterator of values, or None when it yields nothing. Used
/// for the detail view's average power / current / voltage / temp stats.
fn avg(it: impl Iterator<Item = f64>) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0u32;
    for v in it {
        sum += v;
        n += 1;
    }
    if n == 0 { None } else { Some(sum / n as f64) }
}

/// Heuristic "is this row charging?" for pre-v14 rows that don't carry a
/// persisted Tesla phase. `rate_mph` is the authoritative truth when the
/// car reports it: nonzero rate = energy is going into the battery; an
/// explicit `Some(0.0)` rate means "not charging" even when `power_kw`
/// is positive (cabin pre-conditioning, 12V top-up, and BMS thermal
/// management all draw power without charging). Only when rate is
/// missing entirely do we fall back to power — that covers the rare
/// decode failure where the car is genuinely charging but the rate field
/// didn't come through.
///
/// For v14+ rows callers should use `is_actively_charging`, which uses
/// the persisted phase directly and falls back to this heuristic only
/// when phase is `None`.
fn is_charging(power_kw: Option<i64>, rate_mph: Option<f64>) -> bool {
    match rate_mph {
        Some(r) => r > 0.0,
        None => power_kw.is_some_and(|p| p > 0),
    }
}

/// Phase-first "is this row charging?" — the predicate `load_charge_rows`
/// uses to decide whether a sample belongs in a charge session. When the
/// row carries a persisted Tesla phase (v14+) it's authoritative:
/// `charging`/`starting`/`calibrating` → yes, everything else → no.
/// Pre-v14 rows fall back to `is_charging` over power and rate.
///
/// Why both layers: phase alone misses the entire pre-v14 fleet; the
/// heuristic alone produces phantom sessions when the car wakes from
/// sleep plugged in but full (it draws a couple of kW for cabin
/// pre-conditioning, the old heuristic saw `power_kw > 0` and counted
/// it as a charge session that added zero kWh).
fn is_actively_charging(
    phase: Option<&str>,
    power_kw: Option<i64>,
    rate_mph: Option<f64>,
) -> bool {
    match phase_is_active(phase) {
        Some(active) => active,
        None => is_charging(power_kw, rate_mph),
    }
}

/// How stale the latest charge row may be before the banner gives up
/// entirely. Generous (24h) because the only case that can leave a
/// "charging" phase on the newest row is a charge that ended while BLE
/// was fully down (so no stopped/complete poll ever landed) — this is
/// the self-healing backstop for that.
const CHARGE_STALE_SECS: i64 = 86_400;

/// True/false if the persisted Tesla charge phase is an actively-charging
/// one. `None` when there's no phase string (pre-v14 rows) so the caller
/// can fall back to the old power/rate heuristic. The spellings mirror
/// `ChargingState::as_db_str` in the telemetry crate (the api crate can't
/// depend on that binary crate, so this is a deliberate string contract).
fn phase_is_active(phase: Option<&str>) -> Option<bool> {
    phase.map(|p| matches!(p, "charging" | "starting" | "calibrating"))
}

/// Pull charging samples in `[from, to]` ordered by time. `to` of
/// `None` means "no upper bound".
fn load_charge_rows(
    conn: &rusqlite::Connection,
    from: i64,
    to: Option<i64>,
) -> anyhow::Result<Vec<ChargeRow>> {
    let upper = to.unwrap_or(i64::MAX);
    // SQL pulls every row with any charge-related signal (phase OR power
    // OR rate non-NULL); the Rust filter below decides whether each one
    // is actually charging via `is_actively_charging`. This split keeps
    // the SQL simple and the predicate unit-testable.
    let mut stmt = conn.prepare(
        "SELECT ts, charger_power_kw, charger_actual_current_a, charger_voltage_v, \
                charge_rate_mph, charge_energy_added_kwh, charge_limit_soc, \
                battery_range_mi, battery_pct, \
                battery_temp_c, interior_temp_c, exterior_temp_c, location_name, \
                latitude, longitude, charging_state \
         FROM telemetry_samples \
         WHERE ts BETWEEN ?1 AND ?2 \
           AND (charging_state IS NOT NULL \
                OR charger_power_kw IS NOT NULL \
                OR charge_rate_mph IS NOT NULL) \
         ORDER BY ts ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![from, upper], |r| {
        Ok(ChargeRow {
            ts: r.get(0)?,
            power_kw: r.get(1)?,
            current_a: r.get(2)?,
            voltage_v: r.get(3)?,
            rate_mph: r.get(4)?,
            energy_added_kwh: r.get(5)?,
            limit_soc: r.get(6)?,
            range_mi: r.get(7)?,
            battery_pct: r.get(8)?,
            battery_temp_c: r.get(9)?,
            interior_temp_c: r.get(10)?,
            exterior_temp_c: r.get(11)?,
            location: r.get(12)?,
            lat: r.get(13)?,
            lon: r.get(14)?,
            charging_state: r.get(15)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        let row = row?;
        if is_actively_charging(row.charging_state.as_deref(), row.power_kw, row.rate_mph) {
            out.push(row);
        }
    }
    Ok(out)
}

/// Split time-ordered, already-filtered charging rows into sessions on
/// the gap threshold. Each inner Vec is one session, in time order.
fn group_sessions(rows: Vec<ChargeRow>) -> Vec<Vec<ChargeRow>> {
    let mut sessions: Vec<Vec<ChargeRow>> = Vec::new();
    for row in rows {
        match sessions.last_mut() {
            Some(cur) if row.ts - cur.last().unwrap().ts <= SESSION_GAP_SECS => cur.push(row),
            _ => sessions.push(vec![row]),
        }
    }
    sessions
}

/// Reduce one session's rows to a summary. `rows` is non-empty and
/// time-ordered.
fn summarize(rows: &[ChargeRow]) -> ChargeSessionSummary {
    let first = &rows[0];
    let last = &rows[rows.len() - 1];

    // Energy is cumulative within a plug-in; the span between the first
    // and last reading is what this session added. Clamp at zero so a
    // mid-session counter reset can't produce a negative.
    let energy_added_kwh = match (first.energy_added_kwh, last.energy_added_kwh) {
        (Some(a), Some(b)) => Some((b - a).max(0.0)),
        (None, Some(b)) => Some(b),
        _ => None,
    };

    ChargeSessionSummary {
        id: first.ts,
        start_ms: first.ts * 1000,
        end_ms: last.ts * 1000,
        duration_secs: last.ts - first.ts,
        location: rows.iter().find_map(|r| r.location.clone()),
        location_lat: rows.iter().find_map(|r| r.lat),
        location_lon: rows.iter().find_map(|r| r.lon),
        energy_added_kwh,
        peak_power_kw: rows.iter().filter_map(|r| r.power_kw).max(),
        start_soc: rows.iter().find_map(|r| r.battery_pct),
        end_soc: rows.iter().rev().find_map(|r| r.battery_pct),
        start_range_mi: rows.iter().find_map(|r| r.range_mi),
        end_range_mi: rows.iter().rev().find_map(|r| r.range_mi),
        charge_limit_soc: rows.iter().rev().find_map(|r| r.limit_soc),
    }
}

/// GET /api/charging
///
/// Charge sessions newest-first. Empty when no charging has been sampled.
pub async fn list_charging(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = state
        .drives
        .store
        .with_locked_conn(|conn| load_charge_rows(conn, 0, None));

    match result {
        Ok(rows) => {
            let mut sessions: Vec<ChargeSessionSummary> =
                group_sessions(rows).iter().map(|s| summarize(s)).collect();
            sessions.sort_by(|a, b| b.id.cmp(&a.id));
            (
                StatusCode::OK,
                Json(serde_json::json!({ "sessions": sessions })),
            )
        }
        Err(e) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

/// GET /api/charging/{id}
///
/// Detail for the session whose start timestamp is `id`, including the
/// per-sample series for the power / SoC charts. Rows are re-grouped
/// from `id` forward and the first session returned, so the endpoint is
/// stateless and needs no stored session table.
pub async fn single_charging(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Bound the scan so a session that never closes can't read the whole
    // table. One plug-in can't plausibly exceed this; the gap split ends
    // the session well before the bound in practice.
    let window_end = id + 7 * 24 * 60 * 60;
    let result = state
        .drives
        .store
        .with_locked_conn(|conn| load_charge_rows(conn, id, Some(window_end)));

    let rows = match result {
        Ok(rows) => rows,
        Err(e) => return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };

    let session = match group_sessions(rows).into_iter().next() {
        Some(s) => s,
        None => return crate::json_error(StatusCode::NOT_FOUND, "charge session not found"),
    };

    let summary = summarize(&session);
    let points: Vec<ChargePoint> = session
        .iter()
        .map(|r| ChargePoint {
            ts: r.ts * 1000,
            power_kw: r.power_kw,
            current_a: r.current_a,
            voltage_v: r.voltage_v,
            rate_mph: r.rate_mph,
            soc: r.battery_pct,
            range_mi: r.range_mi,
            energy_added_kwh: r.energy_added_kwh,
            battery_temp_c: r.battery_temp_c,
            interior_temp_c: r.interior_temp_c,
            exterior_temp_c: r.exterior_temp_c,
        })
        .collect();

    let detail = ChargeSessionDetail {
        avg_power_kw: avg(session.iter().filter_map(|r| r.power_kw.map(|v| v as f64))),
        peak_current_a: session.iter().filter_map(|r| r.current_a).max(),
        avg_current_a: avg(session.iter().filter_map(|r| r.current_a.map(|v| v as f64))),
        peak_voltage_v: session.iter().filter_map(|r| r.voltage_v).max(),
        avg_voltage_v: avg(session.iter().filter_map(|r| r.voltage_v.map(|v| v as f64))),
        peak_rate_mph: session
            .iter()
            .filter_map(|r| r.rate_mph)
            .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v)))),
        avg_battery_temp_c: avg(session.iter().filter_map(|r| r.battery_temp_c)),
        summary,
        points,
    };

    (StatusCode::OK, Json(serde_json::to_value(detail).unwrap()))
}

/// Live charge status for the dashboard banner. `charging` is false when
/// the latest sample isn't an active charge or is stale (the car stopped
/// being sampled); the other fields are present only while charging.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentCharge {
    charging: bool,
    soc: Option<f64>,
    limit_soc: Option<i64>,
    power_kw: Option<i64>,
    minutes_to_full: Option<i64>,
    range_mi: Option<f64>,
}

impl CurrentCharge {
    fn idle() -> Self {
        Self {
            charging: false,
            soc: None,
            limit_soc: None,
            power_kw: None,
            minutes_to_full: None,
            range_mi: None,
        }
    }
}

/// The single most-recent telemetry row, charge-relevant columns only.
struct LatestCharge {
    ts: i64,
    soc: Option<f64>,
    limit_soc: Option<i64>,
    power_kw: Option<i64>,
    rate_mph: Option<f64>,
    minutes_to_full: Option<i64>,
    range_mi: Option<f64>,
    charging_state: Option<String>,
}

/// GET /api/charging/current — is the car charging right now, with the
/// fields the dashboard banner shows.
///
/// Reads the most-recent *charge-bearing* row (one that carries a charge
/// phase or charger power/rate — also the only rows that carry battery %).
/// The charging decision is phase-first: while the persisted Tesla phase
/// is charging/starting/calibrating the banner stays up for the whole
/// charge regardless of how stale the sample is (the BLE sampler can go
/// minutes between polls mid-charge), and only drops when a poll actually
/// reports a stopped/complete phase. Pre-v14 rows (no phase) fall back to
/// the old "fresh within 10 min AND nonzero power/rate" heuristic.
pub async fn current_charging(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    use rusqlite::OptionalExtension;
    let latest = state.drives.store.with_locked_conn(|conn| {
        conn.query_row(
            "SELECT ts, battery_pct, charge_limit_soc, charger_power_kw, \
                    charge_rate_mph, charge_minutes_to_full, battery_range_mi, \
                    charging_state \
             FROM telemetry_samples \
             WHERE charging_state IS NOT NULL \
                OR charger_power_kw IS NOT NULL \
                OR charge_rate_mph IS NOT NULL \
             ORDER BY ts DESC LIMIT 1",
            [],
            |r| {
                Ok(LatestCharge {
                    ts: r.get(0)?,
                    soc: r.get(1)?,
                    limit_soc: r.get(2)?,
                    power_kw: r.get(3)?,
                    rate_mph: r.get(4)?,
                    minutes_to_full: r.get(5)?,
                    range_mi: r.get(6)?,
                    charging_state: r.get(7)?,
                })
            },
        )
        .optional()
    });

    let cur = match latest {
        Ok(Some(l)) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(l.ts);
            let age = now - l.ts;
            let charging = match phase_is_active(l.charging_state.as_deref()) {
                // Phase says actively charging — hold the banner the whole
                // charge; only the 24h backstop can drop it.
                Some(true) => age <= CHARGE_STALE_SECS,
                // Phase says stopped/complete/disconnected — done, no banner.
                Some(false) => false,
                // Pre-v14 row with no phase — old heuristic.
                None => age <= 600 && is_charging(l.power_kw, l.rate_mph),
            };
            // Battery % is shown for the persistent car-status banner as long
            // as the data is reasonably fresh (<= 24h), so the banner doesn't
            // vanish the moment a charge ends. The charging-only fields are
            // present only while actively charging.
            let soc = if age <= CHARGE_STALE_SECS { l.soc } else { None };
            CurrentCharge {
                charging,
                soc,
                limit_soc: if charging { l.limit_soc } else { None },
                power_kw: if charging { l.power_kw } else { None },
                minutes_to_full: if charging { l.minutes_to_full } else { None },
                range_mi: if charging { l.range_mi } else { l.range_mi.filter(|_| soc.is_some()) },
            }
        }
        _ => CurrentCharge::idle(),
    };
    (StatusCode::OK, Json(serde_json::to_value(cur).unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ts: i64, power: Option<i64>, rate: Option<f64>, energy: Option<f64>) -> ChargeRow {
        ChargeRow {
            ts,
            power_kw: power,
            current_a: None,
            voltage_v: None,
            rate_mph: rate,
            energy_added_kwh: energy,
            limit_soc: None,
            range_mi: None,
            battery_pct: None,
            battery_temp_c: None,
            interior_temp_c: None,
            exterior_temp_c: None,
            location: None,
            lat: None,
            lon: None,
            charging_state: None,
        }
    }

    #[test]
    fn gap_splits_into_two_sessions() {
        let rows = vec![
            row(1_000, Some(7), Some(25.0), Some(0.0)),
            row(1_300, Some(7), Some(25.0), Some(1.0)),
            // > 30 min later — new session
            row(1_300 + SESSION_GAP_SECS + 1, Some(11), Some(40.0), Some(0.0)),
        ];
        let sessions = group_sessions(rows);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].len(), 2);
        assert_eq!(sessions[1].len(), 1);
    }

    #[test]
    fn energy_total_is_first_to_last_span() {
        let rows = vec![
            row(1_000, Some(7), Some(25.0), Some(2.0)),
            row(1_300, Some(7), Some(25.0), Some(9.4)),
        ];
        let s = summarize(&rows);
        assert_eq!(s.energy_added_kwh, Some(7.4));
        assert_eq!(s.peak_power_kw, Some(7));
        assert_eq!(s.duration_secs, 300);
        assert_eq!(s.id, 1_000);
    }

    #[test]
    fn non_charging_rows_excluded_by_is_charging() {
        assert!(!is_charging(Some(0), Some(0.0)));
        assert!(!is_charging(None, None));
        assert!(
            is_charging(Some(7), None),
            "no rate signal at all — trust nonzero power as a real-charge proxy",
        );
        assert!(is_charging(None, Some(12.0)));
    }

    #[test]
    fn rate_zero_overrides_nonzero_power() {
        // Regression for the phantom-session bug. On-vehicle, the user
        // woke the car with the cabin AC remote-start on. Car was at
        // 79% with an 80% limit so it wasn't charging, but BMS routed
        // 2 kW to climate. Tesla reported power_kw=2, rate_mph=0.0,
        // energy_added_kwh=17.48 (carried over from the prior charge).
        // The old `power > 0 || rate > 0` predicate said "charging" on
        // the strength of the 2 kW alone, the row entered a "session",
        // a phantom session appeared in the UI with 0 kWh added.
        //
        // The fix: when rate is reported, trust it. An explicit zero
        // rate means no energy is going to the battery, regardless of
        // power draw elsewhere in the car.
        assert!(
            !is_charging(Some(2), Some(0.0)),
            "rate=0 explicitly reported → not charging, even with nonzero power \
             (cabin AC / BMS thermal / 12V top-up all draw power without charging)",
        );
        assert!(
            !is_charging(Some(4), Some(0.0)),
            "and the larger AC-startup draw at wake-from-sleep is not charging either",
        );
    }

    // ── Phase-first session predicate ──────────────────────────────────
    //
    // `is_actively_charging` is what `load_charge_rows` actually uses to
    // decide whether a sample belongs in a charge session. When the row
    // has a persisted Tesla phase (v14+, written by the sampler) the
    // phase is authoritative; pre-v14 rows fall back to `is_charging`.
    // These tests pin both layers.

    #[test]
    fn phase_charging_is_included_even_with_weak_signals() {
        // Tesla says "charging"; trust the phase even if power_kw is
        // reported as 0 (mid-handshake) or rate_mph as None (decode glitch).
        assert!(is_actively_charging(Some("charging"), Some(0), Some(0.0)));
        assert!(is_actively_charging(Some("charging"), None, None));
        assert!(is_actively_charging(Some("starting"), Some(1), None));
        assert!(is_actively_charging(Some("calibrating"), None, Some(0.0)));
    }

    #[test]
    fn phase_complete_excludes_phantom_power_draw() {
        // The on-vehicle scenario again, but now with the v14 phase
        // present. The phase says "complete" (charge limit reached);
        // any power draw at this point is climate / 12V / BMS, NOT
        // charging. Trust the phase, ignore the nonzero power.
        assert!(!is_actively_charging(Some("complete"), Some(2), Some(0.0)));
        assert!(!is_actively_charging(Some("stopped"), Some(4), Some(0.0)));
        assert!(!is_actively_charging(Some("disconnected"), None, None));
        assert!(!is_actively_charging(Some("nopower"), Some(0), Some(0.0)));
        assert!(
            !is_actively_charging(Some("unknown"), Some(7), Some(20.0)),
            "Tesla explicitly said 'unknown'; be conservative — would rather \
             miss a row than create a phantom session",
        );
    }

    #[test]
    fn no_phase_falls_back_to_heuristic() {
        // Pre-v14 row (or v14 row where the sampler couldn't decode the
        // phase that tick): no `charging_state` value persisted. Defer
        // to `is_charging`, which itself prefers rate over power.
        assert!(is_actively_charging(None, Some(4), Some(20.0)));
        assert!(is_actively_charging(None, Some(7), None));
        assert!(!is_actively_charging(None, Some(2), Some(0.0))); // phantom
        assert!(!is_actively_charging(None, None, None));
        assert!(!is_actively_charging(None, Some(0), Some(0.0)));
    }

    #[test]
    fn structs_serialize_camelcase_for_the_web_client() {
        // Regression for the on-vehicle bug: the web UI reads camelCase
        // keys (startMs, energyAddedKwh, powerKw, ...). Without
        // #[serde(rename_all = "camelCase")] the structs emit snake_case,
        // so EVERY field arrives `undefined` → "Invalid Date", NaN
        // duration, "—" stats, 0.0 energy. Pin the wire names here.
        let s = summarize(&[
            row(1_000, Some(7), Some(25.0), Some(2.0)),
            row(1_300, Some(11), Some(40.0), Some(9.4)),
        ]);
        let j = serde_json::to_string(&s).unwrap();
        for key in ["startMs", "endMs", "durationSecs", "energyAddedKwh", "peakPowerKw"] {
            assert!(j.contains(&format!("\"{key}\"")), "summary must emit {key}: {j}");
        }
        assert!(!j.contains("\"start_ms\""), "summary must NOT emit snake_case: {j}");

        // Obviously-synthetic placeholder values — the test asserts only the
        // serialized KEY NAMES (camelCase), never these numbers.
        let p = ChargePoint {
            ts: 1,
            power_kw: Some(1),
            current_a: Some(1),
            voltage_v: Some(1),
            rate_mph: Some(1.0),
            soc: Some(1.0),
            range_mi: Some(1.0),
            energy_added_kwh: Some(1.0),
            battery_temp_c: None,
            interior_temp_c: None,
            exterior_temp_c: None,
        };
        let jp = serde_json::to_string(&p).unwrap();
        for key in ["powerKw", "currentA", "voltageV", "rateMph", "rangeMi", "energyAddedKwh"] {
            assert!(jp.contains(&format!("\"{key}\"")), "point must emit {key}: {jp}");
        }
        assert!(!jp.contains("\"power_kw\""), "point must NOT emit snake_case: {jp}");
    }
}
