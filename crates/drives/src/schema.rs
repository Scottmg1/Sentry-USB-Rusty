//! SQLite schema + migrations.
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
///
/// v4 -> v5: data-only cleanup. Pre-v5 scans wrote rows for
/// `SavedClips/...` and `SentryClips/...` clips that produce spurious
/// "drives" (parked Sentry recordings) and duplicates of RecentClips
/// data. v5 deletes those rows from `routes` and `processed_files`.
/// scan_dir + grouper now refuse to add them going forward.
///
/// v5 -> v6: add the `telemetry_samples` table (BLE-sampled vehicle
/// state at arbitrary timestamps) and per-route aggregate columns
/// summarizing the samples that fall within each drive's start_ts /
/// end_ts window. Samples are written by the tesla_telemetry daemon
/// independently of drive discovery; the aggregator joins them in
/// during `compute_route_aggregates`. Unmatched samples are kept.
///
/// v6 -> v7: add TPMS (tire pressure) columns to both
/// `telemetry_samples` and `routes`. Tesla exposes 4 tire pressures
/// via `state tire-pressure` in PSI. Per-route columns store the
/// latest non-null reading per tire within each clip's 60s window;
/// per-drive rollup takes the latest across the drive's clips.
/// All nullable â€” cars without TPMS or pre-TPMS-sampler drives
/// simply stay NULL and the UI hides the row.
///
/// v7 -> v8: TPMS unit fix. v7 sampler stored raw `state
/// tire-pressure` values as `tire_*_psi` but Tesla actually returns
/// BAR â€” typical readings showed up as 3.0 instead of the expected
/// ~43 PSI. v8 multiplies any historically-stored value < 5 by
/// 14.5038 in both `telemetry_samples` and `routes`. The bound is
/// safe because a real PSI reading can't be < 5 on a drivable tire,
/// and a real bar reading can't be > 5 (would be ~73 PSI, double
/// every spec).
///
/// v8 -> v9: add `odometer_mi` and `software_version` to
/// `telemetry_samples`; add `odometer_mi_start` / `odometer_mi_end` /
/// `software_version` to `routes`. Odometer is sampled every cycle
/// (it ticks constantly while driving); software version is sampled
/// at a 15-min throttle so we don't waste BLE air time on a field
/// that changes once per OTA. The per-drive rollup uses
/// `software_version` to map to a FSD release (via a hardcoded
/// lookup) â€” but only displayed on drives where FSD was actually
/// engaged at some point.
///
/// `idx_routes_start_ts` cleanup (version-agnostic): the index over
/// `routes.start_ts` â€” shipped by every V1_SCHEMA from v1 through v9
/// â€” indexed an always-NULL column (start_ts is bound to SQL NULL on
/// every insert). V1_SCHEMA no longer ships it; `migrate()` runs an
/// unconditional `DROP INDEX IF EXISTS` near the top so every cohort
/// (pre-v6 / v6 / v7 / v8 / v9 / fresh) ends up without the dead
/// index in one pass. `IF EXISTS` makes the call a no-op on fresh
/// DBs and on any subsequent open.
///
/// v9 -> v10: add `location_name` (TEXT) to `telemetry_samples` â€”
/// Tesla's reverse-geocoded address string from `state drive`'s
/// `locationState.locationName` field. Per-clip rollups
/// `location_name_start` / `location_name_end` added to `routes`,
/// populated by the aggregator from the first / last non-null
/// sample in each clip's 60s window. The per-drive rollup chains
/// these across clips so each drive in the list gets a friendly
/// start â†’ end label without needing to call an external
/// reverse-geocoding API.
///
/// Also: `software_version` columns on both tables are kept for
/// forward-compat but no longer written â€” Tesla doesn't expose
/// `car_version` over BLE state queries.
/// v14 -> v15: add clip-boundary state columns to `routes`
/// (V15_ROUTE_BOUNDARY_COLUMNS) so FSD disengagements and accel pushes
/// resolve across clip seams in the drive grouper.
///
/// v15 -> v16: add `flag_runs_blob` (per-frame SEI blinker/brake/accel
/// flag RLE in RAW frame space — the summon detector's evidence) and
/// `sei_speed_abs_max` (max |SEI speed| per clip; the locked
/// `max_speed_mps` drops negative Reverse samples, so a reverse-only
/// summon crawl would read 0) to `routes`. Both nullable — rows written
/// before flag extraction stay NULL, which the summon detector reads as
/// "unverifiable, not summon". No backfill: flags can only come from
/// re-extracting the MP4's SEI stream, not from stored deduped arrays.
pub const CURRENT_SCHEMA_VERSION: i32 = 16;

/// v1 DDL. Each statement is idempotent (`IF NOT EXISTS`) so `migrate()`
/// is safe on every startup. Column shapes and names match Go exactly â€”
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
    // Note: idx_routes_start_ts is intentionally NOT created here â€” see
    // the `idx_routes_start_ts cleanup` paragraph above CURRENT_SCHEMA_VERSION
    // for the reasoning. migrate() drops any pre-existing copy
    // unconditionally on every open.

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

