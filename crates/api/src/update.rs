//! OTA update: check for updates, run update, version info.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;

use crate::router::AppState;
use crate::status::get_sbc_model;

/// Persists the last update check for `get_update_status`.
const UPDATE_CHECK_CACHE: &str = "/tmp/sentryusb-update-check.json";

static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

/// Keep fixed to preserve pseudonymous fingerprint continuity.
const TELEMETRY_SALT: &str = "SENTRYUSB_2026_PROD";

/// Cached SHA-256 hash of the SBC serial, falling back to machine-id.
pub(crate) fn get_fingerprint() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| {
        use ring::digest::{SHA256, digest};
        let mut id = String::new();
        for p in [
            "/sys/firmware/devicetree/base/serial-number",
            "/proc/device-tree/serial-number",
        ] {
            if let Ok(raw) = std::fs::read_to_string(p) {
                let trimmed = raw.trim_matches(|c: char| c == '\0' || c.is_whitespace());
                if !trimmed.is_empty() {
                    id = trimmed.to_string();
                    break;
                }
            }
        }
        if id.is_empty() {
            for p in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
                if let Ok(raw) = std::fs::read_to_string(p) {
                    let trimmed = raw.trim();
                    if !trimmed.is_empty() {
                        id = trimmed.to_string();
                        break;
                    }
                }
            }
        }
        if id.is_empty() {
            tracing::warn!("[telemetry] no fingerprint source available");
            return String::new();
        }
        let h = digest(&SHA256, format!("{}{}", id, TELEMETRY_SALT).as_bytes());
        hex::encode(h.as_ref())
    })
    .as_str()
}

/// GET /api/system/check-internet
pub async fn check_internet(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    use futures_util::future::select_ok;
    use std::time::Duration;
    use tokio::net::TcpStream;

    // Race HTTPS probes because some Pi-hole networks block direct DNS probes.
    let t = Duration::from_secs(2);
    let probes: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send>>> = vec![
        Box::pin(async move {
            tokio::time::timeout(t, TcpStream::connect("8.8.8.8:443")).await
                .map_err(|_| anyhow::anyhow!("timeout"))?.map_err(anyhow::Error::from)?;
            Ok(())
        }),
        Box::pin(async move {
            tokio::time::timeout(t, TcpStream::connect("1.1.1.1:443")).await
                .map_err(|_| anyhow::anyhow!("timeout"))?.map_err(anyhow::Error::from)?;
            Ok(())
        }),
    ];
    let connected = select_ok(probes).await.is_ok();
    (StatusCode::OK, Json(serde_json::json!({"connected": connected})))
}

/// Starts an OTA update. An optional `{"version":"vX.Y.Z"}` body selects a
/// release; an empty body installs the latest release.
pub async fn run_update(
    State(s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    if UPDATE_RUNNING.swap(true, Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Update already in progress");
    }

    // The client omits the body when installing the latest release.
    let target_version: Option<String> = if body.trim().is_empty() {
        None
    } else {
        serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("version").and_then(|s| s.as_str()).map(String::from))
            .filter(|s| !s.is_empty())
    };

    let hub = s.hub.clone();
    tokio::spawn(async move {
        hub.broadcast("update_status", &serde_json::json!({"status": "running"}));

        let result = self_update(&hub, target_version).await;

        UPDATE_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(msg) => {
                hub.broadcast("update_status", &serde_json::json!({
                    "status": "complete",
                    "output": msg
                }));

                // Allow each WebSocket status to reach the client before rebooting.
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                hub.broadcast("update_status", &serde_json::json!({
                    "status": "restarting",
                    "message": "Restarting Pi to apply update…"
                }));
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;

                let _ = sentryusb_shell::run("reboot", &[]).await;
            }
            Err(e) => hub.broadcast("update_status", &serde_json::json!({
                "status": "error",
                "error": e.to_string()
            })),
        }
    });

    (StatusCode::OK, Json(serde_json::json!({"status": "started"})))
}

/// Default OTA source.
const DEFAULT_UPDATE_OWNER: &str = "Sentry-Six";
const DEFAULT_UPDATE_REPO_NAME: &str = "Sentry-USB-Rusty";

/// Resolves the configured owner while keeping the release repository name fixed.
fn update_repo() -> String {
    let path = sentryusb_config::find_config_path();
    let (active, _commented) = sentryusb_config::parse_file(path).unwrap_or_default();
    let owner = active
        .get("REPO")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_UPDATE_OWNER);
    format!("{}/{}", owner, DEFAULT_UPDATE_REPO_NAME)
}

