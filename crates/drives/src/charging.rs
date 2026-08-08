//! Charge-session derivation from `telemetry_samples` — shared core.
//!
//! Shared between the local /api/charging endpoints and the cloud
//! uploader, which must derive the SAME sessions. Session identity
//! (`id` == start ts) and grouping boundaries have to match exactly
//! between the two consumers — a drifted copy would break cloud tag
//! sync (the cloud chargeId is derived from the start timestamp).
//!
//! Rate/cost application stays in the api crate (`apply_rates` /
//! `apply_cost_override`) — costs are derived at render time on every
//! surface and never baked into stored/uploaded session data.

use serde::Serialize;

/// A gap larger than this between consecutive charging samples ends the
/// session. The sampler polls charge state well inside this window while
/// a car is plugged in; 30 minutes tolerates a missed poll or two
/// without merging two genuinely separate plug-ins.
pub const SESSION_GAP_SECS: i64 = 30 * 60;

/// Peak charger power (kW) above which a session counts as **fast
/// charging** — DC fast charging (Supercharger, CCS). Set just above the
/// AC Level 2 ceiling (19.2 kW in North America, 22 kW in Europe) with a
/// strict `>`, so no home/destination AC charge ever trips it — including
/// a 22 kW European 3-phase wallbox — while every DC charge (50 kW+) does.
/// One threshold covers both regions, no locale setting needed.
///
/// The telemetry crate (`tesla_telemetry::main`) keeps its own copy to
/// drive the adaptive poll cadence; it can't depend on this crate's
/// consumers, so the two are a deliberate value contract. Keep in sync.
pub const FAST_CHARGE_THRESHOLD_KW: i64 = 22;

/// One row pulled from `telemetry_samples`, already filtered to samples
/// where the car was actively charging (see `is_actively_charging`).
pub struct ChargeRow {
    pub ts: i64,
    pub power_kw: Option<i64>,
    pub current_a: Option<i64>,
    pub voltage_v: Option<i64>,
    pub rate_mph: Option<f64>,
    pub energy_added_kwh: Option<f64>,
    pub limit_soc: Option<i64>,
    pub range_mi: Option<f64>,
    pub battery_pct: Option<f64>,
    pub location: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// Persisted Tesla charge phase (v14+, lowercase). `None` on pre-v14
    /// rows or when the sampler couldn't decode it that tick. When
    /// present, this is the authoritative signal — see
    /// `is_actively_charging`.
    pub charging_state: Option<String>,
}

/// Summary of one charge session for the list view. Also the plaintext
/// of the encrypted summary uploaded to Sentry Cloud — the web client
/// deserializes this exact camelCase shape, so field changes are a
/// cross-repo wire change.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChargeSessionSummary {
    /// Session id == start timestamp in unix seconds. Stable and
    /// directly usable as the detail-endpoint key.
    pub id: i64,
    pub start_ms: i64,
    pub end_ms: i64,
    pub duration_secs: i64,
    pub location: Option<String>,
    pub location_lat: Option<f64>,
    pub location_lon: Option<f64>,
    pub energy_added_kwh: Option<f64>,
    /// Energy drawn from the charger (wall-side), kWh. Trapezoidal
    /// integral of per-sample charger power, each sample refined from
    /// volts × amps when those agree with the car's coarse integer kW
    /// (see `sample_power_kw`). Normally >= `energy_added_kwh` (the gap is
    /// charging loss); on coarse data it can dip to/under it, which is why
    /// `efficiency_pct` is clamped.
    pub energy_used_kwh: Option<f64>,
    /// Charging efficiency, percent = added / used, clamped to [0, 100].
    pub efficiency_pct: Option<f64>,
    pub peak_power_kw: Option<i64>,
    pub start_soc: Option<f64>,
    pub end_soc: Option<f64>,
    pub start_range_mi: Option<f64>,
    pub end_range_mi: Option<f64>,
    pub charge_limit_soc: Option<i64>,
    /// User-assigned tags + the cost derived from them. Filled per-session
    /// by the api crate's `apply_rates`; empty/None until then. Stays
    /// empty in the cloud summary blob — tags/cost live in the mutable
    /// envelope + rate config there.
    pub tags: Vec<String>,
    pub cost: Option<f64>,
    /// Resolved price-per-kWh used for `cost` (for UI transparency).
    pub rate: Option<f64>,
    /// Currency symbol for `cost` (from prefs, default "$").
    pub currency: String,
    /// Peak power exceeded `FAST_CHARGE_THRESHOLD_KW` — i.e. DC fast
    /// charging. Drives the web "Fast charging" badge and unlocks the
    /// manual per-charge cost (which is offered on fast charges only).
    pub fast_charging: bool,
    /// `cost` came from a user-entered per-charge override rather than the
    /// tag/rate engine. Lets the UI show the cost as manually set and skip
    /// the "set a rate" hint.
    pub cost_overridden: bool,
    /// Charged inside the configured home geofence. Set by the api crate;
    /// `false` in the cloud blob. Drives the auto "Home" chip + Home rate.
    pub at_home: bool,
}