/// v3 cloud-uploader bookkeeping. `cloud_uploaded_at` (unix seconds) is
/// NULL until the cloud-uploader successfully posts the route to
/// `POST /api/pi/routes` and the server returns `stored | duplicate`.
/// `cloud_route_id` is the lowercase 64-hex SHA-256 of the route's `file`
/// path, cached so we never re-derive (locks in stability if the path
/// normalization ever changes). Both nullable; backfill is unnecessary
/// since pre-v3 rows simply haven't been considered for upload yet.
pub const V3_ROUTE_CLOUD_COLUMNS: &[(&str, &str)] = &[
    ("cloud_uploaded_at", "INTEGER"),
    ("cloud_route_id", "TEXT"),
    // base64 of the wrappedRouteKey produced at upload time. The Pi
    // forgets the raw routeKey immediately after encryption, but cloud
    // tag sync needs to re-derive it to encrypt the tags ciphertext —
    // caching the wrapped form (decryptable with the
    // Pi's own piKey) avoids a cloud round-trip per push. NULL for
    // routes uploaded before this column existed; sync backfills via
    // POST /api/pi/sync/route-keys.
    ("cloud_wrapped_route_key", "TEXT"),
];

/// Partial index on `cloud_uploaded_at IS NULL` rows only â€” keeps the
/// steady-state size near zero (uploaded rows aren't indexed). Drives the
/// uploader's `SELECT file FROM routes WHERE cloud_uploaded_at IS NULL`
/// hot path.
const V3_CLOUD_PENDING_INDEX: &str =
    "CREATE INDEX IF NOT EXISTS idx_routes_cloud_pending \
     ON routes(cloud_uploaded_at) WHERE cloud_uploaded_at IS NULL";

/// v4 Tessie provenance columns. Preserves `source`, `externalSignature`,
/// and `tessieAutopilotPercent` through SQLite on import/export so a
/// round-trip with Sentry-Drive's `drive-data.json` is lossless.
pub const V4_ROUTE_TESSIE_COLUMNS: &[(&str, &str)] = &[
    ("source", "TEXT"),
    ("external_signature", "TEXT"),
    ("tessie_autopilot_percent", "REAL"),
];

/// v6 telemetry rollups on `routes`. Populated by the aggregator from
/// `telemetry_samples` rows whose `ts` falls in `[start_ts, end_ts]`.
/// All nullable â€” a drive that ran before telemetry was enabled, or one
/// where the sampler missed every window, simply has NULLs here. The
/// drives-tab UI reads these directly so the hot path never joins the
/// samples table at render time.
pub const V6_ROUTE_TELEMETRY_COLUMNS: &[(&str, &str)] = &[
    ("battery_pct_start", "REAL"),
    ("battery_pct_end", "REAL"),
    ("battery_temp_avg", "REAL"),
    ("interior_temp_min", "REAL"),
    ("interior_temp_max", "REAL"),
    ("exterior_temp_avg", "REAL"),
    ("hvac_runtime_s", "INTEGER"),
];

/// v6 standalone tables. `telemetry_samples` is keyed on `ts` (unix
/// seconds) and uses WITHOUT ROWID so the PK doubles as the storage
/// order â€” range scans on `ts` for the aggregator's per-route joins
/// are then a B-tree slice with no separate index needed. Every
/// telemetry column is nullable because the sampler uses two source
/// paths: `state climate/charge` (full data) and `body-controller-state`
/// (sleep-safe, no temps/HVAC).
const V6_NEW_TABLES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS telemetry_samples (
        ts                INTEGER PRIMARY KEY,
        battery_pct       REAL,
        battery_temp_c    REAL,
        interior_temp_c   REAL,
        exterior_temp_c   REAL,
        hvac_on           INTEGER,
        source            TEXT NOT NULL
    ) WITHOUT ROWID",
];

/// v7 TPMS columns on `telemetry_samples`. Added via ALTER on
/// existing v6 tables and inline for fresh installs via the CREATE
/// in V6_NEW_TABLES (older databases that miss them get caught by
/// the `list_telemetry_columns` check below).
///
/// Values are in PSI (Tesla's native unit). NULL on cars without
/// TPMS or when the sampler skipped the call (sleep mode uses
/// `body-controller-state` which doesn't include tire data).
pub const V7_TELEMETRY_TPMS_COLUMNS: &[(&str, &str)] = &[
    ("tire_fl_psi", "REAL"),
    ("tire_fr_psi", "REAL"),
    ("tire_rl_psi", "REAL"),
    ("tire_rr_psi", "REAL"),
];

/// v7 TPMS rollup columns on `routes`. Latest non-null reading per
/// tire within the clip's 60s window. Tire pressure changes slowly
/// (minutes-to-hours) so "latest" is a sensible single-value
/// representative â€” drive-level rollup takes the latest across all
/// the drive's clips.
pub const V7_ROUTE_TPMS_COLUMNS: &[(&str, &str)] = &[
    ("tire_fl_psi", "REAL"),
    ("tire_fr_psi", "REAL"),
    ("tire_rl_psi", "REAL"),
    ("tire_rr_psi", "REAL"),
];

/// v9 odometer + software_version on `telemetry_samples`.
/// `odometer_mi` is Tesla's native unit. `software_version` is the
/// Tesla OS version string (e.g. "2026.2.9.10"). The sampler only
/// re-fetches software_version every ~15 min, so most sample rows
/// will have it NULL â€” the per-route aggregator picks the latest
/// non-null in each window.
pub const V9_TELEMETRY_COLUMNS: &[(&str, &str)] = &[
    ("odometer_mi", "REAL"),
    ("software_version", "TEXT"),
];

/// v10 `location_name` column on `telemetry_samples` â€” Tesla's
/// reverse-geocoded address from `state drive`. Free human-readable
/// drive start/end labels without needing to call a geocoder.
pub const V10_TELEMETRY_COLUMNS: &[(&str, &str)] = &[
    ("location_name", "TEXT"),
];