/// Detects the release suffix selected by the boot picker or equivalent live
/// CPU rules. Userspace architecture takes precedence over kernel architecture.
async fn detect_release_suffix() -> anyhow::Result<String> {
    // Older pickers recorded fallback names, so accept only release suffixes.
    const KNOWN_SUFFIXES: &[&str] = &[
        "linux-arm64-a53",
        "linux-arm64-a72",
        "linux-arm64-a76",
        "linux-armv7",
        "linux-amd64",
    ];
    if let Ok(s) = std::fs::read_to_string("/opt/sentryusb/active-variant") {
        let trimmed = s.trim();
        if KNOWN_SUFFIXES.contains(&trimmed) {
            return Ok(trimmed.to_string());
        }
        if !trimmed.is_empty() {
            tracing::warn!(
                "active-variant contains {:?} (not a release suffix) — \
                 ignoring it and re-detecting from CPU",
                trimmed
            );
        }
    }

    // dpkg reflects userspace on systems with a 64-bit kernel and 32-bit rootfs.
    let family = if let Ok(out) = sentryusb_shell::run("dpkg", &["--print-architecture"]).await {
        match out.trim() {
            "arm64" => "aarch64",
            "armhf" => return Ok("linux-armv7".to_string()),
            "armel" => anyhow::bail!(
                "armv6 (armel / Pi Zero W / Pi 1) is no longer supported — \
                 SentryUSB requires Pi Zero 2 W or newer"
            ),
            "amd64" => return Ok("linux-amd64".to_string()),
            other => anyhow::bail!("unsupported userspace architecture: {}", other),
        }
    } else {
        let arch = sentryusb_shell::run("uname", &["-m"]).await?;
        match arch.trim() {
            "aarch64" => "aarch64",
            "armv7l" => return Ok("linux-armv7".to_string()),
            "armv6l" => anyhow::bail!(
                "armv6 (Pi Zero W / Pi 1) is no longer supported — \
                 SentryUSB requires Pi Zero 2 W or newer"
            ),
            "x86_64" => return Ok("linux-amd64".to_string()),
            other => anyhow::bail!("unsupported architecture: {}", other),
        }
    };

    // Mirror the boot picker's aarch64 rules on pre-picker installations.
    debug_assert_eq!(family, "aarch64");
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        // The a76 build requires both LSE atomics and the crypto extension.
        let has_hwcap = |cap: &str| {
            cpuinfo.lines().any(|line| {
                line.starts_with("Features")
                    && line.split_whitespace().any(|w| w == cap)
            })
        };
        if has_hwcap("atomics") && has_hwcap("aes") {
            return Ok("linux-arm64-a76".to_string());
        }
        // 0xD08 = Cortex-A72 (Pi 4 / RK3399 perf cluster).
        for line in cpuinfo.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("CPU part") {
                let part = trimmed.split(':').nth(1).unwrap_or("").trim().to_ascii_lowercase();
                if part == "0xd08" {
                    return Ok("linux-arm64-a72".to_string());
                }
            }
        }
    }
    // Default for aarch64: Cortex-A53 (Pi 3, Pi Zero 2 W, Allwinner H618).
    Ok("linux-arm64-a53".to_string())
}

/// Streams a release to staging and broadcasts advisory download progress.
async fn download_with_progress(
    hub: &sentryusb_ws::Hub,
    url: &str,
    dest: &str,
) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let resp = crate::http_client()
        .get(url)
        .header("User-Agent", "sentryusb-updater")
        .send()
        .await?
        .error_for_status()?;
    // CDN responses may omit Content-Length; the UI then uses indeterminate progress.
    let total = resp.content_length().filter(|t| *t > 0);
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut last_percent: i64 = -1;
    let mut last_emit = std::time::Instant::now();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        let percent = total.map(|t| ((done as f64 / t as f64) * 100.0) as i64);
        let emit = match percent {
            Some(p) => p != last_percent,
            None => last_emit.elapsed() >= std::time::Duration::from_millis(500),
        };
        if emit {
            if let Some(p) = percent {
                last_percent = p;
            }
            last_emit = std::time::Instant::now();
            hub.broadcast(
                "update_status",
                &serde_json::json!({
                    "status": "downloading",
                    "message": "Downloading update…",
                    "percent": percent,
                    "bytes_done": done,
                    "bytes_total": total,
                }),
            );
        }
    }
    file.flush().await?;
    Ok(())
}

