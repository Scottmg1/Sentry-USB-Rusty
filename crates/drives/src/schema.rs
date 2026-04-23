//! SQLite schema + migrations — port of Go `server/drives/schema.go`.
//!
//! Migration semantics must match Go so a DB written by the Go binary
//! opens cleanly under Rust (and vice versa): same table shapes, same
//! column names, same `meta(key, value)` keys, same idempotent-ALTER
//! logic for v2 upgrades.
//!
//! v1 -> v2: add precomputed per-route aggregate columns (distance,
//! speeds, autopilot-mode time/distance, disengagement counts, start/end
//! lat-lon) so the Drives-page summary endpoints can scan BLOB-free rows.
//! See `aggregate.rs` for semantics.

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

/// Schema version this binary writes. Stored in the `meta` table and
/// checked on every open so future upgrades can run targeted migrations.
pub const CURRENT_SCHEMA_VERSION: i32 = 2;

/// v1 DDL. Each statement is idempotent (`IF NOT EXISTS`) so `migrate()`
/// is safe on every startup. Column shapes and names match Go exactly —
/// cross-binary DBs must not diverge.
const V1_SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    ) WITHOUT ROWID",

    "CREATE TABLE IF NOT EXISTS routes (
        file              TEXT PRIMARY KEY,
        date_dir          TEXT NOT NULL,
        point_count       INTEGER NOT NULL DEFAULT 0,
        raw_park_count    INTEGER NOT NULL DEFAULT 0,
        raw_frame_count   INTEGER NOT NULL DEFAULT 0,
        start_ts          INTEGER,
        end_ts            INTEGER,
        distance_m        REAL NOT NULL DEFAULT 0,
        first_lat         REAL,
        first_lon         REAL,
        points_blob       BLOB NOT NULL,
        gear_states_blob  BLOB,
        ap_states_blob    BLOB,
        speeds_blob       BLOB,
        accel_blob        BLOB,
        gear_runs_blob    BLOB,
        updated_at        INTEGER NOT NULL
    ) WITHOUT ROWID",

    "CREATE INDEX IF NOT EXISTS idx_routes_date_dir ON routes(date_dir)",
    "CREATE INDEX IF NOT EXISTS idx_routes_start_ts ON routes(start_ts)",

    "CREATE TABLE IF NOT EXISTS processed_files (
        file      TEXT PRIMARY KEY,
        added_at  INTEGER NOT NULL
    ) WITHOUT ROWID",

    "CREATE TABLE IF NOT EXISTS drive_tags (
        drive_key TEXT NOT NULL,
        tag       TEXT NOT NULL,
        PRIMARY KEY (drive_key, tag)
    ) WITHOUT ROWID",

    "CREATE INDEX IF NOT EXISTS idx_drive_tags_tag ON drive_tags(tag)",
];

/// v2 columns added to `routes` via `ALTER TABLE ADD COLUMN`. All are
/// nullable so pre-v2 rows don't need a synchronous backfill during
/// migrate; the one-shot backfill in Load() fills them afterward.
pub const V2_ROUTE_AGGREGATE_COLUMNS: &[(&str, &str)] = &[
    ("max_speed_mps", "REAL"),
    ("avg_speed_mps", "REAL"),
    ("speed_sample_count", "INTEGER"),
    ("valid_point_count", "INTEGER"),
    ("fsd_engaged_ms", "INTEGER"),
    ("autosteer_engaged_ms", "INTEGER"),
    ("tacc_engaged_ms", "INTEGER"),
    ("fsd_distance_m", "REAL"),
    ("autosteer_distance_m", "REAL"),
    ("tacc_distance_m", "REAL"),
    ("assisted_distance_m", "REAL"),
    ("fsd_disengagements", "INTEGER"),
    ("fsd_accel_pushes", "INTEGER"),
    ("start_lat", "REAL"),
    ("start_lon", "REAL"),
    ("end_lat", "REAL"),
    ("end_lon", "REAL"),
];