/// v11 charging-detail columns on `telemetry_samples`. Decoded from the
/// BLE `ChargeState` message and written only when the experimental
/// sampler flag is on (SENTRYUSB_EXPERIMENTAL) â€” NULL otherwise, so a
/// normal install is unaffected. Columns are added for everyone (cheap,
/// nullable) so flipping the flag needs no schema change. Powers the
/// charging view under "Driving".
pub const V11_TELEMETRY_CHARGE_COLUMNS: &[(&str, &str)] = &[
    ("charger_power_kw", "INTEGER"),
    ("charger_actual_current_a", "INTEGER"),
    ("charger_voltage_v", "INTEGER"),
    ("charge_rate_mph", "REAL"),
    ("charge_energy_added_kwh", "REAL"),
    ("charge_limit_soc", "INTEGER"),
    ("battery_range_mi", "REAL"),
];

/// v12 raw GPS columns on `telemetry_samples`. The coordinates are
/// already decoded from the bundled `LocationState` (they feed the
/// keep-accessory geofence); persisting them lets a parked-and-charging
/// sample carry the charger's location so the charging view can pin it
/// on a map. Written only when the experimental flag is on â€” NULL
/// otherwise, so a normal install is unaffected.
pub const V12_TELEMETRY_GPS_COLUMNS: &[(&str, &str)] = &[
    ("latitude", "REAL"),
    ("longitude", "REAL"),
];

/// v13 estimated-minutes-to-full column on `telemetry_samples`. The car
/// already reports this in its `ChargeState`; persisting it lets the
/// dashboard "charging" banner show a live time-to-full. Additive +
/// nullable like the rest.
pub const V13_TELEMETRY_MINUTES_COLUMN: &[(&str, &str)] = &[
    ("charge_minutes_to_full", "INTEGER"),
];

/// v14 persisted Tesla charge-phase string (lowercase: "charging",
/// "starting", "calibrating", "stopped", "complete", "disconnected",
/// "nopower", "unknown"). The phase enum was previously in-memory only,
/// forcing /api/charging/current to infer "charging" from a short
/// freshness window on power_kw — which the sampler's multi-minute BLE
/// dropouts defeat (latest sample goes stale mid-charge → banner hides).
/// Persisting the phase lets the API keep the banner up the whole charge
/// and only drop it when a poll actually reports a stopped phase.
/// Additive + nullable.
pub const V14_TELEMETRY_CHARGE_PHASE_COLUMN: &[(&str, &str)] = &[
    ("charging_state", "TEXT"),
];

/// v10 per-clip location-name rollups on `routes`. Populated by
/// the aggregator from the first / last non-null sample in the
/// clip's 60s window.
pub const V10_ROUTE_COLUMNS: &[(&str, &str)] = &[
    ("location_name_start", "TEXT"),
    ("location_name_end", "TEXT"),
];

/// v15 clip-boundary state on `routes` (see `RouteAggregates`): lets the
/// drive grouper resolve FSD disengagements and accel pushes across the
/// 60s clip seams instead of each clip guessing in isolation — mirrors
/// the cloud summary v4 `dPnd`/`pk1` boundary fields.
pub const V15_ROUTE_BOUNDARY_COLUMNS: &[(&str, &str)] = &[
    ("fsd_pend_ms_end", "REAL"),
    ("park_ms_start", "REAL"),
    ("fsd_at_end", "INTEGER"),
    ("fsd_accel_pushes_early", "INTEGER"),
    ("ap_at_start", "INTEGER"),
];

/// v16 summon-evidence columns on `routes` (see `CURRENT_SCHEMA_VERSION`
/// docs). `flag_runs_blob` follows the `gear_runs_blob` wire shape
/// (1-byte flags + LE i32 frames per run).
pub const V16_ROUTE_FLAG_COLUMNS: &[(&str, &str)] = &[
    ("flag_runs_blob", "BLOB"),
    ("sei_speed_abs_max", "REAL"),
];

/// v9 rollups on `routes`. Odometer start/end let the UI show a
/// per-trip mileage delta that's more accurate than GPS distance
/// (GPS over-estimates curves, drops in tunnels, can drift).
/// `software_version` rides through so the FSD-version mapping can
/// happen at display time per drive.
pub const V9_ROUTE_COLUMNS: &[(&str, &str)] = &[
    ("odometer_mi_start", "REAL"),
    ("odometer_mi_end", "REAL"),
    ("software_version", "TEXT"),
];

/// Charging-session tags. Mirrors `drive_tags` but keyed on the charge
/// session id: its start timestamp in unix seconds, the stable id the
/// /api/charging endpoints group sessions by. Standalone table created
/// idempotently on every open, so it needs no `CURRENT_SCHEMA_VERSION`
/// bump: presence is guaranteed by the `IF NOT EXISTS` itself, and it is
/// unrelated to the route cache the version key invalidates.
pub const CHARGE_TAGS_TABLE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS charge_tags (
        session_ts INTEGER NOT NULL,
        tag        TEXT NOT NULL,
        PRIMARY KEY (session_ts, tag)
    ) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS idx_charge_tags_tag ON charge_tags(tag)",
];

