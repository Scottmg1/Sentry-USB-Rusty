//! Charging history derived on demand from `telemetry_samples`.
//!
//! Sessions are contiguous active-charge samples split by `SESSION_GAP_SECS`.
//! The car's cumulative plug-in energy resets on unplug, so a session uses the
//! first-to-last span. With the experimental sampler disabled, charge columns
//! remain NULL and endpoints return empty results without scanning.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::router::AppState;

// The drives crate owns session identity/grouping shared with cloud sync;
// this API adds rates and costs.
use sentryusb_drives::charging::{
    avg, display_current_a, group_sessions, is_charging, load_charge_rows,
    phase_is_active, sample_power_kw, summarize, ChargePoint, ChargeRow,
    ChargeSessionSummary,
};
#[cfg(test)]
use sentryusb_drives::charging::{
    integrate_power_kwh, is_actively_charging, session_coord, FAST_CHARGE_THRESHOLD_KW,
    SESSION_GAP_SECS,
};

/// Stale-while-revalidate charging-list cache. Archive-time scans can take
/// seconds; mutations invalidate it, and the clock-independent JSON is shared
/// through an Arc to avoid repeated serialization.
static CHARGING_LIST_CACHE: crate::ttl_cache::StaleWhileRevalidate<(), std::sync::Arc<String>> =
    crate::ttl_cache::StaleWhileRevalidate::new(std::time::Duration::from_secs(15));

/// Invalidate after list mutations or a home-geofence move.
pub(crate) fn invalidate_charging_list() {
    CHARGING_LIST_CACHE.clear();
}

/// Cache the latest charge row without exceeding the sampler's cadence.
static LATEST_CHARGE_CACHE: crate::ttl_cache::StaleWhileRevalidate<(), Option<LatestCharge>> =
    crate::ttl_cache::StaleWhileRevalidate::new(std::time::Duration::from_secs(10));

/// Vehicle-changing controls reject samples older than two normal polls.
const CHARGING_CONTROL_FRESH_SECS: i64 = 120;


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
    points: Vec<ChargePoint>,
}


/// One time-of-use price window for a tag, scoped by time-of-day, days of
/// the week, and a month range — the device equivalent of a Tessie "rate
/// schedule". All bounds are in local time.
#[derive(PartialEq)]
struct RateSchedule {
    rate: f64,
    /// Local minutes-of-day. `start_min > end_min` wraps past midnight
    /// (e.g. 22:00–06:00 off-peak).
    start_min: i32,
    end_min: i32,
    /// Days the window applies on, 0=Sun..6=Sat. Empty = every day.
    days: Vec<i32>,
    /// Inclusive month range, 1=Jan..12=Dec. `start_month > end_month`
    /// wraps the year (e.g. Nov–Feb).
    start_month: i32,
    end_month: i32,
}

impl RateSchedule {
    /// Whether a local `minute`-of-day / `weekday` (0=Sun) / `month`
    /// (1=Jan) falls in this window. Time is half-open `[start, end)`,
    /// the month range is inclusive, and an empty `days` matches any day.
    fn covers(&self, minute: i32, weekday: i32, month: i32) -> bool {
        self.covers_time(minute) && self.covers_day(weekday) && self.covers_month(month)
    }

    /// Half-open time range that wraps at midnight. Equal bounds preserve
    /// legacy all-day schedules even though the editor now rejects them.
    fn covers_time(&self, min: i32) -> bool {
        if self.start_min == self.end_min {
            true
        } else if self.start_min < self.end_min {
            min >= self.start_min && min < self.end_min
        } else {
            min >= self.start_min || min < self.end_min
        }
    }

    fn covers_day(&self, weekday: i32) -> bool {
        self.days.is_empty() || self.days.contains(&weekday)
    }

    /// Inclusive month range, wrapping the year when `start_month >
    /// end_month` (e.g. Nov–Feb covers Nov, Dec, Jan, Feb).
    fn covers_month(&self, month: i32) -> bool {
        if self.start_month <= self.end_month {
            month >= self.start_month && month <= self.end_month
        } else {
            month >= self.start_month || month <= self.end_month
        }
    }
}

/// Pricing for one tag: an optional flat fallback rate plus any number of
/// time-of-use schedules. A charging interval is priced at the first
/// schedule that covers it, else `flat`, else the global default rate.
#[derive(PartialEq)]
struct TagRate {
    flat: Option<f64>,
    schedules: Vec<RateSchedule>,
}

impl TagRate {
    /// A plan with neither a flat rate nor a schedule carries no pricing,
    /// so it never wins selection and never costs a session.
    fn is_configured(&self) -> bool {
        self.flat.is_some() || !self.schedules.is_empty()
    }
}

/// Parse a time-of-day pref ("HH:MM", or a bare minute count) into
/// minutes-of-day.
fn parse_minute_of_day(v: &serde_json::Value) -> Option<i32> {
    if let Some(s) = v.as_str() {
        let s = s.trim();
        if let Some((h, m)) = s.split_once(':') {
            let h: i32 = h.trim().parse().ok()?;
            let m: i32 = m.trim().parse().ok()?;
            return ((0..=24).contains(&h) && (0..60).contains(&m))
                .then_some((h * 60 + m).min(1440));
        }
        let m: i32 = s.parse().ok()?;
        return (0..=1440).contains(&m).then_some(m);
    }
    let m = v.as_i64()? as i32;
    (0..=1440).contains(&m).then_some(m)
}

/// Electricity-rate preferences: currency, default per-kWh rate, and tag
/// plans. Rates accept JSON numbers or numeric strings from web inputs.
struct RateConfig {
    currency: String,
    default_rate: Option<f64>,
    tags: std::collections::HashMap<String, TagRate>,
}

impl RateConfig {
    fn load() -> Self {
        let prefs = crate::preferences::load_prefs();
        let currency = prefs
            .get("charging_currency")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("$")
            .to_string();
        let default_rate = prefs.get("charging_default_rate").and_then(num_from_json);
        let tags = prefs
            .get("charging_tag_rates")
            .and_then(|v| v.as_object())
            .map(|m| m.iter().map(|(k, v)| (k.clone(), parse_tag_rate(v))).collect())
            .unwrap_or_default();
        Self {
            currency,
            default_rate,
            tags,
        }
    }

    /// Copy the Home rate to a freeze label, returning whether cloud sync must
    /// be nudged. Check label reuse under the prefs lock to avoid repricing
    /// unrelated sessions during a concurrent rate edit.
    fn copy_home_rate_to(
        tag: &str,
        label_in_use: impl FnOnce() -> anyhow::Result<bool>,
    ) -> anyhow::Result<bool> {
        crate::preferences::update_prefs(|prefs| match home_rate_copy_plan(prefs, tag) {
            RateCopy::Nothing => Ok(false),
            RateCopy::Conflict(why) => anyhow::bail!("{why}"),
            RateCopy::Copy(rate) => {
                if label_in_use()? {
                    anyhow::bail!(
                        "\"{tag}\" is already used by other charges — pick a name that isn't in use"
                    );
                }
                let mut rates = prefs
                    .get("charging_tag_rates")
                    .and_then(|v| v.as_object())
                    .cloned()
                    .unwrap_or_default();
                rates.insert(tag.to_string(), rate);
                prefs.insert(
                    "charging_tag_rates".to_string(),
                    serde_json::Value::Object(rates),
                );
                Ok(true)
            }
        })
    }
}

/// Parse legacy flat-rate entries or `{ flat, schedules }` plans.
fn parse_tag_rate(v: &serde_json::Value) -> TagRate {
    if let Some(obj) = v.as_object() {
        let flat = obj.get("flat").and_then(num_from_json);
        let schedules = obj
            .get("schedules")
            .and_then(|s| s.as_array())
            .map(|arr| arr.iter().filter_map(parse_schedule).collect())
            .unwrap_or_default();
        TagRate { flat, schedules }
    } else {
        TagRate {
            flat: num_from_json(v),
            schedules: Vec::new(),
        }
    }
}

/// Parse a valid schedule; omitted days/months mean every day/full year.
fn parse_schedule(v: &serde_json::Value) -> Option<RateSchedule> {
    let obj = v.as_object()?;
    Some(RateSchedule {
        rate: num_from_json(obj.get("rate")?)?,
        start_min: parse_minute_of_day(obj.get("start")?)?,
        end_min: parse_minute_of_day(obj.get("end")?)?,
        days: parse_days(obj.get("days")),
        start_month: parse_month(obj.get("startMonth")).unwrap_or(1),
        end_month: parse_month(obj.get("endMonth")).unwrap_or(12),
    })
}

/// Normalize weekday values; an empty result means every day.
fn parse_days(v: Option<&serde_json::Value>) -> Vec<i32> {
    let Some(arr) = v.and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    let mut days: Vec<i32> = arr
        .iter()
        .filter_map(|d| d.as_i64())
        .map(|d| d as i32)
        .filter(|d| (0..=6).contains(d))
        .collect();
    days.sort_unstable();
    days.dedup();
    days
}

/// Parse an in-range month from a JSON number or numeric string.
fn parse_month(v: Option<&serde_json::Value>) -> Option<i32> {
    let v = v?;
    let m = v
        .as_i64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<i64>().ok()))? as i32;
    (1..=12).contains(&m).then_some(m)
}