async fn self_update(
    hub: &sentryusb_ws::Hub,
    target_version: Option<String>,
) -> anyhow::Result<String> {
    hub.broadcast(
        "update_status",
        &serde_json::json!({"status": "checking", "message": "Checking release…"}),
    );
    let suffix = detect_release_suffix().await?;
    let repo = update_repo();

    // A requested version supports both prerelease installs and stable reverts.
    let url = if let Some(v) = &target_version {
        format!(
            "https://github.com/{}/releases/download/{}/sentryusb-{}",
            repo, v, suffix
        )
    } else {
        format!(
            "https://github.com/{}/releases/latest/download/sentryusb-{}",
            repo, suffix
        )
    };

    // Fail clearly if the selected release has no matching binary.
    sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(15),
        "curl",
        &["-sfI", "--max-time", "10", &url],
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "No release binary found at {}. Publish a release with the binary first.",
            url
        )
    })?;

    // Try overlay-aware, standard, and legacy remount forms for supported images.
    hub.broadcast(
        "update_status",
        &serde_json::json!({"status": "remounting", "message": "Preparing filesystem…"}),
    );
    let _ = sentryusb_shell::run("/root/bin/remountfs_rw", &[]).await;
    let _ = sentryusb_shell::run("mount", &["-o", "remount,rw", "/"]).await;
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    // Same-filesystem staging makes the final rename atomic across power loss.
    sentryusb_shell::run("mkdir", &["-p", "/opt/sentryusb"]).await?;
    let tmp = "/opt/sentryusb/.sentryusb-update.new";
    download_with_progress(hub, &url, tmp).await?;

    hub.broadcast(
        "update_status",
        &serde_json::json!({"status": "installing", "message": "Installing update…"}),
    );
    sentryusb_shell::run("chmod", &["+x", tmp]).await?;

    // Preserve the picker's multi-binary layout:
    //   /opt/sentryusb/sentryusb-{suffix}            ← we write here
    //   /opt/sentryusb/sentryusb-current → ↑         ← picker symlink
    //   /opt/sentryusb/sentryusb         → -current  ← back-compat symlink
    //
    // Pre-multi-binary installations still require the legacy destination.
    let dest = if std::path::Path::new("/opt/sentryusb/sentryusb-current").exists() {
        format!("/opt/sentryusb/sentryusb-{}", suffix)
    } else {
        "/opt/sentryusb/sentryusb".to_string()
    };
    sentryusb_shell::run("mv", &[tmp, &dest]).await?;

    // Keep the optional telemetry sampler on the main binary's release and ABI.
    let telemetry_url = if let Some(v) = &target_version {
        format!(
            "https://github.com/{}/releases/download/{}/sentryusb-tesla-telemetry-{}",
            repo, v, suffix
        )
    } else {
        format!(
            "https://github.com/{}/releases/latest/download/sentryusb-tesla-telemetry-{}",
            repo, suffix
        )
    };
    // Auxiliary failures must remain visible even when the core update succeeds.
    let mut install_warnings: Vec<String> = Vec::new();
    hub.broadcast(
        "update_status",
        &serde_json::json!({"status": "installing", "message": "Installing components…"}),
    );
    let head_ok = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(15),
        "curl",
        &["-sfI", "--max-time", "10", &telemetry_url],
    )
    .await
    .is_ok();
    if head_ok {
        // Stage beside the destination for an atomic rename.
        let telemetry_tmp = "/opt/sentryusb/.sentryusb-tesla-telemetry-update.new";
        match sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(120),
            "curl",
            &["-fsSL", &telemetry_url, "-o", telemetry_tmp],
        )
        .await
        {
            Ok(_) => {
                // The picker repairs this per-CPU symlink when an SD card moves hosts.
                if let Err(e) =
                    sentryusb_shell::run("mkdir", &["-p", "/root/bin"]).await
                {
                    install_warnings.push(format!(
                        "telemetry: mkdir /root/bin failed: {e}"
                    ));
                }
                if let Err(e) =
                    sentryusb_shell::run("mkdir", &["-p", "/opt/sentryusb"]).await
                {
                    install_warnings.push(format!(
                        "telemetry: mkdir /opt/sentryusb failed: {e}"
                    ));
                }
                if let Err(e) =
                    sentryusb_shell::run("chmod", &["+x", telemetry_tmp]).await
                {
                    install_warnings.push(format!(
                        "telemetry: chmod +x failed: {e}"
                    ));
                }
                let telemetry_dest =
                    format!("/opt/sentryusb/sentryusb-tesla-telemetry-{}", suffix);
                // Move first so a failed install cannot leave a dangling symlink.
                let install_result =
                    match sentryusb_shell::run("mv", &[telemetry_tmp, &telemetry_dest])
                        .await
                    {
                        Ok(_) => {
                            sentryusb_shell::run(
                                "ln",
                                &[
                                    "-sfn",
                                    &telemetry_dest,
                                    "/root/bin/sentryusb-tesla-telemetry",
                                ],
                            )
                            .await
                            .map(|_| ())
                        }
                        Err(e) => Err(e),
                    };
                match install_result {
                    Ok(_) => {
                        // Activate the new sampler without waiting for reboot.
                        let _ = sentryusb_shell::run(
                            "systemctl",
                            &["daemon-reload"],
                        )
                        .await;
                        let _ = sentryusb_shell::run(
                            "systemctl",
                            &["restart", "sentryusb-telemetry"],
                        )
                        .await;
                    }
                    Err(e) => {
                        install_warnings.push(format!(
                            "telemetry binary install FAILED \
                             (install to {telemetry_dest} + symlink from \
                             /root/bin/sentryusb-tesla-telemetry): \
                             {e} — likely read-only rootfs; if your image \
                             uses an overlay you may need to remount manually \
                             or wait for the next reboot"
                        ));
                        // Remove the failed staging file.
                        let _ = sentryusb_shell::run("rm", &["-f", telemetry_tmp]).await;
                    }
                }
            }
            Err(e) => {
                install_warnings.push(format!(
                    "telemetry binary download FAILED ({}): {e}",
                    telemetry_url
                ));
            }
        }
    } else {
        // Older releases may not include the optional sampler.
        tracing::info!(
            "release does not include a telemetry binary at {} — skipping",
            telemetry_url
        );
    }

    // Keep the optional BLE-action CLI on the main binary's protocol version.
    let action_url = if let Some(v) = &target_version {
        format!(
            "https://github.com/{}/releases/download/{}/sentryusb-ble-action-{}",
            repo, v, suffix
        )
    } else {
        format!(
            "https://github.com/{}/releases/latest/download/sentryusb-ble-action-{}",
            repo, suffix
        )
    };
    let head_ok_action = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(15),
        "curl",
        &["-sfI", "--max-time", "10", &action_url],
    )
    .await
    .is_ok();
    if head_ok_action {
        // Stage beside the destination for an atomic rename.
        let action_tmp = "/opt/sentryusb/.sentryusb-ble-action-update.new";
        match sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(120),
            "curl",
            &["-fsSL", &action_url, "-o", action_tmp],
        )
        .await
        {
            Ok(_) => {
                if let Err(e) =
                    sentryusb_shell::run("mkdir", &["-p", "/root/bin"]).await
                {
                    install_warnings.push(format!(
                        "ble-action: mkdir /root/bin failed: {e}"
                    ));
                }
                if let Err(e) =
                    sentryusb_shell::run("mkdir", &["-p", "/opt/sentryusb"]).await
                {
                    install_warnings.push(format!(
                        "ble-action: mkdir /opt/sentryusb failed: {e}"
                    ));
                }
                if let Err(e) =
                    sentryusb_shell::run("chmod", &["+x", action_tmp]).await
                {
                    install_warnings.push(format!(
                        "ble-action: chmod +x failed: {e}"
                    ));
                }
                // Move first so a failed install cannot leave a dangling symlink.
                let action_dest =
                    format!("/opt/sentryusb/sentryusb-ble-action-{}", suffix);
                let install_result =
                    match sentryusb_shell::run("mv", &[action_tmp, &action_dest])
                        .await
                    {
                        Ok(_) => {
                            sentryusb_shell::run(
                                "ln",
                                &[
                                    "-sfn",
                                    &action_dest,
                                    "/root/bin/sentryusb-ble-action",
                                ],
                            )
                            .await
                            .map(|_| ())
                        }
                        Err(e) => Err(e),
                    };
                if let Err(e) = install_result {
                    install_warnings.push(format!(
                        "ble-action binary install FAILED \
                         (install to {action_dest} + symlink from \
                         /root/bin/sentryusb-ble-action): {e} — \
                         likely read-only rootfs"
                    ));
                    let _ = sentryusb_shell::run("rm", &["-f", action_tmp]).await;
                }
            }
            Err(e) => {
                install_warnings.push(format!(
                    "ble-action binary download FAILED ({}): {e}",
                    action_url
                ));
            }
        }
    } else {
        tracing::info!(
            "release does not include a ble-action binary at {} — skipping",
            action_url
        );
    }

    // Record the requested tag, or resolve the tag backing `/latest`.
    let tag = match target_version {
        Some(v) => v,
        None => {
            let api_url = format!(
                "https://api.github.com/repos/{}/releases/latest",
                repo
            );
            match crate::http_client()
                .get(&api_url)
                .header("User-Agent", "sentryusb-updater")
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) => resp
                    .json::<serde_json::Value>()
                    .await
                    .ok()
                    .and_then(|v| {
                        v.get("tag_name")
                            .and_then(|t| t.as_str())
                            .map(|s| s.trim().to_string())
                    })
                    .unwrap_or_default(),
                Err(_) => String::new(),
            }
        }
    };

    if !tag.is_empty() {
        let _ = std::fs::write("/opt/sentryusb/version", &tag);
    }

    // Refresh and reapply hardware-specific runtime patches after the binary swap.
    hub.broadcast(
        "update_status",
        &serde_json::json!({"status": "updating_scripts", "message": "Updating scripts…"}),
    );
    let patches_path = "/usr/local/bin/sentryusb-apply-runtime-patches";
    // Pin the helper to the installed tag. With no tag, retain an existing
    // helper or bootstrap from main only when no helper exists.
    let patches_ref = if !tag.is_empty() {
        Some(tag.clone())
    } else if std::path::Path::new(patches_path).exists() {
        None
    } else {
        Some("main".to_string())
    };
    let patches_url = patches_ref.as_ref().map(|r| {
        format!(
            "https://raw.githubusercontent.com/{}/{}/setup/pi/apply-runtime-patches.sh",
            repo, r
        )
    });
    // Stage on the target filesystem so replacement is atomic.
    let patches_tmp = "/usr/local/bin/.sentryusb-apply-runtime-patches.new";
    // `None` keeps the installed helper when the release tag is unknown.
    let download = match &patches_url {
        Some(url) => {
            tracing::info!("update.rs: refreshing runtime-patches script from {}", url);
            Some(
                sentryusb_shell::run_with_timeout(
                    std::time::Duration::from_secs(20),
                    "curl",
                    &["-fsSL", "--max-time", "15", "-o", patches_tmp, url],
                )
                .await,
            )
        }
        None => {
            tracing::warn!(
                "update.rs: release tag unknown — keeping the existing runtime-patches \
                 script rather than pulling an unpinned copy from main"
            );
            install_warnings.push(
                "could not determine the release tag, so the existing runtime-patches \
                 script was kept; fixes added in this release may not have applied."
                    .to_string(),
            );
            None
        }
    };
    match download {
        Some(Ok(_)) => {
            // Never replace the live helper with an empty response.
            if std::fs::metadata(patches_tmp)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
            {
                // Validate permissions and syntax before replacing the live helper.
                let staged_ok = sentryusb_shell::run("chmod", &["+x", patches_tmp])
                    .await
                    .is_ok()
                    && sentryusb_shell::run("bash", &["-n", patches_tmp]).await.is_ok();
                if !staged_ok {
                    let _ = std::fs::remove_file(patches_tmp);
                    tracing::error!(
                        "update.rs: staged runtime-patches script failed chmod/syntax check; \
                         keeping the existing one"
                    );
                    install_warnings.push(
                        "the downloaded runtime-patches script was not executable or failed a \
                         syntax check, so the previous one was kept; fixes added in this \
                         release may not have applied."
                            .to_string(),
                    );
                }
                match if staged_ok {
                    std::fs::rename(patches_tmp, patches_path)
                } else {
                    // Validation failure leaves the live helper untouched.
                    Ok(())
                } {
                    Ok(()) if staged_ok => {
                        tracing::info!("update.rs: runtime-patches script refreshed");
                    }
                    Ok(()) => {}
                    Err(e) => {
                        let _ = std::fs::remove_file(patches_tmp);
                        tracing::error!(
                            "update.rs: runtime-patches swap FAILED ({e}); keeping existing script"
                        );
                        install_warnings.push(format!(
                            "runtime-patches script could not be replaced ({e}): this device will \
                             re-run its EXISTING patch script, so fixes added in this release may \
                             not apply. Re-run install-pi.sh manually."
                        ));
                    }
                }
            } else {
                let _ = std::fs::remove_file(patches_tmp);
                if !std::path::Path::new(patches_path).exists() {
                    install_warnings.push(
                        "runtime-patches download empty AND no existing script: board-specific \
                         fixes won't apply this update. Re-run install-pi.sh manually."
                            .to_string(),
                    );
                }
            }
        }
        Some(Err(e)) => {
            let _ = std::fs::remove_file(patches_tmp);
            if !std::path::Path::new(patches_path).exists() {
                install_warnings.push(format!(
                    "runtime-patches download FAILED ({e}) AND no existing script: board-specific \
                     fixes (BCM4345C0 BLE on Rock 4C+, EATT disable, etc.) won't auto-reapply \
                     after this update. Re-run install-pi.sh manually if BLE pairing breaks."
                ));
            } else {
                tracing::warn!(
                    "update.rs: runtime-patches refresh failed ({e}), falling back to existing on-disk script"
                );
            }
        }
        // Tag unknown; retain the existing helper.
        None => {}
    }

    if std::path::Path::new(patches_path).exists() {
        match sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(30),
            patches_path,
            &[],
        )
        .await
        {
            Ok(_) => tracing::info!("update.rs: runtime-patches re-applied successfully"),
            Err(e) => install_warnings.push(format!(
                "runtime-patches re-apply FAILED: {e} — board-specific fixes \
                 (BCM4345C0 BLE on Rock 4C+, etc.) may not survive this update; \
                 if BLE pairing is broken after this update, re-run install-pi.sh"
            )),
        }
    }

    if install_warnings.is_empty() {
        Ok(format!(
            "Updated to {}.",
            if tag.is_empty() { "latest".to_string() } else { tag }
        ))
    } else {
        // Keep full journal detail but cap the WebSocket response at 4 KiB.
        for w in &install_warnings {
            tracing::warn!("update.rs: {}", w);
        }
        let joined = install_warnings.join("\n  • ");
        let mut msg = format!(
            "Updated to {} — but with warnings:\n  • {}",
            if tag.is_empty() {
                "latest".to_string()
            } else {
                tag.clone()
            },
            joined
        );
        if msg.len() > 4096 {
            msg.truncate(4093);
            msg.push_str("...");
        }
        Ok(msg)
    }
}