/// Manual per-charge cost overrides — a user-entered total (e.g. from a
/// Supercharger receipt) that overrides the tag/rate-derived cost. Mirrors
/// `charge_tags`: keyed on the charge session id (its start timestamp in
/// unix seconds), a standalone table created idempotently on every open,
/// so it needs no `CURRENT_SCHEMA_VERSION` bump — presence is guaranteed
/// by `IF NOT EXISTS` and it's unrelated to the route cache the version
/// key invalidates. The currency it was entered in rides along so the
/// shown value stays stable if the default currency pref later changes.
pub const CHARGE_COSTS_TABLE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS charge_costs (
        session_ts INTEGER PRIMARY KEY,
        amount     REAL NOT NULL,
        currency   TEXT
    ) WITHOUT ROWID",
];

/// Cloud charge-upload bookkeeping. One row per uploaded session,
/// keyed by the session's start timestamp
/// (the same stable id charge_tags/charge_costs use). `cloud_charge_id`
/// is the hex64 chargeId; `wrapped_charge_key` is the base64
/// wrappedChargeKey cached for mutable-envelope sync (same rationale as
/// routes.cloud_wrapped_route_key). `uploaded_at` of -1 is the permanent-
/// skip sentinel (rejected_too_large), mirroring routes. Standalone
/// idempotent table — no CURRENT_SCHEMA_VERSION bump needed.
pub const CHARGE_UPLOADS_TABLE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS charge_uploads (
        session_ts         INTEGER PRIMARY KEY,
        cloud_charge_id    TEXT NOT NULL,
        wrapped_charge_key TEXT NOT NULL,
        uploaded_at        INTEGER NOT NULL
    ) WITHOUT ROWID",
    "CREATE INDEX IF NOT EXISTS idx_charge_uploads_cloud_id
     ON charge_uploads(cloud_charge_id)",
];

/// Dirty queue for two-way cloud sync. A row per locally-changed
/// mutable item awaiting push to the cloud:
///   kind 'drive'  — key = drive_tags.drive_key (start_time string)
///   kind 'charge' — key = charge session_ts (base-10 string); covers
///                   both tags and the cost override (one envelope)
///   kind 'rate'   — key = '' (the per-Pi rate config document)
/// `changed_at` (unix ms) is the local edit time, used for last-writer-
/// wins against the server. Rows are deleted after a successful push
/// (only if changed_at is unchanged — a newer edit re-queues). Writes
/// from sync PULL application bypass this table (no echo loop).
pub const MUTABLE_DIRTY_TABLE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS mutable_dirty (
        kind       TEXT NOT NULL,
        key        TEXT NOT NULL,
        changed_at INTEGER NOT NULL,
        PRIMARY KEY (kind, key)
    ) WITHOUT ROWID",
];