/// Parse a finite non-negative rate from a JSON number or string.
fn num_from_json(v: &serde_json::Value) -> Option<f64> {
    let n = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()))?;
    (n.is_finite() && n >= 0.0).then_some(n)
}

/// Integrate interval power and apply the first matching schedule, then the
/// plan flat rate, then the global default. Returns None without sufficient
/// samples or any configured rate.
fn plan_cost(
    rows: &[ChargeRow],
    flat: Option<f64>,
    schedules: &[RateSchedule],
    default_rate: Option<f64>,
) -> Option<f64> {
    use chrono::{Datelike, Local, Timelike};
    let pts: Vec<(i64, f64)> = rows
        .iter()
        .filter_map(|r| sample_power_kw(r).map(|p| (r.ts, p)))
        .collect();
    if pts.len() < 2 {
        return None;
    }
    let mut cost = 0.0;
    let mut priced = false;
    for w in pts.windows(2) {
        let dt_h = (w[1].0 - w[0].0) as f64 / 3600.0;
        if dt_h <= 0.0 {
            continue;
        }
        let energy = (w[0].1 + w[1].1) / 2.0 * dt_h;
        let mid_ts = (w[0].0 + w[1].0) / 2;
        // The local clock at the interval midpoint selects the schedule.
        let rate = chrono::DateTime::from_timestamp(mid_ts, 0).and_then(|dt| {
            let local = dt.with_timezone(&Local);
            let minute = local.hour() as i32 * 60 + local.minute() as i32;
            let weekday = local.weekday().num_days_from_sunday() as i32;
            let month = local.month() as i32;
            schedules
                .iter()
                .find(|s| s.covers(minute, weekday, month))
                .map(|s| s.rate)
                .or(flat)
                .or(default_rate)
        });
        if let Some(rate) = rate {
            cost += energy * rate;
            priced = true;
        }
    }
    priced.then_some(cost)
}

/// Apply tags and wall-side energy cost. Configured tag plans beat the default;
/// the highest matching plan wins, and `rate` is the blended effective rate.
fn apply_rates(
    s: &mut ChargeSessionSummary,
    rows: &[ChargeRow],
    tags: Vec<String>,
    at_home: bool,
    rates: &RateConfig,
) {
    // Home is derived rather than stored and matches configured keys by case.
    let mut rate_keys: Vec<&str> = tags.iter().map(String::as_str).collect();
    if at_home {
        // Include each configured Home spelling once; the max fold is order-free.
        for k in rates.tags.keys() {
            if k.eq_ignore_ascii_case(HOME_TAG) && !rate_keys.contains(&k.as_str()) {
                rate_keys.push(k.as_str());
            }
        }
    }
    // Select the most expensive configured plan carried by the session.
    let best_tag_cost = rate_keys
        .iter()
        .filter_map(|t| rates.tags.get(*t))
        .filter(|p| p.is_configured())
        .filter_map(|p| plan_cost(rows, p.flat, &p.schedules, rates.default_rate))
        .fold(None, |acc: Option<f64>, c| Some(acc.map_or(c, |a: f64| a.max(c))));

    let (cost, rate) = if let Some(c) = best_tag_cost {
        let blended = match s.energy_used_kwh {
            Some(u) if u > 0.0 => Some(c / u),
            _ => None,
        };
        (Some(c), blended)
    } else if let Some(dr) = rates.default_rate {
        // Fall back to the default for unpriced sessions.
        (s.energy_used_kwh.map(|u| dr * u), Some(dr))
    } else {
        (None, None)
    };
    s.cost = cost;
    s.rate = rate;
    s.currency = rates.currency.clone();
    s.tags = tags;
    s.at_home = at_home;
}

/// Apply a stored manual total over rate-derived cost, clearing the per-kWh
/// rate and marking the summary. None leaves derived cost unchanged.
fn apply_cost_override(s: &mut ChargeSessionSummary, override_cost: Option<(f64, String)>) {
    if let Some((amount, currency)) = override_cost {
        s.cost = Some(amount);
        // Legacy blank overrides inherit the current rate currency.
        if !currency.is_empty() {
            s.currency = currency;
        }
        s.rate = None;
        s.cost_overridden = true;
    }
}

/// Default home geofence radius (m) when `KEEP_ACCESSORY_HOME_RADIUS_M` is unset.
const HOME_TAG_RADIUS_M: f64 = 120.0;

// The drives crate owns reserved tags shared with cloud sync-in.
use sentryusb_drives::charging::{strip_reserved_tags, HOME_TAG};


/// Home geofence (lat, lon, radius_m) for the auto "Home" charge tag. Center
/// is shared with keep-accessory (`KEEP_ACCESSORY_HOME_LAT/LON`); `None` when
/// unset, leaving the feature inert.
fn home_geofence() -> Option<(f64, f64, f64)> {
    let config_path = sentryusb_config::find_config_path();
    let (active, commented) = sentryusb_config::parse_file(config_path).ok()?;
    let g = |k: &str| sentryusb_config::get_config_value(&active, &commented, k);
    // Match BLE at_home's coordinate validation.
    let lat = g("KEEP_ACCESSORY_HOME_LAT")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| (-90.0..=90.0).contains(v))?;
    // Older configs may contain a world-copy longitude.
    let lon = g("KEEP_ACCESSORY_HOME_LON")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(crate::normalize_lon)?;
    // Reject malformed radii that could classify every session as home.
    let radius = g("KEEP_ACCESSORY_HOME_RADIUS_M")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|r| r.is_finite() && *r > 0.0 && *r <= 100_000.0)
        .unwrap_or(HOME_TAG_RADIUS_M);
    Some((lat, lon, radius))
}

/// Great-circle distance in meters (haversine). Local copy, as in `away_mode`.
fn distance_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const EARTH_R_M: f64 = 6_371_000.0;
    let (p1, p2) = (lat1.to_radians(), lat2.to_radians());
    let dphi = (lat2 - lat1).to_radians();
    let dlambda = (lon2 - lon1).to_radians();
    let a = (dphi / 2.0).sin().powi(2) + p1.cos() * p2.cos() * (dlambda / 2.0).sin().powi(2);
    2.0 * EARTH_R_M * a.sqrt().min(1.0).asin()
}


/// What freezing under `tag` should do to the rate map.
#[derive(Debug, PartialEq)]
enum RateCopy {
    /// No rate change needed, and none of the sessions change price.
    Nothing,
    /// Refuse the whole freeze; the payload is shown to the user.
    Conflict(String),
    /// Give `tag` this rate.
    Copy(serde_json::Value),
}

/// Decide how to preserve pricing under a freeze label. Destination lookup is
/// exact, matching `apply_rates`; Home source lookup is case-insensitive.
/// Parsed plans compare legacy and object representations semantically.
fn home_rate_copy_plan(
    prefs: &serde_json::Map<String, serde_json::Value>,
    tag: &str,
) -> RateCopy {
    let Some(rates) = prefs.get("charging_tag_rates").and_then(|v| v.as_object()) else {
        return RateCopy::Nothing; // no rates at all: nothing to preserve
    };
    // Ignore malformed Home entries because they price nothing.
    let home_keys: Vec<&serde_json::Value> = rates
        .iter()
        .filter(|(k, v)| k.eq_ignore_ascii_case(HOME_TAG) && parse_tag_rate(v).is_configured())
        .map(|(_, v)| v)
        .collect();
    // Conflicting Home spellings are ambiguous because apply_rates takes the max.
    if home_keys.len() > 1 && home_keys.windows(2).any(|w| parse_tag_rate(w[0]) != parse_tag_rate(w[1]))
    {
        return RateCopy::Conflict(format!(
            "there are two \"{HOME_TAG}\" rates differing only in capitalization — remove one first"
        ));
    }
    let home_rate = rates
        .get(HOME_TAG)
        .filter(|v| parse_tag_rate(v).is_configured())
        .or_else(|| home_keys.first().copied());
    // An unconfigured destination may be replaced because it prices nothing.
    let dest = rates.get(tag).filter(|v| parse_tag_rate(v).is_configured());
    match (home_rate, dest) {
        // Without a Home rate, a priced destination would invent a cost.
        (None, Some(_)) => RateCopy::Conflict(format!(
            "\"{tag}\" already has a rate, and these charges had none — pick another name"
        )),
        (None, None) => RateCopy::Nothing,
        (Some(home), None) => RateCopy::Copy(home.clone()),
        (Some(home), Some(d)) if parse_tag_rate(home) == parse_tag_rate(d) => RateCopy::Nothing,
        (Some(_), Some(_)) => RateCopy::Conflict(format!(
            "\"{tag}\" already has a different rate — pick another name, or those charges would be repriced instead of preserved"
        )),
    }
}

/// Project the geofence after a pending write, rounding changed coordinates to
/// the six decimals persisted on disk. None means no configured home.
pub(crate) fn pending_home_geofence(
    lat: Option<f64>,
    lon: Option<f64>,
    radius_m: Option<f64>,
) -> Option<(f64, f64, f64)> {
    let (cur_lat, cur_lon, cur_radius) = home_geofence()?;
    // Preserve existing precision for fields omitted from the write.
    let round6 = |v: f64| format!("{v:.6}").parse::<f64>().unwrap_or(v);
    Some((
        lat.map(round6).unwrap_or(cur_lat),
        lon.map(round6).unwrap_or(cur_lon),
        radius_m.unwrap_or(cur_radius),
    ))
}