/// Reads the per-boot identifier used to verify that an update rebooted.
fn read_boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub async fn get_version() -> (StatusCode, Json<serde_json::Value>) {
    let version = env!("CARGO_PKG_VERSION");
    let sbc_model = get_sbc_model();

    let installed = std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .unwrap_or_else(|_| version.to_string());

    (StatusCode::OK, Json(serde_json::json!({
        "version": installed.trim(),
        "binary_version": version,
        "sbc_model": sbc_model,
        // The version file changes before reboot, so clients compare boot IDs.
        "boot_id": read_boot_id(),
    })))
}

/// Parses a semver tag, handling prerelease and edge cases.
pub(crate) fn parse_semver(v: &str) -> Option<(u32, u32, u32, String)> {
    let v = v.trim().trim_start_matches('v');
    let (base, pre) = match v.find('-') {
        Some(i) => (&v[..i], v[i + 1..].to_string()),
        None => (v, String::new()),
    };
    let parts: Vec<&str> = base.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let mut nums = [0u32; 3];
    for (i, p) in parts.iter().take(3).enumerate() {
        if p.is_empty() || !p.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        nums[i] = p.parse().ok()?;
    }
    Some((nums[0], nums[1], nums[2], pre))
}

/// True if `candidate` is newer than `current`. Prerelease-aware:
/// stable beats prerelease at the same base version.
pub(crate) fn is_version_newer(candidate: &str, current: &str) -> bool {
    let c = parse_semver(candidate);
    let u = parse_semver(current);
    let (c, u) = match (c, u) {
        (Some(c), Some(u)) => (c, u),
        _ => return candidate.trim() != current.trim(),
    };
    if c.0 != u.0 {
        return c.0 > u.0;
    }
    if c.1 != u.1 {
        return c.1 > u.1;
    }
    if c.2 != u.2 {
        return c.2 > u.2;
    }
    match (u.3.is_empty(), c.3.is_empty()) {
        (true, true) => false,
        (false, true) => true,   // Stable supersedes prerelease at the same version.
        (true, false) => false,
        (false, false) => c.3 > u.3,
    }
}