/// One point on the detail charts. Carries every per-sample series the
/// charging view plots — all sourced from columns the sampler already
/// records, so adding them costs nothing extra on the device.
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct ChargePoint {
    pub ts: i64,
    pub power_kw: Option<i64>,
    pub current_a: Option<i64>,
    pub voltage_v: Option<i64>,
    pub rate_mph: Option<f64>,
    pub soc: Option<f64>,
    pub range_mi: Option<f64>,
    pub energy_added_kwh: Option<f64>,
}

/// Mean of an iterator of values, or None when it yields nothing.
pub fn avg(it: impl Iterator<Item = f64>) -> Option<f64> {
    let mut sum = 0.0;
    let mut n = 0u32;
    for v in it {
        sum += v;
        n += 1;
    }
    if n == 0 { None } else { Some(sum / n as f64) }
}

/// Largest gap (kW) tolerated between volts × amps and the car's own
/// integer `charger_power` before we distrust the fine-grained product.
/// Integer rounding alone is at most ±0.5 kW; 1.0 kW leaves room for
/// sensor noise while staying well below the ~2 kW-plus gap that 3-phase
/// charging always produces (see `sample_power_kw`).
const POWER_REFINE_TOLERANCE_KW: f64 = 1.0;

/// Best per-sample charger power in kW for energy integration.
///
/// Tesla reports `charger_power` only as whole kilowatts — ruinous on a
/// slow Level 1 charge (121 V × 12 A = 1.45 kW arrives as `1`). When the
/// sample also carries voltage and current we recover the fractional kW
/// from V × I, but only when it agrees with the integer power to within
/// `POWER_REFINE_TOLERANCE_KW` (a European 3-phase per-phase product
/// would be ~1/3 of reality and is rejected by the tolerance check).
pub fn sample_power_kw(r: &ChargeRow) -> Option<f64> {
    let coarse = r.power_kw? as f64;
    match (r.voltage_v, r.current_a) {
        (Some(v), Some(a)) => {
            let fine = v as f64 * a as f64 / 1000.0;
            if (fine - coarse).abs() <= POWER_REFINE_TOLERANCE_KW {
                Some(fine)
            } else {
                Some(coarse)
            }
        }
        _ => Some(coarse),
    }
}

/// Charger current in amps for display. During DC fast charging the car
/// reports a literal `0 A` (the onboard AC charger is bypassed); derive
/// P÷V in that case, and trust any real nonzero AC measurement.
pub fn display_current_a(
    power_kw: Option<i64>,
    voltage_v: Option<i64>,
    raw: Option<i64>,
) -> Option<i64> {
    match raw {
        // Real AC measurement — trust it.
        Some(a) if a > 0 => Some(a),
        // Reported 0 (DC) or missing: derive P÷V when both are present.
        _ => match (power_kw, voltage_v) {
            (Some(p), Some(v)) if v > 0 && p > 0 => {
                Some((p as f64 * 1000.0 / v as f64).round() as i64)
            }
            _ => raw,
        },
    }
}

/// Trapezoidal integral of charger power (kW) over a session's samples,
/// in kWh. `None` with fewer than two power readings. In-session samples
/// are <= `SESSION_GAP_SECS` apart by construction, so no gap guard is
/// needed beyond skipping non-positive dt. Per-sample power is the
/// V × I-refined estimate from `sample_power_kw`, not the raw integer kW.
pub fn integrate_power_kwh(rows: &[ChargeRow]) -> Option<f64> {
    let pts: Vec<(i64, f64)> = rows
        .iter()
        .filter_map(|r| sample_power_kw(r).map(|p| (r.ts, p)))
        .collect();
    if pts.len() < 2 {
        return None;
    }
    let mut kwh = 0.0;
    for w in pts.windows(2) {
        let dt_h = (w[1].0 - w[0].0) as f64 / 3600.0;
        if dt_h > 0.0 {
            kwh += (w[0].1 + w[1].1) / 2.0 * dt_h;
        }
    }
    if kwh > 0.0 { Some(kwh) } else { None }
}

/// Pre-v14 charging heuristic over power and rate.
pub fn is_charging(power_kw: Option<i64>, rate_mph: Option<f64>) -> bool {
    match rate_mph {
        Some(r) => r > 0.0,
        None => power_kw.is_some_and(|p| p > 0),
    }
}