/// Whether an at-home session leaves the new geofence. An unknown destination
/// conservatively freezes every current Home session to avoid losing cost.
fn loses_home(
    lat: Option<f64>,
    lon: Option<f64>,
    old_home: (f64, f64, f64),
    new_home: Option<(f64, f64, f64)>,
) -> bool {
    is_home_charge(lat, lon, Some(old_home)) && !is_home_charge(lat, lon, new_home)
}

/// Session ids a move would untag. Read failures must not look like an empty set.
pub(crate) fn sessions_losing_home(
    store: &sentryusb_drives::db::DriveStore,
    new_home: Option<(f64, f64, f64)>,
) -> anyhow::Result<Vec<i64>> {
    let old_home = match home_geofence() {
        Some(h) => h,
        None => return Ok(Vec::new()), // no home configured: nothing was ever Home
    };
    let rows = store.with_read_conn(|conn| load_charge_rows(conn, 0, None))?;
    Ok(group_sessions(rows)
        .iter()
        .map(|g| summarize(g))
        .filter(|s| loses_home(s.location_lat, s.location_lon, old_home, new_home))
        .map(|s| s.id)
        .collect())
}

/// Session ids in the current home geofence.
pub(crate) fn sessions_at_home(store: &sentryusb_drives::db::DriveStore) -> anyhow::Result<Vec<i64>> {
    sessions_losing_home(store, None)
}

/// Freeze sessions that leave Home and copy the Home rate to the label. Sessions
/// still inside the new circle remain derived-only. Tag writes are additive and
/// transactional.
pub(crate) fn freeze_home_sessions(
    store: &sentryusb_drives::db::DriveStore,
    tag: &str,
    new_home: Option<(f64, f64, f64)>,
) -> anyhow::Result<Frozen> {
    let tag = tag.trim();
    if tag.is_empty() || sentryusb_drives::charging::is_reserved_tag(tag) {
        anyhow::bail!("pick a name that isn't a reserved tag");
    }
    let ids = sessions_losing_home(store, new_home)?;
    if ids.is_empty() {
        return Ok(Frozen { tagged: 0, rates_dirty: false });
    }
    // Resolve rate conflicts before writing tags; exact matching mirrors apply_rates.
    let id_set: std::collections::HashSet<i64> = ids.iter().copied().collect();
    RateConfig::copy_home_rate_to(tag, || {
        // A failed reuse check aborts. SQLite and prefs lack a shared lock, so a
        // narrow concurrent tag edit can cause recoverable repricing, not data loss.
        Ok(store
            .get_all_charge_tags()?
            .iter()
            .any(|(id, tags)| !id_set.contains(id) && tags.iter().any(|t| t == tag)))
    })?;
    // Mark before tag writes whenever the label is priced. A pull replaces an
    // unmarked rate map, and retries may see the prior rate copy as unchanged.
    // Failure leaves only an inert orphan rate, so abort.
    let rates_dirty = tag_is_priced(&crate::preferences::load_prefs(), tag);
    if rates_dirty {
        store.mark_rate_config_dirty()?;
    }
    let n = store.add_charge_tag_bulk(&ids, tag)?;
    Ok(Frozen { tagged: n, rates_dirty })
}

/// Whether an exact tag key resolves to a configured rate for sync marking.
fn tag_is_priced(prefs: &serde_json::Map<String, serde_json::Value>, tag: &str) -> bool {
    prefs
        .get("charging_tag_rates")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get(tag))
        .is_some_and(|v| parse_tag_rate(v).is_configured())
}

/// Freeze outcome. `rates_dirty` drives the cloud-sync nudge.
pub(crate) struct Frozen {
    pub tagged: usize,
    pub rates_dirty: bool,
}

/// True when a session's charge location falls inside the home geofence.
fn is_home_charge(lat: Option<f64>, lon: Option<f64>, home: Option<(f64, f64, f64)>) -> bool {
    match (lat, lon, home) {
        (Some(la), Some(lo), Some((hla, hlo, r))) => {
            // Normalize consistently with the BLE at_home path.
            distance_m(la, crate::normalize_lon(lo), hla, hlo) <= r
        }
        _ => false,
    }
}

/// Backstop for a charging phase left behind when BLE misses the terminal poll.
const CHARGE_STALE_SECS: i64 = 86_400;






/// GET /api/charging: charge sessions newest-first.
pub async fn list_charging(State(state): State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;

    // Keep the full scan, grouping, and preference reads off async workers.
    let store = state.drives.store.clone();
    let body = CHARGING_LIST_CACHE
        .get((), move || {
            let build = || -> anyhow::Result<Vec<ChargeSessionSummary>> {
                let rows = store.with_read_conn(|conn| load_charge_rows(conn, 0, None))?;
                let rates = RateConfig::load();
                // Parse the geofence once per cache miss; the rendered JSON cache
                // applies home changes on its next refresh.
                let home = home_geofence();
                let tag_map = store.get_all_charge_tags().unwrap_or_default();
                let cost_map = store.get_all_charge_costs().unwrap_or_default();
                let mut sessions: Vec<ChargeSessionSummary> = group_sessions(rows)
                    .iter()
                    .map(|s| {
                        let mut summary = summarize(s);
                        let stored = tag_map.get(&summary.id).cloned().unwrap_or_default();
                        let at_home =
                            is_home_charge(summary.location_lat, summary.location_lon, home);
                        let override_cost = cost_map.get(&summary.id).cloned();
                        apply_rates(&mut summary, s, stored, at_home, &rates);
                        apply_cost_override(&mut summary, override_cost);
                        summary
                    })
                    .collect();
                sessions.sort_by(|a, b| b.id.cmp(&a.id));
                Ok(sessions)
            };
            // Preserve the prior list when a refresh races storage activity.
            let sessions = build().ok()?;
            Some(std::sync::Arc::new(
                serde_json::json!({ "sessions": sessions }).to_string(),
            ))
        })
        .await;

    match body {
        Some(json) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json.as_str().to_owned(),
        )
            .into_response(),
        None => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "charging list query failed",
        )
        .into_response(),
    }
}

/// GET /api/charging/{id}: re-derive a session and its chart samples from its
/// stable start timestamp without a stored session table.
pub async fn single_charging(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Keep bounded scans, grouping, locked lookups, and prefs I/O off the reactor.
    let store = state.drives.store.clone();
    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<Option<serde_json::Value>> {
            // Bound malformed sessions that never close.
            let window_end = id + 7 * 24 * 60 * 60;
            let rows =
                store.with_read_conn(|conn| load_charge_rows(conn, id, Some(window_end)))?;

            let session = match group_sessions(rows).into_iter().next() {
                Some(s) => s,
                None => return Ok(None),
            };

            let mut summary = summarize(&session);
            let stored = store.get_charge_tags(summary.id).unwrap_or_default();
            let at_home =
                is_home_charge(summary.location_lat, summary.location_lon, home_geofence());
            let override_cost = store.get_charge_cost(summary.id).unwrap_or_default();
            apply_rates(&mut summary, &session, stored, at_home, &RateConfig::load());
            apply_cost_override(&mut summary, override_cost);

            let points: Vec<ChargePoint> = session
                .iter()
                .map(|r| ChargePoint {
                    ts: r.ts * 1000,
                    power_kw: r.power_kw,
                    // DC charging bypasses the onboard current sensor; derive P/V.
                    current_a: display_current_a(r.power_kw, r.voltage_v, r.current_a),
                    voltage_v: r.voltage_v,
                    rate_mph: r.rate_mph,
                    soc: r.battery_pct,
                    range_mi: r.range_mi,
                    energy_added_kwh: r.energy_added_kwh,
                })
                .collect();

            let detail = ChargeSessionDetail {
                avg_power_kw: avg(session.iter().filter_map(|r| r.power_kw.map(|v| v as f64))),
                peak_current_a: session
                    .iter()
                    .filter_map(|r| display_current_a(r.power_kw, r.voltage_v, r.current_a))
                    .max(),
                avg_current_a: avg(session.iter().filter_map(|r| {
                    display_current_a(r.power_kw, r.voltage_v, r.current_a).map(|v| v as f64)
                })),
                peak_voltage_v: session.iter().filter_map(|r| r.voltage_v).max(),
                avg_voltage_v: avg(session.iter().filter_map(|r| r.voltage_v.map(|v| v as f64))),
                peak_rate_mph: session
                    .iter()
                    .filter_map(|r| r.rate_mph)
                    .fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v)))),
                summary,
                points,
            };

            Ok(Some(serde_json::to_value(detail)?))
        })
        .await;

    match result {
        Ok(Ok(Some(v))) => (StatusCode::OK, Json(v)),
        Ok(Ok(None)) => crate::json_error(StatusCode::NOT_FOUND, "charge session not found"),
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("charging task: {}", e))
        }
    }
}

/// Dashboard charge status. Metrics require active charging; control fields
/// require a fresher active-charge or open-port sample.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CurrentCharge {
    charging: bool,
    soc: Option<f64>,
    limit_soc: Option<i64>,
    power_kw: Option<i64>,
    current_a: Option<i64>,
    voltage_v: Option<i64>,
    rate_mph: Option<f64>,
    energy_added_kwh: Option<f64>,
    minutes_to_full: Option<i64>,
    range_mi: Option<f64>,
    charging_amps: Option<i64>,
    max_charging_amps: Option<i64>,
    charge_port_open: Option<bool>,
    controls_available: bool,
    controls_valid_until_ts: Option<i64>,
}