fn read_current_version() -> String {
    std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// Plain-HTTP endpoints used only for their `Date` response header.
///
/// The update source comes first; the connectivity endpoints are the
/// fallback for networks that block GitHub. These stay on `http://` on
/// purpose — a board whose clock reads 1970 cannot complete a TLS
/// handshake, because every server certificate looks not-yet-valid, so
/// an HTTPS time probe would fail exactly when it is needed.
const HTTP_DATE_SOURCES: [&str; 3] = [
    "http://api.github.com/",
    "http://cp.cloudflare.com/generate_204",
    "http://connectivitycheck.gstatic.com/generate_204",
];

/// Parses an HTTP `Date` header (RFC 9110 IMF-fixdate) into epoch
/// milliseconds. Obsolete RFC 2822 spellings are accepted as a fallback.
fn parse_http_date_ms(value: &str) -> Option<i64> {
    let v = value.trim();
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(v, "%a, %d %b %Y %H:%M:%S GMT") {
        return Some(naive.and_utc().timestamp_millis());
    }
    chrono::DateTime::parse_from_rfc2822(v)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Hands a `Date` response header to the clock crate. Best effort: an
/// absent, unparseable or implausible header just leaves the clock alone.
fn apply_http_date(value: Option<&str>, source: &str) {
    let Some(raw) = value else {
        return;
    };
    match parse_http_date_ms(raw) {
        Some(ms) => {
            if sentryusb_clock::maybe_set_clock_from_network(ms, source) {
                tracing::info!("[clock] system time set from the {source} Date header");
            }
        }
        None => tracing::debug!("[clock] unparseable Date header from {source}: {raw:?}"),
    }
}

/// Reads the wall clock from the first responsive `Date` header.
async fn fetch_http_date_ms() -> Option<i64> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        // A 301 to https still carries the header we want, and following
        // it would need the TLS handshake we are trying to avoid.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("sentryusb-updater/", env!("CARGO_PKG_VERSION")))
        .build()
        .ok()?;

    for url in HTTP_DATE_SOURCES {
        let resp = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("[clock] date probe {} failed: {}", url, e);
                continue;
            }
        };
        let Some(raw) = resp
            .headers()
            .get(reqwest::header::DATE)
            .and_then(|v| v.to_str().ok())
        else {
            tracing::debug!("[clock] date probe {} returned no Date header", url);
            continue;
        };
        match parse_http_date_ms(raw) {
            Some(ms) => {
                tracing::debug!("[clock] date probe {} returned {:?}", url, raw);
                return Some(ms);
            }
            None => tracing::debug!("[clock] date probe {} sent unparseable Date {:?}", url, raw),
        }
    }
    None
}

