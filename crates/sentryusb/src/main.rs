#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use std::sync::Arc;

use clap::Parser;
use tower_http::compression::CompressionLayer;
use tracing::info;

mod embed;
mod state;
mod migrate;

#[derive(Parser)]
#[command(name = "sentryusb", about = "SentryUSB server")]
struct Args {
    /// HTTP server port
    #[arg(short, long, default_value_t = 8788)]
    port: u16,

    /// Development mode (don't serve embedded static files)
    #[arg(long)]
    dev: bool,

    /// Path to static files directory (overrides embedded)
    #[arg(long)]
    r#static: Option<String>,
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sentryusb=info,sentryusb_api=info,sentryusb_drives=info,tower_http=info".into()),
        )
        .init();

    let args = Args::parse();

    info!("SentryUSB server starting on port {}", args.port);

    // Run startup migration in background
    tokio::spawn(async {
        migrate::run_startup_migration().await;
    });

    // Initialize auth
    let auth = sentryusb_api::init_auth();

    // WebSocket hub
    let hub = sentryusb_ws::Hub::new();

    // Drive store (SQLite)
    let db_path = sentryusb_drives::DEFAULT_DB_PATH;
    let store = match sentryusb_drives::DriveStore::open(db_path) {
        Ok(s) => Arc::new(s),
        Err(e) => {
            // Try in-memory if DB path doesn't work (e.g., on dev machine)
            tracing::warn!("Failed to open drive DB at {}: {}. Using in-memory.", db_path, e);
            Arc::new(sentryusb_drives::DriveStore::open_memory().expect("failed to create in-memory DB"))
        }
    };

    // Legacy-JSON migration is now handled automatically inside
    // DriveStore::open via the one-shot import dance (matches Go Store.Load).
    // No manual step needed here — the import marker in the meta table
    // ensures it only runs once across the lifetime of the DB.

    // Drive processor
    let processor = Arc::new(sentryusb_drives::processor::Processor::new(
        store.clone(),
        hub.clone(),
    ));

    let drive_state = sentryusb_api::drives_handler::DriveState {
        store: store.clone(),
        processor: processor.clone(),
        importing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    // Keep-awake manager: busy if archiveloop is archiving OR drive processor
    // is running. Matches Go's isBusy closure (server/api/keepawake.go).
    let is_busy_processor = processor.clone();
    let is_busy: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        sentryusb_api::drives_handler::is_archiving() || is_busy_processor.is_running()
    });
    let keep_awake = sentryusb_api::keep_awake::KeepAwakeManager::new(is_busy);

    let app_state = sentryusb_api::router::AppState {
        hub: hub.clone(),
        auth: auth.clone(),
        drives: drive_state,
        keep_awake,
    };

    // Resume setup if it was interrupted by a reboot (e.g. dwc2 overlay, root shrink)
    sentryusb_api::setup::auto_resume_setup(hub.clone());

    // Announce this device + current version to the telemetry endpoint.
    sentryusb_api::update::spawn_startup_telemetry();

    // Resume Away Mode if the flag file still has time remaining.
    sentryusb_api::away_mode::restore_from_file();

    // Build the API router
    let mut app = sentryusb_api::build_router(app_state.clone());

    // Add compression
    app = app.layer(CompressionLayer::new());

    // Serve TeslaCam video files directly from the cam disk-image mount.
    app = app.nest_service(
        "/TeslaCam",
        tower_http::services::ServeDir::new("/mnt/cam/TeslaCam"),
    );

    // Serve /fs/ for music/lightshow/boombox autofs mounts
    app = app.nest_service(
        "/fs",
        tower_http::services::ServeDir::new("/var/www/html/fs"),
    );

    // Static file serving with SPA fallback (unless dev mode)
    if !args.dev {
        app = app.fallback(embed::spa_handler);
        info!("Serving embedded static files");
    } else {
        info!("Running in development mode (no static file serving)");
    }

    // Auth middleware
    app = app.layer(axum::middleware::from_fn_with_state(
        auth,
        sentryusb_api::auth::auth_middleware,
    ));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], args.port));
    info!("SentryUSB server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind address");

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server error");
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install CTRL+C handler");
    info!("Shutdown signal received, draining connections...");
}
