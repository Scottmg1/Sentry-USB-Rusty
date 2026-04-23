use axum::routing::{delete, get, post, put};
use axum::Router;

use std::sync::Arc;

use crate::auth::AuthState;
use crate::drives_handler::DriveState;
use crate::keep_awake::KeepAwakeManager;

/// Shared application state available to all handlers.
#[derive(Clone)]
pub struct AppState {
    pub hub: sentryusb_ws::Hub,
    pub auth: AuthState,
    pub drives: DriveState,
    pub keep_awake: Arc<KeepAwakeManager>,
}

/// Build the complete Axum router with all API routes.
pub fn build_router(state: AppState) -> Router {
    let api = Router::new()
        // Status & config
        .route("/api/status", get(crate::status::get_status))
        .route("/api/status/storage", get(crate::status::get_storage_breakdown))
        .route("/api/config", get(crate::status::get_config))
        .route("/api/wifi", get(crate::status::get_wifi_config))
        // Auth
        .route("/api/auth/login", post(crate::auth::handle_login))
        .route("/api/auth/logout", post(crate::auth::handle_logout))
        .route("/api/auth/check", get(crate::auth::handle_auth_check))
        // Setup
        .route("/api/setup/status", get(crate::setup::get_setup_status))
        .route("/api/setup/config", get(crate::setup::get_setup_config).put(crate::setup::save_setup_config))
        .route("/api/setup/run", post(crate::setup::run_setup))
        .route("/api/setup/phases", get(crate::setup::get_setup_phases))
        .route("/api/setup/test-archive", post(crate::setup::test_archive))
        // Clips
        .route("/api/clips", get(crate::clips::get_clips))
        .route("/api/clips/telemetry", get(crate::clips::get_clip_telemetry))
        // Files
        .route("/api/files/ls", get(crate::files::list_files))
        .route("/api/files/mkdir", post(crate::files::create_dir))
        .route("/api/files/mv", post(crate::files::move_file))
        .route("/api/files/cp", post(crate::files::copy_file))
        .route("/api/files", delete(crate::files::delete_file))
        .route("/api/files/upload", post(crate::files::upload_file))
        .route("/api/files/download", get(crate::files::download_file))
        .route("/api/files/download-zip", get(crate::files::download_zip))
        .route("/api/files/download-zip-multi", post(crate::files::download_zip_multi))
        // Logs
        .route("/api/logs/{name}", get(crate::logs::get_log))
        // Diagnostics & health
        .route("/api/diagnostics/refresh", post(crate::healthcheck::refresh_diagnostics))
        .route("/api/diagnostics", get(crate::healthcheck::get_diagnostics))
        .route("/api/system/health-check", get(crate::healthcheck::health_check))
        // System
        .route("/api/system/reboot", post(crate::system::reboot))
        .route("/api/system/toggle-drives", post(crate::system::toggle_drives))
        .route("/api/system/gadget-enable", post(crate::system::gadget_enable))
        .route("/api/system/gadget-disable", post(crate::system::gadget_disable))
        .route("/api/system/trigger-sync", post(crate::system::trigger_sync))
        .route("/api/system/ble-pair", post(crate::system::ble_pair))
        .route("/api/system/ble-status", get(crate::system::ble_status))
        .route("/api/system/speedtest", get(crate::system::speedtest))
        .route("/api/system/rtc-status", get(crate::system::get_rtc_status))
        .route("/api/system/ssh-pubkey", get(crate::system::get_ssh_pubkey))
        .route("/api/system/ssh-keygen", post(crate::system::generate_ssh_key))
        .route("/api/system/check-internet", get(crate::update::check_internet))
        .route("/api/system/update", post(crate::update::run_update))
        .route("/api/system/version", get(crate::update::get_version))
        .route("/api/system/check-update", post(crate::update::check_for_update))
        .route("/api/system/update-status", get(crate::update::get_update_status))
        .route("/api/system/block-devices", get(crate::devices::list_block_devices))
        // Preferences
        .route("/api/config/preference", get(crate::preferences::get_preference).put(crate::preferences::set_preference))
        // Notifications
        .route("/api/notifications/generate-code", post(crate::notifications::generate_pairing_code))
        .route("/api/notifications/paired-devices", get(crate::notifications::list_paired_devices))
        .route("/api/notifications/paired-devices/{id}", delete(crate::notifications::remove_paired_device))
        .route("/api/notifications/test", post(crate::notifications::send_test_notification))
        .route("/api/notifications/settings", get(crate::notification_center::get_settings).put(crate::notification_center::update_settings))
        .route("/api/notifications/history", get(crate::notification_center::get_history).post(crate::notification_center::append_history).delete(crate::notification_center::clear_history))
        .route("/api/notifications/history/{id}", delete(crate::notification_center::delete_history_item))
        .route("/api/notifications/settings/check", get(crate::notification_center::check_notification_type))
        // Support
        .route("/api/support/check", get(crate::support::check_available))
        .route("/api/support/ticket", post(crate::support::create_ticket))
        .route("/api/support/ticket/{id}/message", post(crate::support::send_message))
        .route("/api/support/ticket/{id}/media", post(crate::support::upload_media))
        .route("/api/support/ticket/{id}/messages", get(crate::support::fetch_messages))
        .route("/api/support/ticket/{id}/close", post(crate::support::close_ticket))
        .route("/api/support/ticket/{id}/mark-read", post(crate::support::mark_read))
        .route("/api/support/ticket/{id}/register-device", post(crate::support::register_device))
        .route("/api/support/ticket/{id}/unregister-device", post(crate::support::unregister_device))
        // Lock chime
        .route("/api/lockchime/list", get(crate::lock_chime::list))
        .route("/api/lockchime/upload", post(crate::lock_chime::upload))
        .route("/api/lockchime/activate/{filename}", post(crate::lock_chime::activate))
        .route("/api/lockchime/clear-active", post(crate::lock_chime::clear_active))
        .route("/api/lockchime/{filename}", delete(crate::lock_chime::delete_chime))
        .route("/api/lockchime/random-config", get(crate::lock_chime::get_random_config).put(crate::lock_chime::save_random_config))
        .route("/api/lockchime/randomize", post(crate::lock_chime::randomize))
        .route("/api/lockchime/randomize-on-connect", post(crate::lock_chime::randomize_on_connect))
        .route("/api/lockchime/ble-shift-state", get(crate::lock_chime::ble_shift_state))
        // Community lock chimes
        .route("/api/lockchime/community/library", get(crate::community::lock_chime_library))
        .route("/api/lockchime/community/stream/{code}", get(crate::community::lock_chime_stream))
        .route("/api/lockchime/community/upload", post(crate::community::lock_chime_upload))
        .route("/api/lockchime/community/download/{code}", post(crate::community::lock_chime_download))
        .route("/api/lockchime/community/admin/validate", post(crate::community::lock_chime_admin_validate))
        .route("/api/lockchime/community/admin/edit/{code}", put(crate::community::lock_chime_admin_edit))
        .route("/api/lockchime/community/admin/delete/{code}", delete(crate::community::lock_chime_admin_delete))
        // Community wraps
        .route("/api/wraps/library", get(crate::community::wraps_library))
        .route("/api/wraps/thumbnail/{code}", get(crate::community::wraps_thumbnail))
        .route("/api/wraps/preview/{code}", get(crate::community::wraps_preview))
        .route("/api/wraps/upload", post(crate::community::wraps_upload))
        .route("/api/wraps/download/{code}", post(crate::community::wraps_download))
        .route("/api/wraps/admin/validate", post(crate::community::wraps_admin_validate))
        .route("/api/wraps/admin/edit/{code}", put(crate::community::wraps_admin_edit))
        .route("/api/wraps/admin/delete/{code}", delete(crate::community::wraps_admin_delete))
        // Memory debug
        .route("/api/memory", get(crate::memory::memory_stats))
        // Backup
        .route("/api/system/backup", post(crate::backup::create_backup))
        .route("/api/system/backups", get(crate::backup::list_backups))
        .route("/api/system/backup/{date}", get(crate::backup::get_backup))
        .route("/api/system/restore", post(crate::backup::restore_backup))
        // Drives
        .route("/api/drives", get(crate::drives_handler::list_drives))
        .route("/api/drives/routes", get(crate::drives_handler::all_routes))
        .route("/api/drives/tags", get(crate::drives_handler::list_tags))
        .route("/api/drives/process", get(crate::drives_handler::processing_status).post(crate::drives_handler::process_files))
        .route("/api/drives/reprocess", post(crate::drives_handler::reprocess_all))
        .route("/api/drives/status", get(crate::drives_handler::processing_status))
        .route("/api/drives/data/download", get(crate::drives_handler::download_data))
        .route("/api/drives/data/upload", post(crate::drives_handler::upload_data))
        .route("/api/drives/stats", get(crate::drives_handler::drive_stats))
        .route("/api/drives/fsd-analytics", get(crate::drives_handler::fsd_analytics))
        .route("/api/drives/{id}/tags", put(crate::drives_handler::set_drive_tags))
        .route("/api/drives/{id}", get(crate::drives_handler::single_drive))
        // Keep-awake
        .route("/api/keep-awake/start", post(crate::keep_awake::start))
        .route("/api/keep-awake/stop", post(crate::keep_awake::stop))
        .route("/api/keep-awake/status", get(crate::keep_awake::status))
        .route("/api/keep-awake/heartbeat", post(crate::keep_awake::heartbeat))
        // Away mode
        .route("/api/away-mode/enable", post(crate::away_mode::enable))
        .route("/api/away-mode/disable", post(crate::away_mode::disable))
        .route("/api/away-mode/status", get(crate::away_mode::status))
        // Terminal WebSocket
        .route("/api/terminal", get(crate::terminal::handle_terminal))
        // WebSocket
        .route("/api/ws", get(ws_handler))
        // Memory HTML page
        .route("/memory", get(crate::memory::memory_page));

    api.with_state(state)
}