/// Best-effort clock correction for boards with no RTC.
///
/// Pi 4 and earlier have no RTC at all, and a Pi 5 only exposes one with
/// a battery fitted, so after a power cut fake-hwclock restores the time
/// of the last clean shutdown — plausible-looking but arbitrarily stale.
/// The update check is a moment we are already expected to be online, so
/// it is the natural place to take a real wall clock from the network.
///
/// Never propagates failure: a wrong clock degrades the update check, but
/// a failed time probe must not.
async fn sync_clock_for_rtcless_board() {
    // A synced NTP client or a battery-backed RTC holding a credible time
    // already owns the clock; a /dev/rtc* node sitting at the epoch does not.
    if !sentryusb_clock::needs_network_time() {
        return;
    }

    // Prefer the Pi's normal NTP path (systemd-timesyncd) before any HTTP.
    let _ = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(3),
        "timedatectl",
        &["set-ntp", "true"],
    )
    .await;
    let _ = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(3),
        "systemctl",
        &["start", "systemd-timesyncd"],
    )
    .await;
    for _ in 0..6 {
        // Only the timesyncd marker ends the wait. `clock_is_credible` is
        // already true whenever fake-hwclock restored a stale-but-plausible
        // time, which is the exact case the HTTP fallback below exists for.
        if sentryusb_clock::ntp_synced() {
            tracing::info!("[clock] clock became credible via NTP before update-check");
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }

    let Some(ms) = fetch_http_date_ms().await else {
        tracing::debug!("[clock] no HTTP date source reachable; leaving clock alone");
        return;
    };
    // Network path: skip a credible RTC, then the shared setter's
    // plausibility window and 5-min threshold. Vehicle BLE does not
    // use this wrapper.
    if sentryusb_clock::maybe_set_clock_from_network(ms, "http date") {
        tracing::info!("[clock] system time set from HTTP date on an RTC-less board");
    }
}

