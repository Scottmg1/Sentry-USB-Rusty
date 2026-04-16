use tracing::info;

/// Run startup migration: update peripheral files if the binary is newer
/// than the last migration. Runs in background so the HTTP server starts
/// immediately.
pub async fn run_startup_migration() {
    // TODO: Port from server/migrate.go
    // For now, just log that we'd run migration
    info!("Startup migration check (not yet implemented)");
}