/// Bring the DB up to `CURRENT_SCHEMA_VERSION`. Safe on every open —
/// idempotent by construction.
pub fn migrate(conn: &Connection) -> Result<()> {
    for stmt in V1_SCHEMA {
        conn.execute(stmt, [])
            .with_context(|| format!("migrate: applying DDL {:?}", truncate(stmt, 60)))?;
    }

    // v2 upgrade: add aggregate columns to existing v1 routes tables.
    // Check column presence rather than parsing schema_version to stay
    // robust against DBs restored from future-version backups.
    let existing = list_route_columns(conn)?;
    for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS {
        if existing.contains(*name) {
            continue;
        }
        let sql = format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ);
        conn.execute(&sql, [])
            .with_context(|| format!("migrate: adding routes.{}", name))?;
    }

    // schema_version handling:
    //   * first-ever migrate: seed to CURRENT_SCHEMA_VERSION.
    //   * upgrading from an older version: bump up to current.
    //   * downgrades (future-version marker): preserve — never clobber
    //     a marker we don't understand.
    match meta_get(conn, "schema_version")? {
        None => {
            meta_set(conn, "schema_version", &CURRENT_SCHEMA_VERSION.to_string())?;
        }
        Some(cur) => {
            if stored_less_than(&cur, CURRENT_SCHEMA_VERSION) {
                meta_set(conn, "schema_version", &CURRENT_SCHEMA_VERSION.to_string())?;
            }
        }
    }

    // Record creation time on the first migrate only.
    if meta_get(conn, "created_at")?.is_none() {
        let now = chrono::Utc::now().to_rfc3339();
        meta_set(conn, "created_at", &now)?;
    }

    Ok(())
}

/// Return the set of column names present on the `routes` table.
fn list_route_columns(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('routes')")?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<std::collections::HashSet<String>, _>>()?;
    Ok(cols)
}

/// Read a value from `meta`. Returns `None` when the key doesn't exist.
pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let v = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(v)
}

/// Upsert a `meta` key/value pair.
pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// True when the string-encoded schema_version is numerically less than
/// `target`. Non-numeric values (corrupted meta) are treated as "older"
/// so migrate() gets a chance to heal them.
fn stored_less_than(stored: &str, target: i32) -> bool {
    let s = stored.trim();
    if s.is_empty() {
        return true;
    }
    match s.parse::<i32>() {
        Ok(n) => n < target,
        Err(_) => true,
    }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode=MEMORY;").unwrap();
        conn
    }

    #[test]
    fn migrate_from_empty_sets_schema_version() {
        let conn = open();
        migrate(&conn).unwrap();
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("2"),
        );
        assert!(meta_get(&conn, "created_at").unwrap().is_some());
    }

    #[test]
    fn migrate_idempotent() {
        let conn = open();
        migrate(&conn).unwrap();
        let t1 = meta_get(&conn, "created_at").unwrap();
        migrate(&conn).unwrap();
        let t2 = meta_get(&conn, "created_at").unwrap();
        assert_eq!(t1, t2, "created_at must be stable across re-migrations");
    }

    #[test]
    fn migrate_from_v1_adds_aggregate_columns() {
        let conn = open();
        // Simulate a v1 DB: apply v1 DDL only, no v2 columns, schema_version = 1.
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        meta_set(&conn, "schema_version", "1").unwrap();
        migrate(&conn).unwrap();
        let existing = list_route_columns(&conn).unwrap();
        for (name, _) in V2_ROUTE_AGGREGATE_COLUMNS {
            assert!(existing.contains(*name), "v2 column {} missing after migrate", name);
        }
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("2")
        );
    }

    #[test]
    fn migrate_preserves_future_version_marker() {
        let conn = open();
        migrate(&conn).unwrap();
        meta_set(&conn, "schema_version", "99").unwrap();
        migrate(&conn).unwrap();
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("99"),
            "future-version marker must not be clobbered"
        );
    }

    #[test]
    fn stored_less_than_handles_corrupted_values() {
        assert!(stored_less_than("", 2));
        assert!(stored_less_than("garbage", 2));
        assert!(stored_less_than("1", 2));
        assert!(!stored_less_than("2", 2));
        assert!(!stored_less_than("99", 2));
    }
}
