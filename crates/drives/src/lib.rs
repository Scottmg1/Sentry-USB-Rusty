pub mod types;
pub mod db;
pub mod extract;
pub mod grouper;
pub mod json_compat;
pub mod processor;

pub use db::DriveStore;
pub use types::*;

/// Default path for the SQLite drive database.
pub const DEFAULT_DB_PATH: &str = "/mutable/drive-data.db";

/// Legacy JSON data path (for migration).
pub const LEGACY_JSON_PATH: &str = "/mutable/drive-data.json";

/// Archive path for backup sync.
pub const ARCHIVE_JSON_PATH: &str = "/mnt/archive/drive-data.json";
