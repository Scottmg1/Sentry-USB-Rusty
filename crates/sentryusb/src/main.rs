// The system allocator is used so the binary works on every Pi kernel
// regardless of page size (Pi 5 / Bookworm uses 16 KB pages while older
// Pis use 4 KB pages). A page-size-specific allocator like jemalloc
// aborts at startup when its compiled-in page size doesn't match the
// kernel's, which is why we don't use one here.

use std::sync::Arc;

use clap::{Parser, Subcommand};
use tower_http::compression::CompressionLayer;
use tracing::info;

mod embed;
mod state;
mod migrate;

#[derive(Parser)]
#[command(name = "sentryusb", about = "SentryUSB server")]
struct Args {
    /// HTTP server port (only used when no subcommand is given)
    #[arg(short, long, default_value_t = 8788)]
    port: u16,

    /// Development mode (don't serve embedded static files)
    #[arg(long)]
    dev: bool,

    /// Path to static files directory (overrides embedded)
    #[arg(long)]
    r#static: Option<String>,

    /// Optional subcommand. Without one, the HTTP server runs.
    ///
    /// Subcommands are invoked by the `/root/bin/{make,release}_snapshot.sh`,
    /// `enable/disable_gadget.sh`, and `manage_free_space.sh` wrappers
    /// installed by the setup wizard — archiveloop calls those wrappers
    /// every cycle, so keeping the subcommands working here keeps the
    /// archive flow alive.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// USB gadget control (configfs + UDC bind/unbind).
    Gadget {
        #[command(subcommand)]
        action: GadgetAction,
    },
    /// Cam-disk snapshot management (reflink-backed).
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Free-space management on `/backingfiles`.
    Space {
        #[command(subcommand)]
        action: SpaceAction,
    },
}

#[derive(Subcommand)]
enum GadgetAction {
    /// Attach the USB mass-storage gadget + bind the UDC.
    Enable {
        /// Ignored — the shim in `/root/bin/enable_gadget.sh` splats
        /// `"$@"`, so callers may pass through args we don't use.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Unbind the UDC + tear down the configfs hierarchy.
    Disable {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Create a new reflink snapshot of `/backingfiles/cam_disk.bin`.
    Make {
        /// Reserved for future compat (e.g. `nofsck`); ignored for now.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Release (delete) an existing snapshot by name (`snap-NNNNNN`).
    Release {
        /// Snapshot name passed through by the `release_snapshot.sh` wrapper.
        name: String,
    },
}

#[derive(Subcommand)]
enum SpaceAction {
    /// Delete old snapshots until `/backingfiles` has enough free space.
    Manage {
        /// Reserved for future compat (e.g. reserve size); ignored for now.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
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

    // Subcommand dispatch — the wrappers in /root/bin/ expect these to
    // run to completion synchronously and exit with a status code.
    if let Some(cmd) = args.command {
        std::process::exit(run_subcommand(cmd).await);
    }

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

    // Cloud-uploader wake channel. Threaded into Processor so do_process
    // calls notify_one() at the tail of every successful run; the cloud
    // sweep loop is the only subscriber. See ENCRYPTION.md + project plan
    // for the archive-lifecycle hook design.
    let cloud_notify = Arc::new(tokio::sync::Notify::new());

    // Drive processor
    let processor = Arc::new(sentryusb_drives::processor::Processor::with_on_complete(
        store.clone(),
        hub.clone(),
        Some(cloud_notify.clone()),
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
    // Wipe any stale wanted-flag from a crashed prior run so the first
    // awake_stop after boot isn't deferred forever.
    sentryusb_api::drives_handler::clear_keep_awake_wanted();
    let keep_awake = sentryusb_api::keep_awake::KeepAwakeManager::new(is_busy);

    // SentryCloud upload pipeline. Background tasks pull pending routes
    // from the local DB, encrypt under the per-Pi key, and POST to
    // sentryusb.com/api/pi/routes whenever the Notify above fires.
    let cloud_uploader = sentryusb_cloud_uploader::CloudUploader::spawn(
        store.clone(),
        hub.clone(),
        cloud_notify,
    ).await;

    let app_state = sentryusb_api::router::AppState {
        hub: hub.clone(),
        auth: auth.clone(),
        drives: drive_state,
        keep_awake,
        cloud: sentryusb_api::cloud::CloudHandlerState {
            uploader: cloud_uploader,
        },
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

/// Dispatch a subcommand. Returns the exit code the wrapper scripts should
/// propagate back to their caller. `0` on success; `1` (or a shell-friendly
/// non-zero) on failure. Errors are printed to stderr so archiveloop's
/// existing `ERROR: make_snapshot.sh failed (exit $?)` log lines stay useful.
async fn run_subcommand(cmd: Command) -> i32 {
    match cmd {
        Command::Gadget { action } => run_gadget(action).await,
        Command::Snapshot { action } => run_snapshot(action).await,
        Command::Space { action } => run_space(action).await,
    }
}

async fn run_gadget(action: GadgetAction) -> i32 {
    // usb_gadget::enable/disable are synchronous and touch configfs; run
    // them on a blocking thread so they don't panic inside a tokio worker
    // on slow udc bind retries.
    let result = match action {
        GadgetAction::Enable { .. } => {
            tokio::task::spawn_blocking(sentryusb_gadget::enable).await
        }
        GadgetAction::Disable { .. } => {
            tokio::task::spawn_blocking(sentryusb_gadget::disable).await
        }
    };
    match result {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            eprintln!("gadget: {}", e);
            1
        }
        Err(e) => {
            eprintln!("gadget task panicked: {}", e);
            1
        }
    }
}

async fn run_snapshot(action: SnapshotAction) -> i32 {
    match action {
        SnapshotAction::Make { args } => {
            // archiveloop calls `make_snapshot.sh nofsck` after a reboot
            // to skip the redundant fsck pass; treat anything else
            // (including bare "fsck" or no arg) as fsck-on. The bash
            // wrapper forwards `"$@"` so the first arg is what landed.
            let skip_fsck = args.iter().any(|a| a.eq_ignore_ascii_case("nofsck"));
            match sentryusb_gadget::snapshot::make_snapshot(skip_fsck).await {
                Ok(Some(name)) => {
                    println!("{}", name);
                    0
                }
                Ok(None) => {
                    // Snapshot was identical to the previous one and
                    // discarded. Print nothing — callers that capture
                    // stdout will see an empty string and know to
                    // skip; archiveloop's only consumer of this output
                    // is informational logging.
                    0
                }
                Err(e) => {
                    eprintln!("snapshot make: {}", e);
                    1
                }
            }
        }
        SnapshotAction::Release { name } => {
            match sentryusb_gadget::snapshot::release_snapshot(&name).await {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("snapshot release {}: {}", name, e);
                    1
                }
            }
        }
    }
}

async fn run_space(action: SpaceAction) -> i32 {
    match action {
        SpaceAction::Manage { .. } => match sentryusb_gadget::space::manage_free_space().await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("space manage: {}", e);
                1
            }
        },
    }
}