/// Bring the DB up to `CURRENT_SCHEMA_VERSION`. Safe on every open â€”
/// idempotent by construction.
pub fn migrate(conn: &Connection) -> Result<()> {
    for stmt in V1_SCHEMA {
        conn.execute(stmt, [])
            .with_context(|| format!("migrate: applying DDL {:?}", truncate(stmt, 60)))?;
    }

    // Drop the legacy `idx_routes_start_ts` index that every V1_SCHEMA
    // through v9 shipped. `routes.start_ts` has only ever been written
    // NULL (see `db.rs::insert_or_update_route` â€” start_ts is bound to
    // SQL NULL), so the index has nothing to index but charges B-tree
    // maintenance on every insert. Unconditional + `IF EXISTS` handles
    // every cohort in one shot: fresh DBs (no-op, V1_SCHEMA no longer
    // ships the CREATE), and every upgraded DB (pre-v6 / v6 / v7 / v8
    // / v9 â€” all inherited the index from the old V1_SCHEMA). The
    // `routes.start_ts` column itself stays.
    conn.execute("DROP INDEX IF EXISTS idx_routes_start_ts", [])?;

    // v6 standalone tables. Idempotent (`IF NOT EXISTS`) so safe on
    // every open and on first-run alongside V1_SCHEMA.
    for stmt in V6_NEW_TABLES {
        conn.execute(stmt, [])
            .with_context(|| format!("migrate: applying v6 DDL {:?}", truncate(stmt, 60)))?;
    }

    // Charging-session tags. Idempotent standalone table; see the
    // CHARGE_TAGS_TABLE const docs for why it rides outside the version
    // gate.
    for stmt in CHARGE_TAGS_TABLE {
        conn.execute(stmt, []).with_context(|| {
            format!("migrate: applying charge_tags DDL {:?}", truncate(stmt, 60))
        })?;
    }

    // Manual per-charge cost overrides. Same idempotent standalone-table
    // treatment as charge_tags; see CHARGE_COSTS_TABLE docs.
    for stmt in CHARGE_COSTS_TABLE {
        conn.execute(stmt, []).with_context(|| {
            format!("migrate: applying charge_costs DDL {:?}", truncate(stmt, 60))
        })?;
    }

    // Cloud charge-upload bookkeeping + mutable-sync dirty queue. Same
    // idempotent standalone-table treatment; see the const docs.
    for stmt in CHARGE_UPLOADS_TABLE.iter().chain(MUTABLE_DIRTY_TABLE.iter()) {
        conn.execute(stmt, []).with_context(|| {
            format!("migrate: applying cloud-sync DDL {:?}", truncate(stmt, 60))
        })?;
    }

    // v2/v3/v4/v6/v7 upgrade: add columns to existing routes tables.
    // Check column presence rather than parsing schema_version to stay
    // robust against DBs restored from future-version backups.
    let existing = list_route_columns(conn)?;
    for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
        .iter()
        .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
        .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
        .chain(V6_ROUTE_TELEMETRY_COLUMNS.iter())
        .chain(V7_ROUTE_TPMS_COLUMNS.iter())
        .chain(V9_ROUTE_COLUMNS.iter())
        .chain(V10_ROUTE_COLUMNS.iter())
        .chain(V15_ROUTE_BOUNDARY_COLUMNS.iter())
        .chain(V16_ROUTE_FLAG_COLUMNS.iter())
    {
        if existing.contains(*name) {
            continue;
        }
        let sql = format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ);
        conn.execute(&sql, [])
            .with_context(|| format!("migrate: adding routes.{}", name))?;
    }

    // v7 upgrade: add TPMS columns to existing telemetry_samples
    // tables. Fresh DBs land via V6_NEW_TABLES which doesn't include
    // them â€” caught here on the first migrate after the v7 bump.
    let existing_tele = list_telemetry_columns(conn)?;
    for (name, typ) in V7_TELEMETRY_TPMS_COLUMNS
        .iter()
        .chain(V9_TELEMETRY_COLUMNS.iter())
        .chain(V10_TELEMETRY_COLUMNS.iter())
        .chain(V11_TELEMETRY_CHARGE_COLUMNS.iter())
        .chain(V12_TELEMETRY_GPS_COLUMNS.iter())
        .chain(V13_TELEMETRY_MINUTES_COLUMN.iter())
        .chain(V14_TELEMETRY_CHARGE_PHASE_COLUMN.iter())
    {
        if existing_tele.contains(*name) {
            continue;
        }
        let sql = format!("ALTER TABLE telemetry_samples ADD COLUMN {} {}", name, typ);
        conn.execute(&sql, [])
            .with_context(|| format!("migrate: adding telemetry_samples.{}", name))?;
    }

    // v3 partial index. Idempotent.
    conn.execute(V3_CLOUD_PENDING_INDEX, [])
        .context("migrate: creating idx_routes_cloud_pending")?;

    // v5 data cleanup: purge SavedClips/SentryClips routes that pre-v5
    // scans wrote. Gated on the stored schema_version so we only pay the
    // table-scan cost during the one upgrade-to-v5 open. Fresh DBs
    // (schema_version = None) have no rows to delete and skip the work.
    // NOTE: event rows can now be legitimate gap-fills
    // (processor::scan_event_gap_fill) — future cleanups must not
    // blanket-delete SavedClips/SentryClips rows.
    let stored_version_for_v5 = meta_get(conn, "schema_version")?;
    let needs_v5_cleanup = matches!(
        stored_version_for_v5.as_deref(),
        Some(v) if stored_less_than(v, 5),
    );
    if needs_v5_cleanup {
        let deleted_routes = conn
            .execute(
                "DELETE FROM routes WHERE file LIKE 'SavedClips/%' OR file LIKE 'SentryClips/%'",
                [],
            )
            .context("migrate v5: purging event-folder routes")?;
        let deleted_processed = conn
            .execute(
                "DELETE FROM processed_files WHERE file LIKE 'SavedClips/%' OR file LIKE 'SentryClips/%'",
                [],
            )
            .context("migrate v5: purging event-folder processed_files")?;
        if deleted_routes > 0 || deleted_processed > 0 {
            tracing::info!(
                "schema v5: purged {} route(s) and {} processed_files row(s) from SavedClips/SentryClips",
                deleted_routes,
                deleted_processed,
            );
        }
    }

    // v7 -> v8: scrub TPMS values that were stored in BAR rather
    // than PSI by the v7 sampler. Idempotent + gated on a stored
    // version < 8 marker â€” if the column is already in PSI (>= 5)
    // the WHERE clause matches nothing and the UPDATE is a no-op
    // anyway, but the version gate keeps us from re-running on
    // every open in steady state.
    let stored_version_for_v8 = meta_get(conn, "schema_version")?;
    let needs_v8_fix = matches!(
        stored_version_for_v8.as_deref(),
        Some(v) if stored_less_than(v, 8),
    );
    if needs_v8_fix {
        let bar_to_psi_sql = |table: &str, col: &str| -> String {
            format!(
                "UPDATE {table} SET {col} = ROUND({col} * 14.5038, 1) \
                 WHERE {col} IS NOT NULL AND {col} < 5"
            )
        };
        for col in ["tire_fl_psi", "tire_fr_psi", "tire_rl_psi", "tire_rr_psi"] {
            let _ = conn.execute(&bar_to_psi_sql("telemetry_samples", col), []);
            let _ = conn.execute(&bar_to_psi_sql("routes", col), []);
        }
    }

    // schema_version handling:
    //   * first-ever migrate: seed to CURRENT_SCHEMA_VERSION.
    //   * upgrading from an older version: bump up to current.
    //   * downgrades (future-version marker): preserve â€” never clobber
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