/// Checks GitHub releases and returns both legacy and current response fields.
pub async fn check_for_update(
    State(_s): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Runs before the GitHub call: TLS to api.github.com fails outright
    // while the clock is unset, and `checked_at` below would be wrong.
    sync_clock_for_rtcless_board().await;

    let current = read_current_version();
    let can_update = !current.is_empty() && current != "dev";

    // Either the request or saved channel can enable prerelease discovery.
    let mut include_prerelease = params.get("include_prerelease").map(String::as_str) == Some("true");
    if !include_prerelease {
        let prefs = crate::preferences::load_prefs();
        if prefs.get("update_channel").and_then(|v| v.as_str()) == Some("prerelease") {
            include_prerelease = true;
        }
    }

    let releases = match fetch_releases().await {
        Ok(rs) => rs,
        Err(msg) => {
            // Update-check telemetry is independent of GitHub availability.
            let cur_clone = current.clone();
            tokio::spawn(async move { send_telemetry(&cur_clone, false, "").await });

            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "available": false,
                    "update_available": false,
                    "error": msg,
                })),
            );
        }
    };

    let (latest_stable, latest_prerelease) = find_latest_releases(&releases);

    let mut result = serde_json::json!({
        "current_version": current,
        "checked_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    });

    let mut new_stable_version = String::new();

    // A prerelease installation may always revert to the latest stable.
    let on_prerelease = parse_semver(&current)
        .map(|(_, _, _, pre)| !pre.is_empty())
        .unwrap_or(false);

    if let Some(stable) = latest_stable {
        let stable_available = can_update && is_version_newer(&stable.tag_name, &current);
        result["update_available"] = serde_json::Value::Bool(stable_available);
        result["latest_version"] = serde_json::Value::String(stable.tag_name.clone());
        result["release_url"] = serde_json::Value::String(stable.html_url.clone());
        result["release_notes"] = serde_json::Value::String(stable.body.clone());
        result["stable"] = serde_json::json!({
            "version": stable.tag_name,
            "release_url": stable.html_url,
            "release_notes": stable.body,
            "available": stable_available,
        });
        if stable_available {
            new_stable_version = stable.tag_name.clone();
        }

        if on_prerelease && can_update && !stable_available {
            result["revert_stable"] = serde_json::json!({
                "version": stable.tag_name,
                "release_url": stable.html_url,
                "release_notes": stable.body,
            });
        }
    } else {
        result["update_available"] = serde_json::Value::Bool(false);
    }

    if include_prerelease {
        if let Some(pre) = latest_prerelease {
            let pre_available = can_update && is_version_newer(&pre.tag_name, &current);
            result["prerelease"] = serde_json::json!({
                "version": pre.tag_name,
                "release_url": pre.html_url,
                "release_notes": pre.body,
                "available": pre_available,
            });
        }
    }

    // Cache the result for subsequent Settings loads.
    if let Ok(data) = serde_json::to_vec(&result) {
        let _ = std::fs::write(UPDATE_CHECK_CACHE, data);
    }

    // Report stable update availability only.
    let cur_clone = current.clone();
    let new_ver_clone = new_stable_version.clone();
    tokio::spawn(async move {
        send_telemetry(&cur_clone, !new_ver_clone.is_empty(), &new_ver_clone).await;
    });

    (StatusCode::OK, Json(result))
}

#[derive(Clone)]
struct ReleaseInfo {
    tag_name: String,
    html_url: String,
    body: String,
    prerelease: bool,
    draft: bool,
}

/// Fetches recent stable and prerelease records from GitHub.
async fn fetch_releases() -> Result<Vec<ReleaseInfo>, String> {
    let url = format!("https://api.github.com/repos/{}/releases?per_page=20", update_repo());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent(concat!("sentryusb-updater/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| format!("http client init failed: {}", e))?;

    let resp = client.get(&url).send().await.map_err(|e| {
        if e.is_timeout() {
            "GitHub API request timed out".to_string()
        } else if e.is_connect() {
            format!("could not reach GitHub: {}", e)
        } else {
            format!("GitHub API request failed: {}", e)
        }
    })?;

    // Once TLS succeeded, GitHub's own Date is the best wall clock this path
    // sees. It still helps a board whose time was stale but not so stale that
    // the certificate looked invalid.
    apply_http_date(
        resp.headers().get(reqwest::header::DATE).and_then(|v| v.to_str().ok()),
        "github-date",
    );

    let status = resp.status();
    if !status.is_success() {
        return Err(if status.as_u16() == 403 || status.as_u16() == 429 {
            "GitHub API rate limit hit — wait about an hour and try again".to_string()
        } else {
            format!("GitHub API returned HTTP {}", status)
        });
    }

    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("GitHub API returned unparseable JSON: {}", e))?;

    let arr = body
        .as_array()
        .ok_or_else(|| "GitHub API response was not an array".to_string())?;

    Ok(arr
        .iter()
        .map(|v| ReleaseInfo {
            tag_name: v.get("tag_name").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            html_url: v.get("html_url").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            body: v.get("body").and_then(|s| s.as_str()).unwrap_or("").to_string(),
            prerelease: v.get("prerelease").and_then(|s| s.as_bool()).unwrap_or(false),
            draft: v.get("draft").and_then(|s| s.as_bool()).unwrap_or(false),
        })
        .filter(|r| !r.tag_name.is_empty())
        .collect())
}

/// Selects the first non-draft stable and prerelease from GitHub's newest-first list.
fn find_latest_releases(releases: &[ReleaseInfo]) -> (Option<&ReleaseInfo>, Option<&ReleaseInfo>) {
    let mut stable: Option<&ReleaseInfo> = None;
    let mut prerelease: Option<&ReleaseInfo> = None;
    for r in releases {
        if r.draft {
            continue;
        }
        if r.prerelease {
            if prerelease.is_none() {
                prerelease = Some(r);
            }
        } else if stable.is_none() {
            stable = Some(r);
        }
        if stable.is_some() && prerelease.is_some() {
            break;
        }
    }
    (stable, prerelease)
}

/// Persistent per-install marker for the aggregate install beacon.
const INSTALL_BEACON_MARKER: &str = "/mutable/.beaconed";

/// Sends best-effort update telemetry. A device fingerprint is included only
/// when `analytics_opt_in` is true; otherwise the payload is identifier-free.
pub async fn send_telemetry(current: &str, update_available: bool, new_version: &str) {
    let opt_in = crate::preferences::load_prefs()
        .get("analytics_opt_in")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let arch = sentryusb_shell::run("uname", &["-m"])
        .await
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| std::env::consts::ARCH.to_string());

    let mut payload = serde_json::json!({
        "current_version": current,
        "update_available": update_available,
        "new_version": new_version,
        "arch": arch,
        "model": get_sbc_model(),
    });

    if opt_in {
        let fp = get_fingerprint();
        if !fp.is_empty() {
            payload["fingerprint"] = serde_json::Value::String(fp.to_string());
        }
    }

    let url = "https://api.sentry-six.com/sentryusb/telemetry";
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    match client.post(url).json(&payload).send().await {
        Ok(r) => tracing::info!(
            "[telemetry] sent (status {}, mode={})",
            r.status(),
            if opt_in { "opt-in" } else { "opted-out" }
        ),
        Err(e) => tracing::warn!("[telemetry] failed: {}", e),
    }
}

/// Sends one identifier-free install beacon, guarded by a persistent marker.
pub fn spawn_install_beacon() {
    tokio::spawn(async move {
        if std::path::Path::new(INSTALL_BEACON_MARKER).exists() {
            return;
        }
        // Leave the marker absent after transient failure so the next boot retries.
        let url = "https://api.sentry-six.com/sentryusb/install-beacon";
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        for attempt in 1..=3 {
            match client.post(url).send().await {
                Ok(r) if r.status().is_success() => {
                    let _ = std::fs::write(INSTALL_BEACON_MARKER, b"1");
                    tracing::info!("[beacon] install counted");
                    return;
                }
                Ok(r) => {
                    tracing::warn!("[beacon] non-success status {}", r.status());
                    // Retry server errors only.
                    if !r.status().is_server_error() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::warn!("[beacon] attempt {} failed: {}", attempt, e);
                }
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_secs(5 * attempt)).await;
            }
        }
    });
}