impl CurrentCharge {
    fn idle() -> Self {
        Self {
            charging: false,
            soc: None,
            limit_soc: None,
            power_kw: None,
            current_a: None,
            voltage_v: None,
            rate_mph: None,
            energy_added_kwh: None,
            minutes_to_full: None,
            range_mi: None,
            charging_amps: None,
            max_charging_amps: None,
            charge_port_open: None,
            controls_available: false,
            controls_valid_until_ts: None,
        }
    }

    fn from_latest(latest: Option<LatestCharge>, now: i64) -> Self {
        let Some(l) = latest else {
            return Self::idle();
        };
        let age = now - l.ts;
        let charging = match phase_is_active(l.charging_state.as_deref()) {
            Some(true) => age <= CHARGE_STALE_SECS,
            Some(false) => false,
            None => age <= 600 && is_charging(l.power_kw, l.rate_mph),
        };
        let soc = if age <= CHARGE_STALE_SECS { l.soc } else { None };
        let controls_available = l.controls_available(now);
        let controls_fresh = (0..=CHARGING_CONTROL_FRESH_SECS).contains(&age);
        Self {
            charging,
            soc,
            limit_soc: if charging { l.limit_soc } else { None },
            power_kw: if charging { l.power_kw } else { None },
            current_a: if charging {
                display_current_a(l.power_kw, l.voltage_v, l.current_a)
            } else {
                None
            },
            voltage_v: if charging { l.voltage_v } else { None },
            rate_mph: if charging { l.rate_mph } else { None },
            energy_added_kwh: if charging { l.energy_added_kwh } else { None },
            minutes_to_full: if charging { l.minutes_to_full } else { None },
            range_mi: if charging {
                l.range_mi
            } else {
                l.range_mi.filter(|_| soc.is_some())
            },
            charging_amps: l.charging_amps_set.filter(|_| controls_fresh),
            max_charging_amps: l.charge_current_request_max.filter(|_| controls_fresh),
            charge_port_open: l.charge_port_door_open.filter(|_| controls_fresh),
            controls_available,
            controls_valid_until_ts: controls_available
                .then_some(l.ts + CHARGING_CONTROL_FRESH_SECS),
        }
    }
}

/// The single most-recent telemetry row, charge-relevant columns only.
#[derive(Clone)]
struct LatestCharge {
    ts: i64,
    soc: Option<f64>,
    limit_soc: Option<i64>,
    power_kw: Option<i64>,
    current_a: Option<i64>,
    voltage_v: Option<i64>,
    rate_mph: Option<f64>,
    energy_added_kwh: Option<f64>,
    minutes_to_full: Option<i64>,
    range_mi: Option<f64>,
    charging_state: Option<String>,
    charging_amps_set: Option<i64>,
    charge_current_request_max: Option<i64>,
    charge_port_door_open: Option<bool>,
}

impl LatestCharge {
    fn controls_available(&self, now: i64) -> bool {
        let age = now - self.ts;
        let fresh = (0..=CHARGING_CONTROL_FRESH_SECS).contains(&age);
        let active = phase_is_active(self.charging_state.as_deref()) == Some(true)
            || (self.charging_state.is_none() && is_charging(self.power_kw, self.rate_mph));
        fresh && (self.charge_port_door_open == Some(true) || active)
    }
}

fn query_latest_charge(conn: &rusqlite::Connection) -> rusqlite::Result<Option<LatestCharge>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT ts, battery_pct, charge_limit_soc, charger_power_kw, \
                charger_actual_current_a, charger_voltage_v, charge_rate_mph, \
                charge_energy_added_kwh, charge_minutes_to_full, battery_range_mi, \
                charging_state, charging_amps_set, charge_current_request_max, \
                charge_port_door_open \
         FROM telemetry_samples \
         WHERE source = 'state' AND (charging_state IS NOT NULL \
            OR charger_power_kw IS NOT NULL \
            OR charge_rate_mph IS NOT NULL \
            OR charge_port_door_open IS NOT NULL) \
         ORDER BY ts DESC LIMIT 1",
        [],
        |r| {
            Ok(LatestCharge {
                ts: r.get(0)?,
                soc: r.get(1)?,
                limit_soc: r.get(2)?,
                power_kw: r.get(3)?,
                current_a: r.get(4)?,
                voltage_v: r.get(5)?,
                rate_mph: r.get(6)?,
                energy_added_kwh: r.get(7)?,
                minutes_to_full: r.get(8)?,
                range_mi: r.get(9)?,
                charging_state: r.get(10)?,
                charging_amps_set: r.get(11)?,
                charge_current_request_max: r.get(12)?,
                charge_port_door_open: r.get::<_, Option<i64>>(13)?.map(|v| v != 0),
            })
        },
    )
    .optional()
}

/// GET /api/charging/current. Persisted Tesla phase is authoritative because
/// BLE may poll sparsely mid-charge; pre-v14 rows fall back to fresh nonzero
/// power/rate.
pub async fn current_charging(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Cache the row rather than response so freshness still uses the current clock
    // and archive-time disk stalls do not queue every poll.
    let store = state.drives.store.clone();
    let latest = LATEST_CHARGE_CACHE
        .get((), move || {
        store.with_read_conn(query_latest_charge)
        // Failed reads retain the cache; successful empty reads clear it.
        .ok()
        })
        .await
        .flatten();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let cur = CurrentCharge::from_latest(latest, now);
    (StatusCode::OK, Json(serde_json::to_value(cur).unwrap()))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChargingActionRequest {
    action: String,
    value: Option<i64>,
}

fn charging_action_verb(
    req: &ChargingActionRequest,
    max_charging_amps: Option<i64>,
) -> Result<String, String> {
    match req.action.as_str() {
        "start" => Ok("charge-start".into()),
        "stop" => Ok("charge-stop".into()),
        "setAmps" => {
            let value = req.value.ok_or_else(|| "charging amps value is required".to_string())?;
            let reported_max = max_charging_amps
                .filter(|max| (1..=80).contains(max))
                .ok_or_else(|| "vehicle did not report a safe charging-current maximum".to_string())?;
            if !(1..=reported_max).contains(&value) {
                return Err(format!("charging amps must be between 1 and {reported_max}"));
            }
            Ok(format!("set-charging-amps:{value}"))
        }
        "setLimit" => {
            let value = req.value.ok_or_else(|| "charge limit value is required".to_string())?;
            if !(50..=100).contains(&value) {
                return Err("charge limit must be between 50 and 100".into());
            }
            Ok(format!("set-charge-limit:{value}"))
        }
        _ => Err("action must be start, stop, setAmps, or setLimit".into()),
    }
}

/// POST /api/charging/action, enforcing the UI safety gate server-side.
pub async fn charging_action(
    State(state): State<AppState>,
    Json(req): Json<ChargingActionRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let store = state.drives.store.clone();
    let (latest, health_is_green) = match tokio::task::spawn_blocking(move || {
        store.with_read_conn(|conn| -> rusqlite::Result<(Option<LatestCharge>, bool)> {
            Ok((
                query_latest_charge(conn)?,
                crate::ble::charging_controls_health_is_green(conn, now),
            ))
        })
    })
    .await
    {
        Ok(Ok((Some(latest), health_is_green))) => (latest, health_is_green),
        Ok(Ok((None, _))) => {
            return crate::json_error(StatusCode::CONFLICT, "no charging telemetry is available");
        }
        Ok(Err(e)) => {
            return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("charging telemetry read failed: {e}"));
        }
        Err(e) => {
            return crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("charging telemetry task failed: {e}"));
        }
    };
    if !health_is_green {
        return crate::json_error(
            StatusCode::CONFLICT,
            "charging controls require a healthy authenticated BLE connection",
        );
    }
    if !latest.controls_available(now) {
        return crate::json_error(
            StatusCode::CONFLICT,
            "charging controls require a fresh open charge port or active charge",
        );
    }
    let verb = match charging_action_verb(&req, latest.charge_current_request_max) {
        Ok(verb) => verb,
        Err(message) => return crate::json_error(StatusCode::BAD_REQUEST, &message),
    };

    match sentryusb_shell::run_with_timeout(
        Duration::from_secs(30),
        "/root/bin/sentryusb-ble-action",
        &[verb.as_str()],
    )
    .await
    {
        Ok(_) => {
            LATEST_CHARGE_CACHE.clear();
            let _ = sentryusb_shell::run(
                "systemctl",
                &["kill", "-s", "SIGUSR1", "sentryusb-telemetry"],
            )
            .await;
            (StatusCode::OK, Json(serde_json::json!({ "ok": true })))
        }
        Err(e) => crate::json_error(
            StatusCode::BAD_GATEWAY,
            &format!("charging command failed: {e}"),
        ),
    }
}

#[derive(Deserialize)]
pub struct SetChargeTagsRequest {
    pub tags: Vec<String>,
}