/// Return the set of column names present on the `telemetry_samples`
/// table. Returns empty when the table doesn't exist yet (older DB
/// caught mid-migration); the v7 ALTER loop then no-ops harmlessly.
fn list_telemetry_columns(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt =
        conn.prepare("SELECT name FROM pragma_table_info('telemetry_samples')")?;
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
            Some("16"),
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
    fn migrate_from_v1_adds_all_later_columns() {
        let conn = open();
        // Simulate a v1 DB: apply v1 DDL only, schema_version = 1.
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        meta_set(&conn, "schema_version", "1").unwrap();
        migrate(&conn).unwrap();
        let existing = list_route_columns(&conn).unwrap();
        for (name, _) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
            .chain(V6_ROUTE_TELEMETRY_COLUMNS.iter())
            .chain(V7_ROUTE_TPMS_COLUMNS.iter())
            .chain(V9_ROUTE_COLUMNS.iter())
            .chain(V10_ROUTE_COLUMNS.iter())
        {
            assert!(existing.contains(*name), "column {} missing after migrate", name);
        }
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_from_v2_adds_v3_and_v4_columns() {
        let conn = open();
        // Simulate a v2 DB: v1 DDL + v2 columns + schema_version = 2.
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS {
            let sql = format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ);
            conn.execute(&sql, []).unwrap();
        }
        meta_set(&conn, "schema_version", "2").unwrap();

        migrate(&conn).unwrap();

        let existing = list_route_columns(&conn).unwrap();
        for (name, _) in V3_ROUTE_CLOUD_COLUMNS
            .iter()
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
            .chain(V6_ROUTE_TELEMETRY_COLUMNS.iter())
            .chain(V7_ROUTE_TPMS_COLUMNS.iter())
            .chain(V9_ROUTE_COLUMNS.iter())
            .chain(V10_ROUTE_COLUMNS.iter())
        {
            assert!(existing.contains(*name), "column {} missing", name);
        }
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_from_v3_adds_v4_tessie_columns() {
        let conn = open();
        // Simulate a v3 DB.
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
        {
            let sql = format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ);
            conn.execute(&sql, []).unwrap();
        }
        meta_set(&conn, "schema_version", "3").unwrap();

        migrate(&conn).unwrap();

        let existing = list_route_columns(&conn).unwrap();
        for (name, _) in V4_ROUTE_TESSIE_COLUMNS
            .iter()
            .chain(V6_ROUTE_TELEMETRY_COLUMNS.iter())
            .chain(V7_ROUTE_TPMS_COLUMNS.iter())
            .chain(V9_ROUTE_COLUMNS.iter())
            .chain(V10_ROUTE_COLUMNS.iter())
        {
            assert!(existing.contains(*name), "column {} missing", name);
        }
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn v3_partial_index_exists_after_migrate() {
        let conn = open();
        migrate(&conn).unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_routes_cloud_pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "v3 partial index must be created by migrate()");
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
        assert!(stored_less_than("", 4));
        assert!(stored_less_than("garbage", 4));
        assert!(stored_less_than("1", 4));
        assert!(stored_less_than("3", 4));
        assert!(!stored_less_than("4", 4));
        assert!(!stored_less_than("99", 4));
    }

    /// Seed `routes` and `processed_files` with a row from each category.
    /// Returns the count of each category present in `routes`.
    fn seed_three_categories(conn: &Connection) {
        for file in [
            "RecentClips/2026-05-17/2026-05-17_18-47-34-front.mp4",
            "SavedClips/2026-05-17_18-47-59/2026-05-17_18-47-34-front.mp4",
            "SentryClips/2026-05-17_18-46-39/2026-05-17_18-35-39-front.mp4",
        ] {
            conn.execute(
                "INSERT INTO routes (file, date_dir, points_blob, updated_at) VALUES (?1, ?2, X'', 0)",
                params![file, "RecentClips"],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO processed_files (file, added_at) VALUES (?1, 0)",
                params![file],
            )
            .unwrap();
        }
    }

    fn count_routes(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM routes", [], |row| row.get(0))
            .unwrap()
    }

    fn count_processed(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM processed_files", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn migrate_v5_purges_event_folder_rows() {
        let conn = open();
        // Stand up a v4 DB with three seed rows (one per category).
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
        {
            conn.execute(&format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ), [])
                .unwrap();
        }
        meta_set(&conn, "schema_version", "4").unwrap();
        seed_three_categories(&conn);
        assert_eq!(count_routes(&conn), 3);
        assert_eq!(count_processed(&conn), 3);

        migrate(&conn).unwrap();

        // Only the RecentClips row survives in both tables.
        assert_eq!(count_routes(&conn), 1, "expected only RecentClips route to remain");
        assert_eq!(count_processed(&conn), 1, "expected only RecentClips processed_files row");
        let surviving_route: String = conn
            .query_row("SELECT file FROM routes", [], |row| row.get(0))
            .unwrap();
        assert!(surviving_route.starts_with("RecentClips/"));
        let surviving_processed: String = conn
            .query_row("SELECT file FROM processed_files", [], |row| row.get(0))
            .unwrap();
        assert!(surviving_processed.starts_with("RecentClips/"));
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_v5_is_idempotent() {
        let conn = open();
        migrate(&conn).unwrap();
        // After the first migrate, schema_version is "5", so a second
        // migrate must NOT re-run the cleanup. Seed an event-folder row
        // AFTER the version is set, and confirm the second migrate leaves
        // it alone â€” proves the cleanup is gated on schema_version.
        conn.execute(
            "INSERT INTO routes (file, date_dir, points_blob, updated_at) VALUES (?1, ?2, X'', 0)",
            params!["SavedClips/x/y-front.mp4", "SavedClips"],
        )
        .unwrap();
        migrate(&conn).unwrap();
        assert_eq!(count_routes(&conn), 1, "v5 cleanup must not re-run on a v5 DB");
    }

    #[test]
    fn migrate_v5_skips_cleanup_on_fresh_db() {
        // A fresh DB (no stored schema_version) shouldn't even attempt
        // the DELETE â€” there's nothing to clean. Verify by inserting an
        // event-folder row after we manually create the schema but before
        // calling migrate, and observe that the row survives because
        // schema_version is None on entry.
        let conn = open();
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
        {
            conn.execute(&format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ), [])
                .unwrap();
        }
        conn.execute(
            "INSERT INTO routes (file, date_dir, points_blob, updated_at) VALUES (?1, ?2, X'', 0)",
            params!["SavedClips/x/y-front.mp4", "SavedClips"],
        )
        .unwrap();
        assert_eq!(meta_get(&conn, "schema_version").unwrap(), None);
        migrate(&conn).unwrap();
        // Fresh-DB seed path: v5 cleanup skipped, version stamped at 6.
        assert_eq!(count_routes(&conn), 1, "fresh-DB seed must not run v5 cleanup");
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_creates_telemetry_samples_table() {
        let conn = open();
        migrate(&conn).unwrap();
        let exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='telemetry_samples'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "telemetry_samples table must be created by migrate()");
    }

    #[test]
    fn migrate_from_v5_adds_v6_telemetry_columns() {
        // Stand up a v5 DB (everything but v6) and confirm migrate adds
        // both the routes columns and the standalone telemetry_samples
        // table.
        let conn = open();
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
        {
            conn.execute(&format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ), [])
                .unwrap();
        }
        meta_set(&conn, "schema_version", "5").unwrap();

        migrate(&conn).unwrap();

        let existing = list_route_columns(&conn).unwrap();
        for (name, _) in V6_ROUTE_TELEMETRY_COLUMNS {
            assert!(existing.contains(*name), "v6 column {} missing", name);
        }
        let table_exists: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='telemetry_samples'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_exists, 1, "v6 must create telemetry_samples");
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_from_v14_adds_v15_boundary_columns() {
        // Stand up a v14 DB (everything but the v15 boundary columns) and
        // confirm migrate adds them.
        let conn = open();
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for stmt in V6_NEW_TABLES {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
            .chain(V6_ROUTE_TELEMETRY_COLUMNS.iter())
            .chain(V7_ROUTE_TPMS_COLUMNS.iter())
            .chain(V9_ROUTE_COLUMNS.iter())
            .chain(V10_ROUTE_COLUMNS.iter())
        {
            conn.execute(&format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ), [])
                .unwrap();
        }
        meta_set(&conn, "schema_version", "14").unwrap();

        migrate(&conn).unwrap();

        let cols = list_route_columns(&conn).unwrap();
        for (name, _) in V15_ROUTE_BOUNDARY_COLUMNS {
            assert!(cols.contains(*name), "routes.{} missing after v15", name);
        }
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_from_v15_adds_v16_flag_columns() {
        // Stand up a v15 DB (everything but the v16 flag columns) and
        // confirm migrate adds them.
        let conn = open();
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for stmt in V6_NEW_TABLES {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
            .chain(V6_ROUTE_TELEMETRY_COLUMNS.iter())
            .chain(V7_ROUTE_TPMS_COLUMNS.iter())
            .chain(V9_ROUTE_COLUMNS.iter())
            .chain(V10_ROUTE_COLUMNS.iter())
            .chain(V15_ROUTE_BOUNDARY_COLUMNS.iter())
        {
            conn.execute(&format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ), [])
                .unwrap();
        }
        meta_set(&conn, "schema_version", "15").unwrap();

        migrate(&conn).unwrap();

        let cols = list_route_columns(&conn).unwrap();
        for (name, _) in V16_ROUTE_FLAG_COLUMNS {
            assert!(cols.contains(*name), "routes.{} missing after v16", name);
        }
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_from_v6_adds_v7_tpms_columns() {
        // Stand up a v6 DB (everything but v7's tpms cols) and
        // confirm migrate adds them to BOTH routes and
        // telemetry_samples.
        let conn = open();
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        for stmt in V6_NEW_TABLES {
            conn.execute(stmt, []).unwrap();
        }
        for (name, typ) in V2_ROUTE_AGGREGATE_COLUMNS
            .iter()
            .chain(V3_ROUTE_CLOUD_COLUMNS.iter())
            .chain(V4_ROUTE_TESSIE_COLUMNS.iter())
            .chain(V6_ROUTE_TELEMETRY_COLUMNS.iter())
        {
            conn.execute(&format!("ALTER TABLE routes ADD COLUMN {} {}", name, typ), [])
                .unwrap();
        }
        meta_set(&conn, "schema_version", "6").unwrap();

        migrate(&conn).unwrap();

        let route_cols = list_route_columns(&conn).unwrap();
        for (name, _) in V7_ROUTE_TPMS_COLUMNS {
            assert!(route_cols.contains(*name), "routes.{} missing after v7", name);
        }
        let tele_cols = list_telemetry_columns(&conn).unwrap();
        for (name, _) in V7_TELEMETRY_TPMS_COLUMNS {
            assert!(
                tele_cols.contains(*name),
                "telemetry_samples.{} missing after v7",
                name,
            );
        }
        assert_eq!(
            meta_get(&conn, "schema_version").unwrap().as_deref(),
            Some("16")
        );
    }

    #[test]
    fn migrate_v8_converts_bar_tpms_to_psi() {
        // Stand up a v7 DB with mixed-unit tpms values: some still in
        // BAR (the bug â€” values < 5) and some that happen to be in
        // PSI already. v8 should convert the bar values and leave
        // the PSI ones alone.
        let conn = open();
        migrate(&conn).unwrap();
        // Pretend the DB came from v7 by rolling the marker back.
        meta_set(&conn, "schema_version", "7").unwrap();

        // Bar (typical: 3.0 = 43.5 PSI) â€” should be converted.
        conn.execute(
            "INSERT INTO telemetry_samples \
             (ts, source, tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![1_700_000_000_i64, "state", 3.0_f64, 3.1_f64, 2.9_f64, 3.0_f64],
        )
        .unwrap();
        // Already-correct PSI â€” should pass through untouched.
        conn.execute(
            "INSERT INTO telemetry_samples \
             (ts, source, tire_fl_psi, tire_fr_psi, tire_rl_psi, tire_rr_psi) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![1_700_000_100_i64, "state", 42.0_f64, 43.0_f64, 41.5_f64, 42.5_f64],
        )
        .unwrap();
        // Mixed NULL row â€” should stay all NULL.
        conn.execute(
            "INSERT INTO telemetry_samples (ts, source) VALUES (?1, ?2)",
            params![1_700_000_200_i64, "body_controller"],
        )
        .unwrap();

        migrate(&conn).unwrap();

        let converted: f64 = conn
            .query_row(
                "SELECT tire_fl_psi FROM telemetry_samples WHERE ts = 1700000000",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            (converted - 43.5).abs() < 0.1,
            "3.0 bar should become ~43.5 PSI, got {}",
            converted,
        );
        let untouched: f64 = conn
            .query_row(
                "SELECT tire_fl_psi FROM telemetry_samples WHERE ts = 1700000100",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(untouched, 42.0, "already-PSI values must not be re-converted");
        let null: Option<f64> = conn
            .query_row(
                "SELECT tire_fl_psi FROM telemetry_samples WHERE ts = 1700000200",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(null.is_none(), "null tire values must stay null");
    }

    #[test]
    fn telemetry_samples_insert_and_range_query_works() {
        let conn = open();
        migrate(&conn).unwrap();

        // Two samples from the full BLE path, one body-controller-only.
        conn.execute(
            "INSERT INTO telemetry_samples \
             (ts, battery_pct, battery_temp_c, interior_temp_c, exterior_temp_c, hvac_on, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![1_700_000_000_i64, 73.0_f64, 18.5_f64, 22.0_f64, 12.5_f64, 0_i64, "state"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO telemetry_samples \
             (ts, battery_pct, battery_temp_c, interior_temp_c, exterior_temp_c, hvac_on, source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![1_700_000_060_i64, 72.5_f64, 18.7_f64, 23.5_f64, 12.4_f64, 1_i64, "state"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO telemetry_samples (ts, source) VALUES (?1, ?2)",
            params![1_700_000_999_i64, "body_controller"],
        )
        .unwrap();

        // Range query mirrors the aggregator's per-drive join.
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM telemetry_samples WHERE ts BETWEEN ?1 AND ?2",
                params![1_700_000_000_i64, 1_700_000_100_i64],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 2, "range query should include both `state` samples");

        // PRIMARY KEY constraint: duplicate ts rejected.
        let dup = conn.execute(
            "INSERT INTO telemetry_samples (ts, source) VALUES (?1, ?2)",
            params![1_700_000_000_i64, "state"],
        );
        assert!(dup.is_err(), "duplicate ts must violate PRIMARY KEY");
    }

    /// Returns 1 if the named index exists in sqlite_master, else 0.
    fn index_exists(conn: &Connection, name: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name = ?1",
            params![name],
            |row| row.get(0),
        )
        .unwrap()
    }

    #[test]
    fn migrate_drops_dead_index_on_upgrade() {
        // Stand up a DB that has the legacy `idx_routes_start_ts` index
        // (as pre-v6 V1_SCHEMA shipped) and confirm migrate() drops it.
        let conn = open();
        for stmt in V1_SCHEMA {
            conn.execute(stmt, []).unwrap();
        }
        // Re-create the legacy index that pre-v6 schemas shipped.
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_routes_start_ts ON routes(start_ts)",
            [],
        )
        .unwrap();
        assert_eq!(index_exists(&conn, "idx_routes_start_ts"), 1);

        migrate(&conn).unwrap();

        assert_eq!(
            index_exists(&conn, "idx_routes_start_ts"),
            0,
            "migrate must drop idx_routes_start_ts",
        );
    }

    #[test]
    fn migrate_drops_dead_index_is_idempotent() {
        // After the first migrate, the index is gone. A second migrate
        // must not error if the index is absent.
        let conn = open();
        migrate(&conn).unwrap();
        assert_eq!(index_exists(&conn, "idx_routes_start_ts"), 0);
        migrate(&conn).unwrap();
        assert_eq!(index_exists(&conn, "idx_routes_start_ts"), 0);
    }

    #[test]
    fn migrate_fresh_db_never_creates_dead_index() {
        // A fresh DB must not ship the dead index. V1_SCHEMA no longer
        // includes the CREATE INDEX, so migrate() on an empty DB leaves
        // sqlite_master without idx_routes_start_ts.
        let conn = open();
        migrate(&conn).unwrap();
        assert_eq!(
            index_exists(&conn, "idx_routes_start_ts"),
            0,
            "fresh DB must not create the legacy index",
        );
        // idx_routes_date_dir is still expected (it indexes a real column).
        assert_eq!(index_exists(&conn, "idx_routes_date_dir"), 1);
    }
}
