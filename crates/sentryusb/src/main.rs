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

    // Check for legacy JSON migration
    if sentryusb_drives::DriveStore::needs_migration(
        sentryusb_drives::LEGACY_JSON_PATH,
        db_path,
    ) {
        info!("Migrating legacy drive-data.json to SQLite...");
        if let Err(e) = sentryusb_drives::json_compat::import_json(
            sentryusb_drives::LEGACY_JSON_PATH,
            &store,
        ) {
            tracing::warn!("JSON migration failed: {}", e);
        }
    }

    // Drive processor
    let processor = Arc::new(sentryusb_drives::processor::Processor::new(
        store.clone(),
        hub.clone(),
    ));

    let drive_state = sentryusb_api::drives_handler::DriveState {
        store: store.clone(),
        processor,
    };

    let app_state = sentryusb_api::router::AppState {
        hub: hub.clone(),
        auth: auth.clone(),
        drives: drive_state,
    };

    // Build the API router
    let mut app = sentryusb_api::build_router(app_state.clone());

    // Add compression
    app = app.layer(CompressionLayer::new());

    // Serve TeslaCam video files
    app = app.nest_service(
        "/TeslaCam",
        tower_http::services::ServeDir::new("/var/www/html/TeslaCam"),
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