/// Phase-first "is this row charging?" — the predicate `load_charge_rows`
/// uses to decide whether a sample belongs in a charge session. When the
/// row carries a persisted Tesla phase (v14+) it's authoritative;
/// pre-v14 rows fall back to `is_charging` over power and rate.
pub fn is_actively_charging(
    phase: Option<&str>,
    power_kw: Option<i64>,
    rate_mph: Option<f64>,
) -> bool {
    match phase_is_active(phase) {
        Some(active) => active,
        None => is_charging(power_kw, rate_mph),
    }
}

/// True/false if the persisted Tesla charge phase is an actively-charging
/// one. `None` when there's no phase string (pre-v14 rows) so the caller
/// can fall back to the old power/rate heuristic. The spellings mirror
/// `ChargingState::as_db_str` in the telemetry crate (deliberate string
/// contract — that binary crate can't be depended on from here).
pub fn phase_is_active(phase: Option<&str>) -> Option<bool> {
    phase.map(|p| matches!(p, "charging" | "starting" | "calibrating"))
}

/// Pull charging samples in `[from, to]` ordered by time. `to` of
/// `None` means "no upper bound".
pub fn load_charge_rows(
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
                battery_range_mi, battery_pct, location_name, \
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
            location: r.get(9)?,
            lat: r.get(10)?,
            lon: r.get(11)?,
            charging_state: r.get(12)?,
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
pub fn group_sessions(rows: Vec<ChargeRow>) -> Vec<Vec<ChargeRow>> {
    let mut sessions: Vec<Vec<ChargeRow>> = Vec::new();
    for row in rows {
        match sessions.last_mut() {
            Some(cur) if row.ts - cur.last().unwrap().ts <= SESSION_GAP_SECS => cur.push(row),
            _ => sessions.push(vec![row]),
        }
    }
    sessions
}

/// The session's representative GPS fix for the map pin: the most
/// frequently reported `(lat, lon)` across its samples, ties broken toward
/// the one seen latest. (Not the first non-null fix — at arrival the
/// reverse-geocoded address updates a poll or two before the raw GPS, so
/// the first charge sample can carry the new charger's address but the
/// previous location's coordinates.)
pub fn session_coord(rows: &[ChargeRow]) -> (Option<f64>, Option<f64>) {
    use std::collections::HashMap;
    // (lat bits, lon bits) → (count, last index seen, lat, lon).
    let mut seen: HashMap<(u64, u64), (usize, usize, f64, f64)> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let (Some(lat), Some(lon)) = (r.lat, r.lon) {
            let slot = seen
                .entry((lat.to_bits(), lon.to_bits()))
                .or_insert((0, i, lat, lon));
            slot.0 += 1;
            slot.1 = i;
        }
    }
    seen.values()
        .max_by_key(|(count, last_idx, _, _)| (*count, *last_idx))
        .map(|(_, _, lat, lon)| (Some(*lat), Some(*lon)))
        .unwrap_or((None, None))
}