/// GET /api/charging/tags — every charge tag in use, sorted.
pub async fn list_charge_tags(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = state.drives.store.clone();
    let task = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        let mut tags = store.get_all_charge_tag_names()?;
        // Include derived Home when configured.
        if home_geofence().is_some() && !tags.iter().any(|t| t.eq_ignore_ascii_case(HOME_TAG)) {
            tags.push(HOME_TAG.to_string());
            tags.sort();
        }
        Ok(tags)
    });
    match task.await {
        Ok(Ok(tags)) => (
            StatusCode::OK,
            Json(serde_json::to_value(tags).unwrap_or_default()),
        ),
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("tags task: {}", e))
        }
    }
}

/// PUT /api/charging/{id}/tags uses the session start timestamp as stable id.
/// GET /api/charging/home-sessions counts sessions in the current geofence;
/// errors must not be misreported as zero before a move.
pub async fn home_session_count(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = state.drives.store.clone();
    match tokio::task::spawn_blocking(move || sessions_at_home(&store)).await {
        Ok(Ok(ids)) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "count": ids.len(),
                "home_set": home_geofence().is_some(),
            })),
        ),
        Ok(Err(e)) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("could not read charge history: {e}"),
        ),
        Err(e) => crate::json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("task failed: {e}"),
        ),
    }
}

pub async fn set_charge_tags(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SetChargeTagsRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let store = state.drives.store.clone();
    // Derived reserved tags never enter local or cloud storage.
    let tags = strip_reserved_tags(body.tags);
    match tokio::task::spawn_blocking(move || store.set_charge_tags(id, &tags)).await {
        Ok(Ok(())) => {
            invalidate_charging_list();
            crate::json_ok()
        }
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("tags task: {}", e))
        }
    }
}

#[derive(Deserialize)]
pub struct SetChargeCostRequest {
    /// The manual total for this charge. `null` clears the override and
    /// reverts the session to its rate-derived cost.
    pub amount: Option<f64>,
}

/// PUT /api/charging/{id}/cost stores or clears a total in the current currency.
pub async fn set_charge_cost(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<SetChargeCostRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let cost = match body.amount {
        Some(a) if a.is_finite() && a >= 0.0 => Some((a, RateConfig::load().currency)),
        // Malformed amounts must not masquerade as a successful clear.
        Some(_) => {
            return crate::json_error(
                StatusCode::BAD_REQUEST,
                "amount must be a non-negative number",
            );
        }
        None => None,
    };
    let store = state.drives.store.clone();
    match tokio::task::spawn_blocking(move || store.set_charge_cost(id, cost)).await {
        Ok(Ok(())) => {
            invalidate_charging_list();
            crate::json_ok()
        }
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("cost task: {}", e))
        }
    }
}

#[derive(Deserialize)]
pub struct BulkDeleteChargesRequest {
    pub ids: Vec<String>,
}

