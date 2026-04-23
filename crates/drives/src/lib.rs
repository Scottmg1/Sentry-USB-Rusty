pub mod aggregate;
pub mod backfill;
pub mod blob;
pub mod db;
pub mod extract;
pub mod grouper;
pub mod json_compat;
pub mod processor;
pub mod schema;
pub mod syncguard;
pub mod types;

pub use db::DriveStore;
pub use types::*;

/// Default SQLite DB path.
pub const DEFAULT_DB_PATH: &str = db::DEFAULT_DATA_PATH;

/// Legacy JSON path (pre-SQLite).
pub const LEGACY_JSON_PATH: &str = db::LEGACY_JSON_PATH;

/// Archive-side JSON copy.
pub const ARCHIVE_JSON_PATH: &str = db::ARCHIVE_DATA_PATH;
