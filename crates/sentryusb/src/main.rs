// The system allocator is used so the binary works on every Pi kernel
// regardless of page size (Pi 5 / Bookworm uses 16 KB pages while older
// Pis use 4 KB pages). A page-size-specific allocator like jemalloc
// aborts at startup when its compiled-in page size doesn't match the
// kernel's, which is why we don't use one here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use clap::{Parser, Subcommand};
use tower_http::CompressionLevel;
use tower_http::compression::{
    CompressionLayer,
    predicate::{NotForContentType, Predicate, SizeAbove},
};
use tracing::info;

mod embed;
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
    /// Delete /mutable/TeslaCam symlinks into snapshots that no longer exist.
    Sweep {
        /// Count what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum SpaceAction {
    /// Delete old snapshots until `/backingfiles` has enough free space.
    Manage {
        /// Free-space target in BYTES, as archiveloop's freespacemanager
        /// computes it and the manage_free_space.sh wrapper forwards.
        /// Omitted: use the same `10 GiB + total/33` formula. Parsed
        /// strictly — a malformed value is rejected rather than coerced
        /// (a reserve of 0 would "satisfy" instantly, and any garbage
        /// silently falling back to a different policy is what made the
        /// bash and Rust paths diverge in the first place).
        #[arg(value_parser = clap::value_parser!(u64).range(1..))]
        reserve_bytes: Option<u64>,
    },
}

