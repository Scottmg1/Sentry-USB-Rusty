//! Thin wrapper around the same SQLite DB the drives crate uses.
//! Opens with WAL, applies the drives migrations (so `telemetry_samples`
//! definitely exists), and exposes a one-shot insert.

use anyhow::{Context, Result};
use rusqlite::{params, Connection};

use crate::sample::Sample;

/// Open the canonical drive-data DB and ensure schema v6+ is applied.
/// The drives crate's `schema::migrate` is idempotent so running it
/// here alongside the sentryusb-main service is safe.
pub fn open() -> Result<Connection> {
    let path = sentryusb_drives::DEFAULT_DB_PATH;
    // Make sure the parent directory exists before SQLite tries to
    // create the file — dev / fresh-Pi cases would otherwise fail.
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let conn = Connection::open(path)
        .with_context(|| format!("failed to open {}", path))?;
    // busy_timeout matters here: this daemon shares the WAL DB with the
    // main server process. Without it, a write that lands while the
    // server holds the write lock fails instantly with SQLITE_BUSY and
    // the sample is dropped; with it, rusqlite retries for up to 5s.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA busy_timeout=5000;",
    )?;
    sentryusb_drives::schema::migrate(&conn)
        .context("schema migrate failed in telemetry sampler")?;
    Ok(conn)
}

/// Insert one sample. Duplicate `ts` rows are silently ignored
/// (`INSERT OR IGNORE`) — the sampler's clock-tick cadence makes
/// duplicates only possible if the daemon races itself on a clock
/// adjustment, in which case keeping the older row is fine.
pub fn insert(conn: &Connection, s: &Sample) -> Result<()> {
    // `software_version` column is intentionally not written —
    // Tesla doesn't expose `car_version` over BLE. The column stays
    // nullable in the schema for forward-compat.
    conn.execute(
        "INSERT OR IGNORE INTO telemetry_samples \
         (ts, battery_pct, battery_temp_c, interior_temp_c, exterior_temp_c, hvac_on, \
          tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi, \
          odometer_mi, location_name, \
          charger_power_kw, charger_actual_current_a, charger_voltage_v, \
          charge_rate_mph, charge_energy_added_kwh, charge_limit_soc, battery_range_mi, \
          charging_amps_set, charge_current_request_max, charge_port_door_open, \
          latitude, longitude, \
          source, charge_minutes_to_full, charging_state) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, \
                 ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, \
                 ?25, ?26, ?27)",
        params![
            s.ts,
            s.battery_pct,
            s.battery_temp_c,
            s.interior_temp_c,
            s.exterior_temp_c,
            s.hvac_on.map(|b| if b { 1_i64 } else { 0_i64 }),
            s.tire_fl_psi,
            s.tire_fr_psi,
            s.tire_rl_psi,
            s.tire_rr_psi,
            s.odometer_mi,
            s.location_name,
            s.charger_power_kw,
            s.charger_actual_current_a,
            s.charger_voltage_v,
            s.charge_rate_mph,
            s.charge_energy_added_kwh,
            s.charge_limit_soc,
            s.battery_range_mi,
            s.charging_amps_set,
            s.charge_current_request_max,
            s.charge_port_door_open.map(|b| if b { 1_i64 } else { 0_i64 }),
            s.latitude,
            s.longitude,
            s.source,
            s.charge_minutes_to_full,
            s.charging_state,
        ],
    )?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_memory_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=MEMORY;").unwrap();
        sentryusb_drives::schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn insert_full_sample() {
        let conn = fresh_memory_db();
        let s = Sample {
            ts: 1_700_000_000,
            battery_pct: Some(73.0),
            battery_temp_c: Some(18.5),
            interior_temp_c: Some(22.0),
            exterior_temp_c: Some(12.0),
            hvac_on: Some(true),
            tire_fl_psi: Some(40.0),
            tire_fr_psi: Some(40.5),
            tire_rl_psi: Some(38.5),
            tire_rr_psi: Some(39.0),
            odometer_mi: Some(12453.5),
            location_name: Some("123 Main St".into()),
            source: "state".into(),
            ..Sample::default()
        };
        insert(&conn, &s).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM telemetry_samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn insert_persists_charging_control_telemetry() {
        let conn = fresh_memory_db();
        let s = Sample {
            ts: 1_700_000_050,
            charging_amps_set: Some(32),
            charge_current_request_max: Some(48),
            charge_port_door_open: Some(true),
            source: "state".into(),
            ..Sample::default()
        };

        insert(&conn, &s).unwrap();
        let values: (Option<i64>, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT charging_amps_set, charge_current_request_max, charge_port_door_open \
                 FROM telemetry_samples WHERE ts = ?1",
                [s.ts],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(values, (Some(32), Some(48), Some(1)));
    }

    #[test]
    fn insert_sparse_body_controller_sample() {
        let conn = fresh_memory_db();
        let s = Sample {
            ts: 1_700_000_100,
            source: "body_controller".into(),
            ..Sample::default()
        };
        insert(&conn, &s).unwrap();
        let (pct, src): (Option<f64>, String) = conn
            .query_row(
                "SELECT battery_pct, source FROM telemetry_samples",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(pct.is_none());
        assert_eq!(src, "body_controller");
    }

    #[test]
    fn duplicate_ts_silently_ignored() {
        let conn = fresh_memory_db();
        let s = Sample {
            ts: 1_700_000_200,
            source: "state".into(),
            ..Sample::default()
        };
        insert(&conn, &s).unwrap();
        // Second insert with the same ts must not error.
        insert(&conn, &s).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM telemetry_samples", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "ON CONFLICT IGNORE should keep the first row");
    }
}