/// POST /api/charging/bulk-delete removes charge-bearing samples and metadata
/// for start-timestamp ids while preserving non-charge samples in each window.
pub async fn bulk_delete_charges(
    State(state): State<AppState>,
    Json(body): Json<BulkDeleteChargesRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.ids.is_empty() {
        return (
            StatusCode::OK,
            Json(serde_json::json!({ "deleted": 0, "sessions": 0 })),
        );
    }
    let ids: Vec<i64> = body.ids.iter().filter_map(|s| s.parse::<i64>().ok()).collect();

    // Bounded scans and mutex-held deletes belong on the blocking pool.
    let store = state.drives.store.clone();
    let result = tokio::task::spawn_blocking(move || {
        store.with_locked_conn(|conn| -> anyhow::Result<(usize, usize)> {
            let mut deleted = 0usize;
            let mut sessions = 0usize;
            let now = chrono::Utc::now().timestamp();
            for id in &ids {
                // Re-derive the bounded session window from its start id.
                let window_end = *id + 7 * 24 * 60 * 60;
                let rows = load_charge_rows(conn, *id, Some(window_end))?;
                let session = match group_sessions(rows).into_iter().next() {
                    Some(s) if !s.is_empty() => s,
                    _ => continue,
                };
                let start = session.first().unwrap().ts;
                let end = session.last().unwrap().ts;
                // Commit local deletion and cloud outbox entry atomically.
                let tx = conn.unchecked_transaction()?;
                deleted += tx.execute(
                    "DELETE FROM telemetry_samples WHERE ts BETWEEN ?1 AND ?2 \
                     AND (charging_state IS NOT NULL \
                          OR charger_power_kw IS NOT NULL \
                          OR charge_rate_mph IS NOT NULL)",
                    rusqlite::params![start, end],
                )?;
                tx.execute(
                    "DELETE FROM charge_tags WHERE session_ts = ?1",
                    rusqlite::params![start],
                )?;
                // Remove the start-id-keyed manual cost override.
                tx.execute(
                    "DELETE FROM charge_costs WHERE session_ts = ?1",
                    rusqlite::params![start],
                )?;
                // Queue deletion even if an upload may still be in flight.
                tx.execute(
                    "INSERT INTO charge_delete_outbox(session_ts, cloud_charge_id, queued_at) \
                     VALUES(?1, ?2, ?3) \
                     ON CONFLICT(session_ts) DO UPDATE SET \
                       cloud_charge_id = ?2, queued_at = ?3, acked_at = NULL",
                    rusqlite::params![
                        start,
                        sentryusb_cloud_uploader::charge_id_for_session(start),
                        now
                    ],
                )?;
                tx.execute(
                    "DELETE FROM charge_uploads WHERE session_ts = ?1",
                    rusqlite::params![start],
                )?;
                tx.execute(
                    "DELETE FROM mutable_dirty WHERE kind = 'charge' AND key = ?1",
                    rusqlite::params![start.to_string()],
                )?;
                tx.commit()?;
                sessions += 1;
            }
            Ok((deleted, sessions))
        })
    })
    .await;

    match result {
        Ok(Ok((deleted, sessions))) => {
            invalidate_charging_list();
            if sessions > 0 {
                // Drain promptly rather than waiting for the safety timer.
                state.cloud.uploader.nudge();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({ "deleted": deleted, "sessions": sessions })),
            )
        }
        Ok(Err(e)) => crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        Err(e) => {
            crate::json_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("charging task: {}", e))
        }
    }
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
            // Exceeds the session gap.
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
        // Climate can draw power while an explicit zero charge rate proves the
        // battery is not charging; this must not create a phantom session.
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

    // Persisted phase is authoritative; legacy rows fall back to power/rate.

    #[test]
    fn phase_charging_is_included_even_with_weak_signals() {
        // Charging phase survives transient zero/missing metrics.
        assert!(is_actively_charging(Some("charging"), Some(0), Some(0.0)));
        assert!(is_actively_charging(Some("charging"), None, None));
        assert!(is_actively_charging(Some("starting"), Some(1), None));
        assert!(is_actively_charging(Some("calibrating"), None, Some(0.0)));
    }

    #[test]
    fn phase_complete_excludes_phantom_power_draw() {
        // Complete phase overrides nonzero auxiliary power draw.
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
        // Missing phase uses the legacy rate-first predicate.
        assert!(is_actively_charging(None, Some(4), Some(20.0)));
        assert!(is_actively_charging(None, Some(7), None));
        assert!(!is_actively_charging(None, Some(2), Some(0.0))); // phantom
        assert!(!is_actively_charging(None, None, None));
        assert!(!is_actively_charging(None, Some(0), Some(0.0)));
    }

    #[test]
    fn structs_serialize_camelcase_for_the_web_client() {
        // Pin the camelCase wire contract consumed by the web UI.
        let s = summarize(&[
            row(1_000, Some(7), Some(25.0), Some(2.0)),
            row(1_300, Some(11), Some(40.0), Some(9.4)),
        ]);
        let j = serde_json::to_string(&s).unwrap();
        for key in ["startMs", "endMs", "durationSecs", "energyAddedKwh", "peakPowerKw"] {
            assert!(j.contains(&format!("\"{key}\"")), "summary must emit {key}: {j}");
        }
        assert!(!j.contains("\"start_ms\""), "summary must NOT emit snake_case: {j}");

        // Values are synthetic; only serialized keys matter.
        let p = ChargePoint {
            ts: 1,
            power_kw: Some(1),
            current_a: Some(1),
            voltage_v: Some(1),
            rate_mph: Some(1.0),
            soc: Some(1.0),
            range_mi: Some(1.0),
            energy_added_kwh: Some(1.0),
        };
        let jp = serde_json::to_string(&p).unwrap();
        for key in ["powerKw", "currentA", "voltageV", "rateMph", "rangeMi", "energyAddedKwh"] {
            assert!(jp.contains(&format!("\"{key}\"")), "point must emit {key}: {jp}");
        }
        assert!(!jp.contains("\"power_kw\""), "point must NOT emit snake_case: {jp}");
    }

    #[test]
    fn current_charge_wire_shape_includes_live_electrical_details() {
        let value = serde_json::to_value(CurrentCharge::idle()).unwrap();
        for key in [
            "currentA",
            "voltageV",
            "rateMph",
            "energyAddedKwh",
            "chargingAmps",
            "maxChargingAmps",
            "chargePortOpen",
        ] {
            assert_eq!(
                value.get(key),
                Some(&serde_json::Value::Null),
                "idle current-charge response must include nullable {key}: {value}",
            );
        }
        assert_eq!(value["controlsAvailable"], false);
    }

    #[test]
    fn active_current_charge_carries_latest_live_details() {
        let latest = LatestCharge {
            ts: 1_000,
            soc: Some(64.0),
            limit_soc: Some(80),
            power_kw: Some(7),
            current_a: Some(32),
            voltage_v: Some(240),
            rate_mph: Some(28.5),
            energy_added_kwh: Some(12.3),
            minutes_to_full: Some(75),
            range_mi: Some(172.0),
            charging_state: Some("charging".into()),
            charging_amps_set: Some(32),
            charge_current_request_max: Some(48),
            charge_port_door_open: Some(true),
        };

        let value = serde_json::to_value(CurrentCharge::from_latest(Some(latest), 1_060)).unwrap();
        assert_eq!(value["charging"], true);
        assert_eq!(value["currentA"], 32);
        assert_eq!(value["voltageV"], 240);
        assert_eq!(value["rateMph"], 28.5);
        assert_eq!(value["energyAddedKwh"], 12.3);
        assert_eq!(value["chargingAmps"], 32);
        assert_eq!(value["maxChargingAmps"], 48);
        assert_eq!(value["chargePortOpen"], true);
        assert_eq!(value["controlsAvailable"], true);
        assert_eq!(value["controlsValidUntilTs"], 1_120);
    }

    #[test]
    fn active_dc_charge_derives_current_when_tesla_reports_zero_amps() {
        let latest = LatestCharge {
            ts: 1_000,
            soc: Some(64.0),
            limit_soc: Some(80),
            power_kw: Some(158),
            current_a: Some(0),
            voltage_v: Some(389),
            rate_mph: Some(600.0),
            energy_added_kwh: Some(12.3),
            minutes_to_full: Some(20),
            range_mi: Some(172.0),
            charging_state: Some("charging".into()),
            charging_amps_set: Some(250),
            charge_current_request_max: Some(250),
            charge_port_door_open: Some(true),
        };

        let value = serde_json::to_value(CurrentCharge::from_latest(Some(latest), 1_060)).unwrap();
        assert_eq!(value["currentA"], 406);
    }

    #[test]
    fn charging_controls_require_fresh_open_port_or_active_charge() {
        let mut latest = LatestCharge {
            ts: 1_000,
            soc: Some(64.0),
            limit_soc: Some(80),
            power_kw: Some(0),
            current_a: Some(0),
            voltage_v: Some(240),
            rate_mph: Some(0.0),
            energy_added_kwh: Some(12.3),
            minutes_to_full: None,
            range_mi: Some(172.0),
            charging_state: Some("stopped".into()),
            charging_amps_set: Some(32),
            charge_current_request_max: Some(48),
            charge_port_door_open: Some(true),
        };

        let open_port = serde_json::to_value(CurrentCharge::from_latest(
            Some(latest.clone()),
            1_060,
        ))
        .unwrap();
        assert_eq!(open_port["controlsAvailable"], true);

        latest.charge_port_door_open = Some(false);
        let closed = serde_json::to_value(CurrentCharge::from_latest(
            Some(latest.clone()),
            1_060,
        ))
        .unwrap();
        assert_eq!(closed["controlsAvailable"], false);

        latest.charging_state = Some("charging".into());
        let stale = serde_json::to_value(CurrentCharge::from_latest(Some(latest), 1_121)).unwrap();
        assert_eq!(stale["controlsAvailable"], false);
        assert_eq!(stale["controlsValidUntilTs"], serde_json::Value::Null);
    }

    #[test]
    fn charging_action_request_validates_ranges_and_reported_max() {
        let verb = charging_action_verb(
            &ChargingActionRequest {
                action: "setAmps".into(),
                value: Some(32),
            },
            Some(48),
        )
        .unwrap();
        assert_eq!(verb, "set-charging-amps:32");
        assert!(charging_action_verb(
            &ChargingActionRequest {
                action: "setAmps".into(),
                value: Some(49),
            },
            Some(48),
        )
        .is_err());
        assert_eq!(
            charging_action_verb(
                &ChargingActionRequest {
                    action: "setLimit".into(),
                    value: Some(80),
                },
                Some(48),
            )
            .unwrap(),
            "set-charge-limit:80"
        );
        assert!(charging_action_verb(
            &ChargingActionRequest {
                action: "setLimit".into(),
                value: Some(49),
            },
            Some(48),
        )
        .is_err());
    }

    /// Flat-rate fixture for cost and efficiency tests.
    fn rates(default: Option<f64>, tags: &[(&str, f64)]) -> RateConfig {
        RateConfig {
            currency: "$".into(),
            default_rate: default,
            tags: tags
                .iter()
                .map(|(k, v)| (k.to_string(), TagRate { flat: Some(*v), schedules: Vec::new() }))
                .collect(),
        }
    }

    /// One hour at 10 kW gives a 10 kWh pricing fixture.
    fn hour_session() -> [ChargeRow; 2] {
        [
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ]
    }

    /// Compare optional floating-point results with tolerance.
    fn approx(a: Option<f64>, b: f64) -> bool {
        matches!(a, Some(x) if (x - b).abs() < 1e-9)
    }

    #[test]
    fn highest_cost_tag_plan_wins_then_default() {
        let session = hour_session();
        let r = rates(Some(0.10), &[("Home", 0.12), ("Public", 0.40)]);
        let cost_for = |tags: Vec<String>| {
            let mut s = summarize(&session);
            apply_rates(&mut s, &session, tags, false, &r);
            s.cost
        };
        assert!(approx(cost_for(vec![]), 1.0));
        assert!(approx(cost_for(vec!["Work".into()]), 1.0));
        assert!(approx(cost_for(vec!["Home".into()]), 1.2));
        assert!(approx(cost_for(vec!["Home".into(), "Public".into()]), 4.0));
        assert!(approx(cost_for(vec!["Public".into(), "Home".into()]), 4.0));
    }

    #[test]
    fn strip_reserved_tags_removes_home_any_casing() {
        let out = strip_reserved_tags(vec![
            "Home".into(),
            "home".into(),
            "HOME".into(),
            "Fast charging".into(),
            "FAST CHARGING".into(),
            "Work".into(),
            "Homebrew".into(),
        ]);
        // Reserved labels match whole tags case-insensitively, not substrings.
        assert_eq!(out, vec!["Work".to_string(), "Homebrew".to_string()]);
    }

    #[test]
    fn at_home_prices_via_home_rate_and_sets_flag() {
        let session = hour_session();
        let run = |tags: Vec<String>, at_home: bool, r: &RateConfig| {
            let mut s = summarize(&session);
            apply_rates(&mut s, &session, tags, at_home, r);
            (s.cost, s.at_home, s.tags)
        };
        let (cost, at_home, stored) = run(vec![], true, &rates(Some(0.10), &[("Home", 0.40)]));
        assert!(approx(cost, 4.0));
        assert!(at_home);
        assert!(stored.is_empty()); // "Home" is derived, never added to tags
        // A stored reserved spelling cannot suppress the derived Home rate.
        assert!(approx(
            run(vec!["home".into()], true, &rates(Some(0.10), &[("Home", 0.40)])).0,
            4.0
        ));
        assert!(approx(
            run(vec![], true, &rates(Some(0.10), &[("Home", 0.20), ("home", 0.40)])).0,
            4.0
        ));
        let (cost, at_home, _) = run(vec![], false, &rates(Some(0.10), &[("Home", 0.40)]));
        assert!(approx(cost, 1.0));
        assert!(!at_home);
        assert!(approx(run(vec![], true, &rates(Some(0.10), &[])).0, 1.0));
    }

    #[test]
    fn cost_is_none_without_default_or_tag_plan() {
        let session = hour_session();
        let mut s = summarize(&session);
        apply_rates(&mut s, &session, vec!["Home".into()], false, &rates(None, &[]));
        assert_eq!(s.cost, None);
        assert_eq!(s.rate, None);
    }

    #[test]
    fn num_from_json_accepts_number_or_string_rejects_negative() {
        assert_eq!(num_from_json(&serde_json::json!(0.30)), Some(0.30));
        assert_eq!(num_from_json(&serde_json::json!("0.30")), Some(0.30));
        assert_eq!(num_from_json(&serde_json::json!(0)), Some(0.0));
        assert_eq!(num_from_json(&serde_json::json!(-1.0)), None);
        assert_eq!(num_from_json(&serde_json::json!("abc")), None);
    }

    #[test]
    fn energy_used_is_trapezoidal_integral_of_power() {
        let used = integrate_power_kwh(&[
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ])
        .unwrap();
        assert!((used - 10.0).abs() < 1e-9, "expected 10 kWh, got {used}");
        assert_eq!(integrate_power_kwh(&[row(0, Some(10), None, None)]), None);
    }

    #[test]
    fn low_power_used_refined_from_volts_amps() {
        // V×I corrects integer-kW undercount that produced >100% efficiency.
        let mut a = row(0, Some(1), Some(4.0), Some(0.0));
        a.voltage_v = Some(121);
        a.current_a = Some(12);
        let mut b = row(3600, Some(1), Some(4.0), Some(1.4));
        b.voltage_v = Some(121);
        b.current_a = Some(12);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 1.452).abs() < 1e-6, "expected ~1.452 kWh from V×I, got {used}");
    }

    #[test]
    fn level2_power_refined_within_tolerance() {
        // Refine a plausible single-phase integer-kW sample with V×I.
        let mut a = row(0, Some(11), Some(40.0), Some(0.0));
        a.voltage_v = Some(240);
        a.current_a = Some(48);
        let mut b = row(3600, Some(11), Some(40.0), Some(11.0));
        b.voltage_v = Some(240);
        b.current_a = Some(48);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 11.52).abs() < 1e-6, "expected V×I refinement 11.52, got {used}");
    }

    #[test]
    fn three_phase_power_falls_back_to_integer() {
        // Reject per-phase V×I when reported power already sums three phases.
        let mut a = row(0, Some(11), Some(30.0), Some(0.0));
        a.voltage_v = Some(230);
        a.current_a = Some(16);
        let mut b = row(3600, Some(11), Some(30.0), Some(10.0));
        b.voltage_v = Some(230);
        b.current_a = Some(16);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 11.0).abs() < 1e-9, "expected integer fallback 11 kWh, got {used}");
    }

    #[test]
    fn missing_volts_or_amps_uses_integer_power() {
        // Missing voltage retains reported power.
        let mut a = row(0, Some(7), Some(25.0), Some(0.0));
        a.current_a = Some(30); // voltage still None
        let mut b = row(3600, Some(7), Some(25.0), Some(7.0));
        b.current_a = Some(30);
        let used = integrate_power_kwh(&[a, b]).unwrap();
        assert!((used - 7.0).abs() < 1e-9, "expected integer 7 kWh when volts missing, got {used}");
    }

    #[test]
    fn display_current_derives_dc_amps_only_when_reported_zero_or_missing() {
        // DC bypasses the onboard sensor, so derive current from P/V.
        assert_eq!(display_current_a(Some(158), Some(389), Some(0)), Some(406));
        assert_eq!(display_current_a(Some(85), Some(398), Some(0)), Some(214));
        assert_eq!(display_current_a(Some(90), Some(397), None), Some(227));
        // Preserve measured AC current rather than deriving it.
        assert_eq!(display_current_a(Some(11), Some(240), Some(48)), Some(48));
        assert_eq!(display_current_a(Some(1), Some(120), Some(12)), Some(12));
        assert_eq!(display_current_a(Some(150), None, Some(0)), Some(0));
        assert_eq!(display_current_a(None, None, None), None);
    }

    #[test]
    fn fast_charging_flag_tracks_peak_power_above_threshold() {
        let sc = summarize(&[
            row(0, Some(60), Some(500.0), Some(0.0)),
            row(60, Some(158), Some(600.0), Some(6.0)),
        ]);
        assert!(sc.fast_charging);
        let home = summarize(&[
            row(0, Some(7), Some(25.0), Some(0.0)),
            row(60, Some(11), Some(40.0), Some(1.0)),
        ]);
        assert!(!home.fast_charging);
        // The strict threshold keeps 22 kW AC wallboxes non-fast.
        let edge = summarize(&[
            row(0, Some(22), Some(80.0), Some(0.0)),
            row(60, Some(22), Some(80.0), Some(1.0)),
        ]);
        assert!(!edge.fast_charging);
    }

    #[test]
    fn session_coord_uses_dominant_fix_not_stale_leading_sample() {
        // Use the dominant fix when address and GPS update on adjacent polls.
        let mut rows = vec![
            row(0, Some(150), Some(600.0), Some(0.0)),
            row(60, Some(150), Some(600.0), Some(5.0)),
            row(120, Some(120), Some(500.0), Some(9.0)),
        ];
        rows[0].lat = Some(10.0); // stale leading fix (seen once)
        rows[0].lon = Some(20.0);
        rows[1].lat = Some(30.0); // the real location (dominant — seen twice)
        rows[1].lon = Some(40.0);
        rows[2].lat = Some(30.0);
        rows[2].lon = Some(40.0);
        let s = summarize(&rows);
        assert_eq!(s.location_lat, Some(30.0));
        assert_eq!(s.location_lon, Some(40.0));
        let mut single = vec![row(0, Some(2), Some(5.0), Some(0.0))];
        single[0].lat = Some(50.0);
        single[0].lon = Some(60.0);
        let ss = summarize(&single);
        assert_eq!(ss.location_lat, Some(50.0));
    }

    #[test]
    fn summarize_computes_used_and_efficiency_then_apply_rates_costs_on_used() {
        let session = [
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ];
        let mut s = summarize(&session);
        assert_eq!(s.energy_added_kwh, Some(9.0)); // battery-side
        assert_eq!(s.energy_used_kwh, Some(10.0)); // wall-side
        assert_eq!(s.efficiency_pct.map(|p| p.round()), Some(90.0));

        // Cost uses wall-side energy, including losses.
        apply_rates(&mut s, &session, vec!["Home".into()], false, &rates(None, &[("Home", 0.30)]));
        assert_eq!(s.tags, vec!["Home".to_string()]);
        assert_eq!(s.rate, Some(0.30));
        assert_eq!(s.cost, Some(3.0));
        assert_eq!(s.currency, "$");
    }

    #[test]
    fn apply_rates_leaves_cost_none_when_no_rate() {
        let session = [
            row(0, Some(10), Some(30.0), Some(0.0)),
            row(3600, Some(10), Some(30.0), Some(9.0)),
        ];
        let mut s = summarize(&session);
        apply_rates(&mut s, &session, vec![], false, &rates(None, &[]));
        assert_eq!(s.cost, None);
        assert_eq!(s.rate, None);
    }

    #[test]
    fn manual_cost_override_beats_rate_and_sets_flag() {
        let session = hour_session();
        let mut s = summarize(&session);
        apply_rates(&mut s, &session, vec!["Home".into()], false, &rates(None, &[("Home", 0.30)]));
        assert_eq!(s.cost, Some(3.0));
        assert!(!s.cost_overridden);
        // Manual totals replace derived cost and rate metadata.
        apply_cost_override(&mut s, Some((18.75, "€".to_string())));
        assert_eq!(s.cost, Some(18.75));
        assert_eq!(s.rate, None);
        assert_eq!(s.currency, "€");
        assert!(s.cost_overridden);
    }

    #[test]
    fn no_override_leaves_rate_cost_untouched() {
        let session = hour_session();
        let mut s = summarize(&session);
        apply_rates(&mut s, &session, vec!["Home".into()], false, &rates(None, &[("Home", 0.30)]));
        apply_cost_override(&mut s, None);
        assert_eq!(s.cost, Some(3.0));
        assert!(!s.cost_overridden);
    }

    #[test]
    fn equal_start_end_covers_all_day() {
        // Equal bounds preserve legacy all-day schedules.
        let s = RateSchedule {
            rate: 0.10,
            start_min: 0,
            end_min: 0,
            days: vec![],
            start_month: 1,
            end_month: 12,
        };
        assert!(s.covers(0, 3, 7));
        assert!(s.covers(12 * 60, 3, 7));
        assert!(s.covers(1439, 3, 7));

        let nine = RateSchedule { start_min: 9 * 60, end_min: 9 * 60, ..s };
        assert!(nine.covers(9 * 60, 3, 7));
        assert!(nine.covers(8 * 60, 3, 7));
    }

    #[test]
    fn schedule_covers_time_day_month() {
        let s = RateSchedule {
            rate: 0.08,
            start_min: 22 * 60,
            end_min: 6 * 60,
            days: vec![1, 2, 3, 4, 5],
            start_month: 6,
            end_month: 9,
        };
        assert!(s.covers(23 * 60, 3, 7)); // 11pm Wed in July — in window
        assert!(s.covers(2 * 60, 3, 7)); // 2am — still off-peak (wraps midnight)
        assert!(!s.covers(12 * 60, 3, 7)); // noon — outside time window
        assert!(!s.covers(23 * 60, 0, 7)); // Sunday — outside day set
        assert!(!s.covers(23 * 60, 3, 12)); // December — outside month range

        // Empty weekdays and a year-wrapping month range.
        let winter = RateSchedule {
            rate: 0.05,
            start_min: 0,
            end_min: 1440,
            days: vec![],
            start_month: 11,
            end_month: 2,
        };
        assert!(winter.covers(9 * 60, 0, 1)); // January, Sunday — inside wrap
        assert!(winter.covers(9 * 60, 0, 12)); // December — inside wrap
        assert!(!winter.covers(9 * 60, 0, 6)); // June — outside wrapped range
    }

    #[test]
    fn tag_schedule_prices_session() {
        let session = hour_session();
        let mut r = rates(None, &[]);
        r.tags.insert(
            "Home".into(),
            TagRate {
                flat: None,
                schedules: vec![RateSchedule {
                    rate: 0.20,
                    start_min: 0,
                    end_min: 1440,
                    days: vec![],
                    start_month: 1,
                    end_month: 12,
                }],
            },
        );
        let mut s = summarize(&session);
        apply_rates(&mut s, &session, vec!["Home".into()], false, &r);
        assert!(approx(s.cost, 2.0));
        assert!(approx(s.rate, 0.20));
    }

    #[test]
    fn tag_plan_beats_default() {
        let session = hour_session();
        let mut s = summarize(&session);
        apply_rates(
            &mut s,
            &session,
            vec!["Supercharger".into()],
            false,
            &rates(Some(0.10), &[("Supercharger", 0.40)]),
        );
        assert!(approx(s.cost, 4.0)); // 0.40 × 10 used; tag beats default
        assert!(approx(s.rate, 0.40));
    }

    #[test]
    fn parse_tag_rate_accepts_number_and_object() {
        let legacy = parse_tag_rate(&serde_json::json!(0.04));
        assert_eq!(legacy.flat, Some(0.04));
        assert!(legacy.schedules.is_empty());

        // Object shape accepts numeric strings from web inputs.
        let plan = parse_tag_rate(&serde_json::json!({
            "flat": 0.30,
            "schedules": [{
                "label": "Off-peak",
                "start": "22:00",
                "end": "06:00",
                "days": [1, 2, 3, 4, 5],
                "startMonth": 6,
                "endMonth": 9,
                "rate": "0.08"
            }]
        }));
        assert_eq!(plan.flat, Some(0.30));
        assert_eq!(plan.schedules.len(), 1);
        let sch = &plan.schedules[0];
        assert_eq!(sch.rate, 0.08);
        assert_eq!(sch.start_min, 22 * 60);
        assert_eq!(sch.end_min, 6 * 60);
        assert_eq!(sch.days, vec![1, 2, 3, 4, 5]);
        assert_eq!(sch.start_month, 6);
        assert_eq!(sch.end_month, 9);

        let empty = parse_tag_rate(&serde_json::json!({}));
        assert_eq!(empty.flat, None);
        assert!(!empty.is_configured());
    }

    #[test]
    fn parse_minute_of_day_handles_hhmm_and_numbers() {
        assert_eq!(parse_minute_of_day(&serde_json::json!("06:30")), Some(390));
        assert_eq!(parse_minute_of_day(&serde_json::json!("22:00")), Some(1320));
        assert_eq!(parse_minute_of_day(&serde_json::json!(390)), Some(390));
        assert_eq!(parse_minute_of_day(&serde_json::json!("nope")), None);
        assert_eq!(parse_minute_of_day(&serde_json::json!("25:00")), None);
    }
    #[test]
    fn home_rate_is_copied_onto_the_frozen_tag() {
        let mk = |json: &str| -> serde_json::Map<String, serde_json::Value> {
            serde_json::from_str(json).unwrap()
        };

        // Copy Home pricing so frozen sessions retain cost.
        let p = mk(r#"{"charging_tag_rates":{"Home":0.12,"Public":0.45}}"#);
        assert_eq!(
            home_rate_copy_plan(&p, "Old House"),
            RateCopy::Copy(serde_json::json!(0.12))
        );

        let p = mk(r#"{"charging_tag_rates":{"home":0.2}}"#);
        assert_eq!(
            home_rate_copy_plan(&p, "Old House"),
            RateCopy::Copy(serde_json::json!(0.2))
        );

        // Copy the complete scheduled plan.
        let p = mk(r#"{"charging_tag_rates":{"Home":{"flat":0.1,"schedules":[{"rate":0.05}]}}}"#);
        match home_rate_copy_plan(&p, "Old House") {
            RateCopy::Copy(v) => assert_eq!(v["schedules"][0]["rate"], serde_json::json!(0.05)),
            other => panic!("expected a copy, got {other:?}"),
        }

        let p = mk(r#"{"charging_tag_rates":{"Public":0.45}}"#);
        assert_eq!(home_rate_copy_plan(&p, "Old House"), RateCopy::Nothing);
        assert_eq!(home_rate_copy_plan(&mk("{}"), "Old House"), RateCopy::Nothing);

        // An identical destination plan is a no-op.
        let p = mk(r#"{"charging_tag_rates":{"Home":0.12,"Old House":0.12}}"#);
        assert_eq!(home_rate_copy_plan(&p, "Old House"), RateCopy::Nothing);

        let p = mk(r#"{"charging_tag_rates":{"Home":0.2,"Old House":"0.20"}}"#);
        assert_eq!(home_rate_copy_plan(&p, "Old House"), RateCopy::Nothing);
        let p = mk(r#"{"charging_tag_rates":{"Home":0.2,"Old House":{"flat":0.2}}}"#);
        assert_eq!(home_rate_copy_plan(&p, "Old House"), RateCopy::Nothing);

        // Refuse a conflicting deliberate destination rate.
        let p = mk(r#"{"charging_tag_rates":{"Home":0.20,"Old House":0.29}}"#);
        assert!(matches!(home_rate_copy_plan(&p, "Old House"), RateCopy::Conflict(_)));

        // Destination matching uses the exact render-time spelling.
        let p = mk(r#"{"charging_tag_rates":{"Home":0.12,"old house":0.12}}"#);
        assert_eq!(
            home_rate_copy_plan(&p, "Old House"),
            RateCopy::Copy(serde_json::json!(0.12))
        );

        // Home source matching remains case-insensitive.
        let p = mk(r#"{"charging_tag_rates":{"home":0.4,"Home":0.4}}"#);
        assert_eq!(
            home_rate_copy_plan(&p, "Old House"),
            RateCopy::Copy(serde_json::json!(0.4))
        );

        // Refuse conflicting Home spellings rather than guessing which max won.
        let p = mk(r#"{"charging_tag_rates":{"home":0.4,"Home":0.1}}"#);
        assert!(matches!(home_rate_copy_plan(&p, "Old House"), RateCopy::Conflict(_)));

        // A priced label would invent cost when Home had none.
        let p = mk(r#"{"charging_tag_rates":{"Old House":0.29}}"#);
        assert!(matches!(home_rate_copy_plan(&p, "Old House"), RateCopy::Conflict(_)));

        let p = mk(r#"{"charging_tag_rates":{"Home":0.15,"Old House":{}}}"#);
        assert_eq!(
            home_rate_copy_plan(&p, "Old House"),
            RateCopy::Copy(serde_json::json!(0.15))
        );

        let p = mk(r#"{"charging_tag_rates":{"Old House":{}}}"#);
        assert_eq!(home_rate_copy_plan(&p, "Old House"), RateCopy::Nothing);

        // Malformed Home plans price nothing.
        for junk in [r#""NaN""#, r#""-1""#, "{}", r#"{"flat":null}"#] {
            let p = mk(&format!(r#"{{"charging_tag_rates":{{"Home":{junk}}}}}"#));
            assert_eq!(
                home_rate_copy_plan(&p, "Old House"),
                RateCopy::Nothing,
                "junk Home rate {junk} must not be copied"
            );
        }
    }

    /// False negatives lose unrecoverable cost; false positives leave a removable tag.
    #[test]
    fn only_the_charges_a_move_leaves_behind_are_frozen() {
        let old = (40.000_000, -75.000_000, 120.0);
        let at_old = (Some(40.000_000), Some(-75.000_000));
        let far = (Some(40.010_000), Some(-75.000_000));

        assert!(loses_home(at_old.0, at_old.1, old, None));

        let moved = Some((40.010_000, -75.000_000, 120.0));
        assert!(loses_home(at_old.0, at_old.1, old, moved));

        assert!(!loses_home(at_old.0, at_old.1, old, Some(old)));

        // Radius increases must not stamp a redundant permanent label.
        let wider = Some((40.000_000, -75.000_000, 400.0));
        assert!(!loses_home(at_old.0, at_old.1, old, wider));

        // Radius shrink freezes only sessions now outside.
        let tighter = Some((40.000_000, -75.000_000, 20.0));
        let edge = (Some(40.000_500), Some(-75.000_000));
        assert!(loses_home(edge.0, edge.1, old, tighter));
        assert!(!loses_home(at_old.0, at_old.1, old, tighter));

        assert!(!loses_home(far.0, far.1, old, moved));
        assert!(!loses_home(None, None, old, moved));
    }

    /// Dirty marking prevents the next cloud pull from replacing a preserved rate.
    #[test]
    fn a_priced_freeze_label_is_recognised_for_sync() {
        let mk = |json: &str| -> serde_json::Map<String, serde_json::Value> {
            serde_json::from_str(json).unwrap()
        };

        let p = mk(r#"{"charging_tag_rates":{"Home":0.12,"Old House":0.12}}"#);
        assert!(tag_is_priced(&p, "Old House"));

        // Retries must mark an already-copied rate after a failed tag write.
        assert_eq!(home_rate_copy_plan(&p, "Old House"), RateCopy::Nothing);
        assert!(tag_is_priced(&p, "Old House"), "an equal rate still needs marking");

        let p = mk(
            r#"{"charging_tag_rates":{"Old House":{"schedules":[
                {"start":"22:00","end":"06:00","rate":"0.08"}
            ]}}}"#,
        );
        assert!(tag_is_priced(&p, "Old House"));

        // Invalid schedules discarded by the parser do not price the label.
        let p = mk(r#"{"charging_tag_rates":{"Old House":{"schedules":[{"rate":0.05}]}}}"#);
        assert!(!tag_is_priced(&p, "Old House"));

        assert!(!tag_is_priced(&mk("{}"), "Old House"));
        let p = mk(r#"{"charging_tag_rates":{"Home":0.12}}"#);
        assert!(!tag_is_priced(&p, "Old House"));

        // Exact matching mirrors render-time lookup.
        let p = mk(r#"{"charging_tag_rates":{"old house":0.12}}"#);
        assert!(!tag_is_priced(&p, "Old House"));

        for junk in [r#""NaN""#, r#""-1""#, "{}", r#"{"flat":null}"#] {
            let p = mk(&format!(r#"{{"charging_tag_rates":{{"Old House":{junk}}}}}"#));
            assert!(!tag_is_priced(&p, "Old House"), "junk rate {junk} is not a price");
        }
    }
}