#[tokio::main]
async fn main() {
    // Boot-phase timer. Lets us attribute the gap between systemd
    // "Started sentryusb.service" and the UDC bind in the journal.
    // Each `phase!` call emits `boot_phase=NAME elapsed_ms=N` so it's
    // greppable: `journalctl -b -u sentryusb.service | grep boot_phase`.
    let t0 = std::time::Instant::now();

    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sentryusb=info,sentryusb_api=info,sentryusb_drives=info,tower_http=info".into()),
        )
        .init();

    macro_rules! phase {
        ($name:expr) => {
            info!(boot_phase = $name, elapsed_ms = t0.elapsed().as_millis() as u64);
        };
    }
    phase!("tracing_initialized");

    let args = Args::parse();
    phase!("args_parsed");

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

    // Boot-time timezone safety net: if setup left TIME_ZONE=auto unresolved
    // (no network during setup → Pi stuck on UTC → drive telemetry mis-links),
    // re-resolve once now. Non-blocking; no-op once a real zone is set.
    tokio::spawn(async {
        sentryusb_setup::system::ensure_timezone_resolved().await;
    });

    // Periodic malloc_trim — releases heap pages back to the kernel that
    // glibc would otherwise keep cached in its per-arena free lists.
    // Combined with MALLOC_ARENA_MAX=2 (set in the systemd unit) this
    // keeps RSS bounded during/after burst workloads like clip ingest.
    // No-op on non-glibc targets.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    tokio::spawn(async {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(300));
        tick.tick().await; // skip the first immediate tick
        loop {
            tick.tick().await;
            // SAFETY: malloc_trim is thread-safe (takes the arena mutex
            // internally per glibc docs) and we call it from a tokio task,
            // never a signal handler. Returns 1 if memory was released, 0
            // if not.
            unsafe { libc::malloc_trim(0); }
        }
    });

    // Initialize auth
    let auth = sentryusb_api::init_auth();
    phase!("auth_initialized");

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
    // Remove orphaned files older binaries wrote to /mutable (drive-data.json
    // moved to /backingfiles, plus a couple of pre-Rust state files). Runs
    // after DriveStore::open so any one-shot importer that needs the legacy
    // path has already had a chance to consume it.
    sentryusb_drives::cleanup_legacy_mutable_files();
    phase!("drive_store_opened");

    // Legacy-JSON migration is now handled automatically inside
    // DriveStore::open via the one-shot import dance.
    // No manual step needed here — the import marker in the meta table
    // ensures it only runs once across the lifetime of the DB.

    // Cloud-uploader wake channel. Threaded into Processor so do_process
    // calls notify_one() at the tail of every successful run; the cloud
    // sweep loop is the only subscriber.
    let cloud_notify = Arc::new(tokio::sync::Notify::new());

    // Drive processor
    let mut processor = sentryusb_drives::processor::Processor::with_on_complete(
        store.clone(),
        hub.clone(),
        Some(cloud_notify.clone()),
    );
    // Gap-fill backfill: create missing RecentClips links for manifest
    // clips right after each process pass rewrites the manifest. Lives
    // here (not in the snapshot path) because on-device snapshots run
    // through the bash make_snapshot.sh, which never reaches the Rust
    // snapshot code — the daemon is the only OTA-updatable place this
    // can run. No-op unless the manifest changed since the last pass.
    processor.set_after_process(Arc::new(|| {
        if let Err(e) = sentryusb_gadget::snapshot::backfill_gapfill_links() {
            tracing::warn!("gap-fill backfill: {}", e);
        }
    }));
    let processor = Arc::new(processor);

    let drive_state = sentryusb_api::drives_handler::DriveState {
        store: store.clone(),
        processor: processor.clone(),
        importing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    // Keep-awake manager: busy if archiveloop is archiving OR the drive
    // processor is running.
    let is_busy_processor = processor.clone();
    let is_busy: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || {
        sentryusb_api::drives_handler::is_archiving() || is_busy_processor.is_running()
    });
    // Wipe any stale wanted-flag from a crashed prior run so the first
    // awake_stop after boot isn't deferred forever.
    sentryusb_api::drives_handler::clear_keep_awake_wanted();
    let keep_awake = sentryusb_api::keep_awake::KeepAwakeManager::new(is_busy);
    phase!("processor_keepawake_initialized");

    // SentryCloud upload pipeline. Background tasks pull pending routes
    // from the local DB, encrypt under the per-Pi key, and POST to
    // sentryusb.com/api/pi/routes whenever the Notify above fires.
    let cloud_uploader = sentryusb_cloud_uploader::CloudUploader::spawn_with_options(
        store.clone(),
        hub.clone(),
        cloud_notify,
        sentryusb_cloud_uploader::SpawnOptions {
            // Rate-config sync hook — preferences live in the api
            // crate, which depends on the uploader, so the binary wires
            // the implementation in here.
            rate_config: Some(Arc::new(sentryusb_api::preferences::PrefsRateConfig)),
            ..Default::default()
        },
    ).await;
    phase!("cloud_uploader_spawned");

    let app_state = sentryusb_api::router::AppState {
        hub: hub.clone(),
        auth: auth.clone(),
        drives: drive_state,
        keep_awake,
        cloud: sentryusb_api::cloud::CloudHandlerState {
            uploader: cloud_uploader,
        },
        net_sampler: Arc::new(Mutex::new(HashMap::new())),
    };

    // Resume setup if it was interrupted by a reboot (e.g. dwc2 overlay, root shrink)
    sentryusb_api::setup::auto_resume_setup(hub.clone());

    // Fire the anonymous install beacon once per install (gated by
    // /mutable/.beaconed). No fingerprint, no identifier — just an
    // incrementing counter on the support server. The opted-in update-
    // check telemetry is handled separately in check_for_update().
    sentryusb_api::update::spawn_install_beacon();

    // Resume Away Mode if the flag file still has time remaining.
    sentryusb_api::away_mode::restore_from_file();
    // One-shot: migrate legacy "VIN implies BLE on" users to explicit
    // BLE_ENABLED + BLE_KEEP_AWAKE_ENABLED flags so they don't lose
    // either feature across the decoupling change. Idempotent —
    // skips if `BLE_KEEP_AWAKE_ENABLED` is already present.
    sentryusb_api::ble::migrate_legacy_ble_flag();
    // Note (#336): the BLE_KEEP_AWAKE_VIA_SAMPLER seed call was removed.
    // The 4-valued flag is no longer read by the sampler — keep-awake
    // now always dispatches `charge-port-close` inline at the end of an
    // active tick (see "Sampler keep-awake CPC dispatch" in
    // tesla_telemetry/src/main.rs). Existing conf lines remain inert.
    // Boot-time storage auto repair (opt-in via the storage_auto_repair
    // preference). Detects a /backingfiles that failed to mount at boot
    // and runs the guarded xfs_repair ladder; see api::storage_repair.
    sentryusb_api::storage_repair::spawn_boot_check(hub.clone());
    // Boot-time sweep of TeslaCam symlinks orphaned by releases that skipped the
    // link purge. Delayed off the boot path; internally guarded and idempotent,
    // so a skipped run just retries next boot.
    tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        match tokio::task::spawn_blocking(|| {
            sentryusb_gadget::snapshot::sweep_dangling_links(false)
        })
        .await
        {
            Ok(Ok(0)) => {}
            Ok(Ok(n)) => tracing::info!("Dangling-link sweep removed {} stale TeslaCam link(s)", n),
            Ok(Err(e)) => tracing::info!("Dangling-link sweep skipped: {}", e),
            Err(e) => tracing::warn!("Dangling-link sweep task failed: {}", e),
        }
    });
    phase!("startup_tasks_spawned");

    // Build the API router
    let mut app = sentryusb_api::build_router(app_state.clone());

    // Serve TeslaCam video files via the bind mount of /mutable/TeslaCam
    // at /var/www/html/TeslaCam. Modern browsers (Chrome 80+, Firefox 70+,
    // Safari iOS 13+, ExoPlayer) parse Tesla's `ctts` atom natively, so
    // no FUSE wrapper is needed.
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

    // Compression wraps everything *after* all routes are added. axum's
    // `Router::layer` only wraps routes registered before the call, so we
    // apply compression AFTER the api router + ServeDir nests + SPA fallback
    // are in place. The predicate keeps already-compressed media bodies
    // (MP4, MP3, JPEG, ZIP) and binary streams out of the gzip path:
    //   - video/*  — Tesla MP4s under /TeslaCam/*; gzipping wastes CPU and
    //                produces no size win on already-compressed H.264.
    //   - audio/*  — /fs/* music/lock_chimes (MP3/AAC/OGG are pre-compressed).
    //   - image/*  — already-compressed JPEG/PNG/WebP.
    //   - application/octet-stream — /api/files/download streams arbitrary
    //                binary; without Content-Length the default predicate
    //                would gzip-stream the whole download. Skip.
    //   - application/zip — /api/files/download-zip; entries inside the zip
    //                are already DEFLATE'd, re-gzipping gains nothing.
    //   - application/grpc, text/event-stream — never compress these.
    // Size floor raised from the tower-http default of 32 bytes to 1024 to
    // match nginx/Cloudflare defaults — sub-1 KB JSON responses don't benefit
    // from gzip and incur per-request compression CPU. JSON above 1 KB and
    // the SPA JS/CSS bundle still compress normally (1.2 MB → ~280 KB).
    // Explicitly enable brotli + gzip + deflate. tower-http's
    // `compression-full` feature compiles all three in; the
    // CompressionLayer default already enables them, but spelling it
    // out makes the supported codecs obvious to anyone auditing the
    // file. Brotli is preferred when the client supports it (15–25%
    // smaller than gzip for JSON/HTML at comparable CPU).
    //
    // Embedded SPA assets that build.sh pre-compressed into
    // .br/.gz siblings are served by embed.rs with a
    // Content-Encoding header already set — tower-http detects that
    // and skips re-compressing, so no per-request CPU is wasted on
    // the bundle.
    // Brotli quality 6 (not the default 11) on dynamic responses. Quality
    // 11 is the compression algorithm's slowest setting — fine for the
    // build-time pre-compressed assets (paid once, served forever) but
    // wasteful per-request on a Pi Zero 2W where it can add 100-200 ms
    // to a single big-JSON response. Quality 6 gets within ~5% of
    // quality-11 size at ~15× the encode speed. Gzip and deflate use
    // level 6 too, which is their normal default — no behavior change
    // for those codecs.
    let compression = CompressionLayer::new()
        .br(true)
        .gzip(true)
        .deflate(true)
        .quality(CompressionLevel::Precise(6))
        .compress_when(
        SizeAbove::new(1024)
            .and(NotForContentType::new("video/"))
            .and(NotForContentType::new("audio/"))
            .and(NotForContentType::new("image/"))
            .and(NotForContentType::new("application/octet-stream"))
            .and(NotForContentType::new("application/zip"))
            .and(NotForContentType::new("application/grpc"))
            .and(NotForContentType::new("text/event-stream")),
    );
    app = app.layer(compression);

    // Auth middleware
    app = app.layer(axum::middleware::from_fn_with_state(
        auth,
        sentryusb_api::auth::auth_middleware,
    ));

    // Slow-request journal — outermost, so its timing covers auth +
    // compression + handler and login slowness is visible too.
    app = app.layer(axum::middleware::from_fn(
        sentryusb_api::router::slow_request_log,
    ));
    phase!("router_built");

    let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, args.port));
    info!("SentryUSB server listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind address");
    phase!("listener_bound");

    info!(
        boot_phase = "ready",
        elapsed_total_ms = t0.elapsed().as_millis() as u64,
        "SentryUSB ready to serve requests",
    );

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .expect("server error");
}

async fn shutdown_signal() {
    // systemd stops the service with SIGTERM — without listening for it
    // the graceful drain below only ever ran on interactive Ctrl+C.
    #[cfg(unix)]
    {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = sigterm.recv() => {}
        }
    }
    #[cfg(not(unix))]
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
        SnapshotAction::Sweep { dry_run } => {
            let result = tokio::task::spawn_blocking(move || {
                sentryusb_gadget::snapshot::sweep_dangling_links(dry_run)
            })
            .await;
            match result {
                Ok(Ok(n)) => {
                    println!("{} dangling link(s) {}", n, if dry_run { "found" } else { "removed" });
                    0
                }
                Ok(Err(e)) => {
                    eprintln!("snapshot sweep: {}", e);
                    1
                }
                Err(e) => {
                    eprintln!("snapshot sweep task panicked: {}", e);
                    1
                }
            }
        }
    }
}

async fn run_space(action: SpaceAction) -> i32 {
    match action {
        SpaceAction::Manage { reserve_bytes } => match sentryusb_gadget::space::manage_free_space(
            reserve_bytes,
        )
        .await
        {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("space manage: {}", e);
                1
            }
        },
    }
}