/// Reduce one session's rows to a summary. `rows` is non-empty and
/// time-ordered. Tags/cost/currency are left empty — the api crate
/// fills them via its rate engine; the cloud blob ships them empty.
pub fn summarize(rows: &[ChargeRow]) -> ChargeSessionSummary {
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

    // Energy drawn from the charger ("used", wall-side): trapezoidal
    // integral of per-sample power over the session, refined per-sample
    // from volts × amps where possible (`sample_power_kw`).
    let energy_used_kwh = integrate_power_kwh(rows);

    // Charging efficiency = added / used, clamped as a residual safety
    // net against coarse data nudging "used" a hair under "added".
    let efficiency_pct = match (energy_added_kwh, energy_used_kwh) {
        (Some(added), Some(used)) if used > 0.0 => {
            Some((added / used * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };

    let peak_power_kw = rows.iter().filter_map(|r| r.power_kw).max();
    // Map-pin coordinate: the session's dominant fix, not the first
    // non-null one (which can be a stale arrival reading).
    let (location_lat, location_lon) = session_coord(rows);

    ChargeSessionSummary {
        id: first.ts,
        start_ms: first.ts * 1000,
        end_ms: last.ts * 1000,
        duration_secs: last.ts - first.ts,
        location: rows.iter().find_map(|r| r.location.clone()),
        location_lat,
        location_lon,
        energy_added_kwh,
        energy_used_kwh,
        efficiency_pct,
        peak_power_kw,
        start_soc: rows.iter().find_map(|r| r.battery_pct),
        end_soc: rows.iter().rev().find_map(|r| r.battery_pct),
        start_range_mi: rows.iter().find_map(|r| r.range_mi),
        end_range_mi: rows.iter().rev().find_map(|r| r.range_mi),
        charge_limit_soc: rows.iter().rev().find_map(|r| r.limit_soc),
        // Filled by the api crate's rate engine once tags + config are known.
        tags: Vec::new(),
        cost: None,
        rate: None,
        currency: String::new(),
        fast_charging: peak_power_kw.is_some_and(|p| p > FAST_CHARGE_THRESHOLD_KW),
        cost_overridden: false,
        // Derived by the api crate from the home geofence; stays false here
        // (and in the cloud blob, which never fills it).
        at_home: false,
    }
}

/// Per-sample chart series for one session, with the DC-current fix
/// applied (`display_current_a`). Shared by the local detail endpoint
/// and the cloud blob builder.
pub fn session_points(rows: &[ChargeRow]) -> Vec<ChargePoint> {
    rows.iter()
        .map(|r| ChargePoint {
            ts: r.ts * 1000,
            power_kw: r.power_kw,
            current_a: display_current_a(r.power_kw, r.voltage_v, r.current_a),
            voltage_v: r.voltage_v,
            rate_mph: r.rate_mph,
            soc: r.battery_pct,
            range_mi: r.range_mi,
            energy_added_kwh: r.energy_added_kwh,
        })
        .collect()
}

/// Evenly downsample chart points to at most `max` entries, always
/// keeping the first and last. Charge curves don't need full sample
/// density — this bounds the cloud blob size and makes detail-open
/// near-instant.
pub fn downsample_points(points: Vec<ChargePoint>, max: usize) -> Vec<ChargePoint> {
    let n = points.len();
    if n <= max || max < 2 {
        return points;
    }
    let mut out = Vec::with_capacity(max);
    let step = (n - 1) as f64 / (max - 1) as f64;
    let mut last_idx = usize::MAX;
    for i in 0..max {
        let idx = ((i as f64 * step).round() as usize).min(n - 1);
        if idx != last_idx {
            out.push(points[idx]);
            last_idx = idx;
        }
    }
    out
}

/// Reserved, DERIVED charge tags — computed on read, never stored.
///
/// They live here rather than in the API crate because two independent call
/// sites must agree on them: the API's tag-write handler, and the cloud
/// sync-in path in `sentryusb-cloud-uploader`. Neither crate depends on the
/// other; both depend on this one. A tag arriving from the cloud is otherwise
/// persisted as a real stored tag, which is exactly what the local write
/// handler forbids.
pub const HOME_TAG: &str = "Home";

/// Derived from charger power, not stored — see [`HOME_TAG`].
pub const FAST_TAG: &str = "Fast charging";

/// True when `tag` is reserved (case-insensitive) and so must never be stored.
pub fn is_reserved_tag(tag: &str) -> bool {
    tag.eq_ignore_ascii_case(HOME_TAG) || tag.eq_ignore_ascii_case(FAST_TAG)
}

/// Drop every reserved tag from user- or cloud-supplied tags, so a derived
/// label can never round-trip into the store as an editable one.
pub fn strip_reserved_tags(tags: Vec<String>) -> Vec<String> {
    tags.into_iter().filter(|t| !is_reserved_tag(t)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn row(ts: i64, power: Option<i64>, rate: Option<f64>, energy: Option<f64>) -> ChargeRow {
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
            location: None,
            lat: None,
            lon: None,
            charging_state: None,
        }
    }

    #[test]
    fn downsample_keeps_endpoints_and_bounds() {
        let pts = (0..1000)
            .map(|i| ChargePoint {
                ts: i,
                power_kw: Some(i),
                current_a: None,
                voltage_v: None,
                rate_mph: None,
                soc: None,
                range_mi: None,
                energy_added_kwh: None,
            })
            .collect::<Vec<_>>();
        let out = downsample_points(pts, 200);
        assert!(out.len() <= 200);
        assert_eq!(out.first().unwrap().ts, 0);
        assert_eq!(out.last().unwrap().ts, 999);
        // Strictly increasing — no duplicates, no reordering.
        assert!(out.windows(2).all(|w| w[0].ts < w[1].ts));
    }

    #[test]
    fn downsample_noop_when_small() {
        let pts = (0..50)
            .map(|i| ChargePoint {
                ts: i,
                power_kw: None,
                current_a: None,
                voltage_v: None,
                rate_mph: None,
                soc: None,
                range_mi: None,
                energy_added_kwh: None,
            })
            .collect::<Vec<_>>();
        assert_eq!(downsample_points(pts, 200).len(), 50);
    }

    #[test]
    fn gap_splits_into_two_sessions() {
        let rows = vec![
            row(0, Some(5), None, Some(1.0)),
            row(60, Some(5), None, Some(1.5)),
            row(60 + SESSION_GAP_SECS + 1, Some(5), None, Some(0.2)),
        ];
        let sessions = group_sessions(rows);
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].len(), 2);
        assert_eq!(sessions[1].len(), 1);
    }
}