/// WebSocket handler.
async fn ws_handler(
    ws: axum::extract::WebSocketUpgrade,
    axum::extract::State(state): axum::extract::State<AppState>,
) -> impl axum::response::IntoResponse {
    ws.on_upgrade(move |socket| handle_ws(socket, state.hub))
}

async fn handle_ws(socket: axum::extract::ws::WebSocket, hub: sentryusb_ws::Hub) {
    use axum::extract::ws::Message;
    use tokio::time::{interval, Duration};

    let mut rx = hub.subscribe();

    let (mut sender, mut receiver) = socket.split();
    use futures_util::{SinkExt, StreamExt};

    let hub_clone = hub.clone();

    // Writer task: forward broadcasts + periodic pings
    let mut send_task = tokio::spawn(async move {
        let mut ping_interval = interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        Ok(data) => {
                            if sender.send(Message::Text(String::from_utf8_lossy(&data).into_owned().into())).await.is_err() {
                                break;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(_) => break,
                    }
                }
                _ = ping_interval.tick() => {
                    let ping_msg = serde_json::json!({"type": "ping", "data": null});
                    if sender.send(Message::Text(ping_msg.to_string().into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Reader task: any message (including pong) resets the 60-second read
    // deadline. Two missed pings (30s each) tears down the socket — matches
    // Go's `SetReadDeadline(60s)` in hub.go and means a JS tab that's been
    // paused by the browser stops holding a server-side goroutine within a
    // minute, instead of however long it takes the TCP send buffer to fill.
    let mut recv_task = tokio::spawn(async move {
        loop {
            match tokio::time::timeout(Duration::from_secs(60), receiver.next()).await {
                Ok(Some(Ok(_))) => continue,   // message/pong → deadline resets
                Ok(Some(Err(_))) | Ok(None) => break, // socket error / closed
                Err(_) => break,               // deadline — tear down
            }
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = &mut send_task => { recv_task.abort(); }
        _ = &mut recv_task => { send_task.abort(); }
    }

    hub_clone.client_disconnected();
}