/// Returns the last cached update check; live progress uses the WebSocket channel.
pub async fn get_update_status(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    match std::fs::read_to_string(UPDATE_CHECK_CACHE) {
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => (StatusCode::OK, Json(v)),
            Err(_) => (
                StatusCode::OK,
                Json(serde_json::json!({"update_available": false})),
            ),
        },
        Err(_) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "update_available": false,
                "checked_at": "",
            })),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_imf_fixdate_from_github() {
        // The exact spelling GitHub and Cloudflare send.
        let ms = parse_http_date_ms("Sun, 23 Aug 2026 14:05:09 GMT").unwrap();
        assert_eq!(ms, 1_787_493_909_000);
    }

    #[test]
    fn parses_obsolete_rfc2822_offset_form() {
        let ms = parse_http_date_ms("Sun, 23 Aug 2026 14:05:09 +0000").unwrap();
        assert_eq!(ms, 1_787_493_909_000);
    }

    #[test]
    fn rejects_unparseable_date_headers() {
        assert!(parse_http_date_ms("").is_none());
        assert!(parse_http_date_ms("not a date").is_none());
        // Unix `date` output, which is not an HTTP-date.
        assert!(parse_http_date_ms("Sun Aug 23 14:05:09 UTC 2026").is_none());
    }

    #[test]
    fn parsed_dates_pass_the_clock_plausibility_gate() {
        let good = parse_http_date_ms("Sun, 23 Aug 2026 14:05:09 GMT").unwrap();
        assert!(sentryusb_clock::is_plausible_ms(good));
        // A server stuck at the epoch must never be allowed to set the clock.
        let bad = parse_http_date_ms("Thu, 01 Jan 1970 00:00:00 GMT").unwrap();
        assert!(!sentryusb_clock::is_plausible_ms(bad));
    }
}
