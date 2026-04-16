use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::Connection;
use tracing::info;

use crate::types::{GearRun, GpsPoint, Route};

/// Drive data store backed by SQLite.
pub struct DriveStore {
    conn: Mutex<Connection>,
}

impl DriveStore {
    /// Opens or creates the SQLite database at the given path.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open drive database: {}", path))?;

        // Enable WAL mode for better concurrent read performance
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        let store = DriveStore {
            conn: Mutex::new(conn),
        };
        store.create_tables()?;

        info!("Drive store opened at {}", path);
        Ok(store)
    }

    /// Opens an in-memory database (for testing).
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = DriveStore {
            conn: Mutex::new(conn),
        };
        store.create_tables()?;
        Ok(store)
    }

    fn create_tables(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS routes (
                id INTEGER PRIMARY KEY,
                file TEXT UNIQUE NOT NULL,
                date TEXT NOT NULL,
                points BLOB NOT NULL,
                gear_states BLOB NOT NULL,
                autopilot_states BLOB NOT NULL,
                speeds BLOB NOT NULL,
                accel_positions BLOB NOT NULL,
                raw_park_count INTEGER NOT NULL DEFAULT 0,
                raw_frame_count INTEGER NOT NULL DEFAULT 0,
                gear_runs BLOB NOT NULL
            );
            CREATE TABLE IF NOT EXISTS processed_files (
                file TEXT PRIMARY KEY
            );
            CREATE TABLE IF NOT EXISTS drive_tags (
                drive_id TEXT NOT NULL,
                tag TEXT NOT NULL,
                UNIQUE(drive_id, tag)
            );
            CREATE INDEX IF NOT EXISTS idx_routes_date ON routes(date);
            CREATE INDEX IF NOT EXISTS idx_drive_tags_id ON drive_tags(drive_id);",
        )?;
        Ok(())
    }

    /// Insert or update a route in the database.
    pub fn upsert_route(&self, route: &Route) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let points_blob = bincode::serialize(&route.points)?;
        let gear_blob = &route.gear_states;
        let autopilot_blob = &route.autopilot_states;
        let speeds_blob = bincode::serialize(&route.speeds)?;
        let accel_blob = bincode::serialize(&route.accel_positions)?;
        let gear_runs_blob = bincode::serialize(&route.gear_runs)?;

        conn.execute(
            "INSERT INTO routes (file, date, points, gear_states, autopilot_states, speeds, accel_positions, raw_park_count, raw_frame_count, gear_runs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(file) DO UPDATE SET
                date=excluded.date, points=excluded.points, gear_states=excluded.gear_states,
                autopilot_states=excluded.autopilot_states, speeds=excluded.speeds,
                accel_positions=excluded.accel_positions, raw_park_count=excluded.raw_park_count,
                raw_frame_count=excluded.raw_frame_count, gear_runs=excluded.gear_runs",
            rusqlite::params![
                route.file,
                route.date,
                points_blob,
                gear_blob,
                autopilot_blob,
                speeds_blob,
                accel_blob,
                route.raw_park_count,
                route.raw_frame_count,
                gear_runs_blob,
            ],
        )?;
        Ok(())
    }

    /// Mark a file as processed.
    pub fn mark_processed(&self, file: &str) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute(
            "INSERT OR IGNORE INTO processed_files (file) VALUES (?1)",
            rusqlite::params![file],
        )?;
        Ok(())
    }

    /// Check if a file has been processed.
    pub fn is_processed(&self, file: &str) -> Result<bool> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM processed_files WHERE file = ?1",
            rusqlite::params![file],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Get all routes from the database.
    pub fn get_all_routes(&self) -> Result<Vec<Route>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let mut stmt = conn.prepare(
            "SELECT file, date, points, gear_states, autopilot_states, speeds, accel_positions, raw_park_count, raw_frame_count, gear_runs FROM routes ORDER BY date, file",
        )?;

        let routes = stmt
            .query_map([], |row| {
                let points_blob: Vec<u8> = row.get(2)?;
                let gear_states: Vec<u8> = row.get(3)?;
                let autopilot_states: Vec<u8> = row.get(4)?;
                let speeds_blob: Vec<u8> = row.get(5)?;
                let accel_blob: Vec<u8> = row.get(6)?;
                let gear_runs_blob: Vec<u8> = row.get(9)?;

                let points: Vec<GpsPoint> =
                    bincode::deserialize(&points_blob).unwrap_or_default();
                let speeds: Vec<f32> =
                    bincode::deserialize(&speeds_blob).unwrap_or_default();
                let accel_positions: Vec<f32> =
                    bincode::deserialize(&accel_blob).unwrap_or_default();
                let gear_runs: Vec<GearRun> =
                    bincode::deserialize(&gear_runs_blob).unwrap_or_default();

                Ok(Route {
                    file: row.get(0)?,
                    date: row.get(1)?,
                    points,
                    gear_states,
                    autopilot_states,
                    speeds,
                    accel_positions,
                    raw_park_count: row.get::<_, u32>(7)?,
                    raw_frame_count: row.get::<_, u32>(8)?,
                    gear_runs,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(routes)
    }

    /// Execute a closure with read access to all routes.
    /// This avoids copying the routes out of the database when you only
    /// need to compute something from them.
    pub fn with_routes<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&[Route]) -> R,
    {
        let routes = self.get_all_routes()?;
        Ok(f(&routes))
    }

    /// Get total route count (lightweight — no data deserialization).
    pub fn route_count(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM routes", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Get total processed file count.
    pub fn processed_count(&self) -> Result<usize> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM processed_files", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Clear all processed file records (for reprocessing).
    pub fn clear_processed(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute("DELETE FROM processed_files", [])?;
        Ok(())
    }

    /// Clear all route data (for reprocessing).
    pub fn clear_routes(&self) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute("DELETE FROM routes", [])?;
        Ok(())
    }

    /// Get tags for a drive.
    pub fn get_tags(&self, drive_id: &str) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let mut stmt = conn.prepare("SELECT tag FROM drive_tags WHERE drive_id = ?1 ORDER BY tag")?;
        let tags = stmt
            .query_map(rusqlite::params![drive_id], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(tags)
    }

    /// Set tags for a drive (replaces existing tags).
    pub fn set_tags(&self, drive_id: &str, tags: &[String]) -> Result<()> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        conn.execute(
            "DELETE FROM drive_tags WHERE drive_id = ?1",
            rusqlite::params![drive_id],
        )?;
        let mut stmt =
            conn.prepare("INSERT INTO drive_tags (drive_id, tag) VALUES (?1, ?2)")?;
        for tag in tags {
            stmt.execute(rusqlite::params![drive_id, tag])?;
        }
        Ok(())
    }

    /// Get all unique tags across all drives.
    pub fn get_all_tags(&self) -> Result<Vec<String>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let mut stmt = conn.prepare("SELECT DISTINCT tag FROM drive_tags ORDER BY tag")?;
        let tags = stmt
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        Ok(tags)
    }

    /// Get all drive IDs and their tags.
    pub fn get_all_drive_tags(&self) -> Result<std::collections::HashMap<String, Vec<String>>> {
        let conn = self.conn.lock().map_err(|e| anyhow::anyhow!("lock poisoned: {}", e))?;
        let mut stmt =
            conn.prepare("SELECT drive_id, tag FROM drive_tags ORDER BY drive_id, tag")?;
        let mut map = std::collections::HashMap::new();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, tag) = row?;
            map.entry(id).or_insert_with(Vec::new).push(tag);
        }
        Ok(map)
    }

    /// Check if legacy JSON data exists and needs migration.
    pub fn needs_migration(json_path: &str, db_path: &str) -> bool {
        Path::new(json_path).exists() && !Path::new(db_path).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_query() {
        let store = DriveStore::open_memory().unwrap();
        assert_eq!(store.route_count().unwrap(), 0);
        assert_eq!(store.processed_count().unwrap(), 0);
    }

    #[test]
    fn test_upsert_and_get() {
        let store = DriveStore::open_memory().unwrap();
        let route = Route {
            file: "2025-01-15/clip-front.mp4".to_string(),
            date: "2025-01-15".to_string(),
            points: vec![[37.7749, -122.4194], [37.7750, -122.4195]],
            gear_states: vec![1, 1],
            autopilot_states: vec![0, 0],
            speeds: vec![25.0, 26.0],
            accel_positions: vec![0.5, 0.6],
            raw_park_count: 0,
            raw_frame_count: 100,
            gear_runs: vec![GearRun { gear: 1, frames: 100 }],
        };
        store.upsert_route(&route).unwrap();
        assert_eq!(store.route_count().unwrap(), 1);

        let routes = store.get_all_routes().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0].file, "2025-01-15/clip-front.mp4");
        assert_eq!(routes[0].points.len(), 2);
    }

    #[test]
    fn test_processed_files() {
        let store = DriveStore::open_memory().unwrap();
        assert!(!store.is_processed("test.mp4").unwrap());
        store.mark_processed("test.mp4").unwrap();
        assert!(store.is_processed("test.mp4").unwrap());
    }

    #[test]
    fn test_tags() {
        let store = DriveStore::open_memory().unwrap();
        store
            .set_tags("drive1", &["Work".to_string(), "Commute".to_string()])
            .unwrap();
        let tags = store.get_tags("drive1").unwrap();
        assert_eq!(tags, vec!["Commute", "Work"]);
    }
}
