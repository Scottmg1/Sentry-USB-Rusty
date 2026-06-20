//! OTA update: check for updates, run update, version info.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;

use crate::router::AppState;
use crate::status::get_sbc_model;

/// Cache file written by `check_for_update`, read by `get_update_status` so
/// the Settings page can render last-check results on load without forcing
/// a network round-trip.
const UPDATE_CHECK_CACHE: &str = "/tmp/sentryusb-update-check.json";

static UPDATE_RUNNING: AtomicBool = AtomicBool::new(false);

/// Salt for the telemetry fingerprint hash. Must match Go `telemetrySalt`.
const TELEMETRY_SALT: &str = "SENTRYUSB_2026_PROD";

/// SHA-256 hash of a stable hardware identifier + salt. Uses the SBC serial
/// number (survives reflash) with fallback to machine-id. Cached.
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

    // Port 443 works on Pi-hole networks (Pi-hole blocks port 53 for non-Pi-hole DNS).
    // Race two probes so we succeed as soon as either connects.
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

/// POST /api/system/update
///
/// Body (optional): `{"version": "vX.Y.Z"}` — install a specific release.
/// Empty body / missing version → install whatever `/releases/latest`
/// currently points to (backward-compatible "install latest" path).
///
/// On success the daemon broadcasts `complete` → `restarting` and then
/// shells out to `reboot` ~3 s later, so the new binary is running by the
/// time the user's tab reconnects. The 3 s gap is what lets the client
/// mount the restart modal before the WebSocket goes away.
pub async fn run_update(
    State(s): State<AppState>,
    body: String,
) -> (StatusCode, Json<serde_json::Value>) {
    if UPDATE_RUNNING.swap(true, Ordering::SeqCst) {
        return crate::json_error(StatusCode::CONFLICT, "Update already in progress");
    }

    // Frontend conditionally attaches the body only when targetVersion is set
    // (Settings.tsx:1597), so an empty string is the "install latest" case.
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

        let result = self_update(target_version).await;

        UPDATE_RUNNING.store(false, Ordering::SeqCst);

        match result {
            Ok(msg) => {
                hub.broadcast("update_status", &serde_json::json!({
                    "status": "complete",
                    "output": msg
                }));

                // Give the WS message a moment to land, then announce the restart and reboot.
                // The 3 s wait between `restarting` and `reboot` lets the modal mount on the
                // client before the WebSocket dies.
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

/// Default GitHub source for OTA updates when the config doesn't override it.
const DEFAULT_UPDATE_OWNER: &str = "Sentry-Six";
const DEFAULT_UPDATE_REPO_NAME: &str = "Sentry-USB-Rusty";

/// Resolve the `owner/repo` slug for OTA updates. Honors `REPO` from the
/// active sentryusb.conf (with the legacy hardcoded default as fallback)
/// so a user running a fork can point self-update at their own releases
/// via the wizard's Advanced → Update Source field. `REPO_NAME` stays
/// hardcoded — forks must keep the original repo name.
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

/// Detect the release suffix matching the currently-running CPU variant.
///
/// Three-tier resolution:
///   1. `/opt/sentryusb/active-variant` — written by the boot picker
///      (sentryusb-pick-binary). If present, this is authoritative — it's
///      exactly the variant that's running right now, so re-downloading
///      the same suffix guarantees the update lands on a binary the picker
///      will pick again.
///   2. Live CPU detection mirroring the picker's rules (HWCAP atomics →
///      a76, CPU part 0xD08 → a72, else a53). Used when the picker hasn't
///      written the active-variant file yet (e.g., during the first
///      migration update from an old single-binary install).
///   3. Architecture-family fallback via dpkg/uname for armv7/amd64
///      — those targets don't have per-CPU variants.
///
/// On Pi OS a 64-bit kernel can be paired with a 32-bit (armhf) userspace,
/// in which case `uname -m` reports `aarch64` but the aarch64 binary can't
/// actually load — exec returns ENOENT because the dynamic linker
/// `/lib/ld-linux-aarch64.so.1` isn't installed. Trust dpkg first when
/// determining the architecture family.
async fn detect_release_suffix() -> anyhow::Result<String> {
    // Tier 1: ask the picker what it chose at boot. Only trust values
    // that are real release suffixes — old picker versions recorded
    // whatever they ended up RUNNING (their on-disk fallback, or even
    // "legacy"), and building download URLs from that either 404s or
    // permanently installs the wrong CPU variant (issue #88's second
    // act). Anything else falls through to live detection below.
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

    // Tier 3 first (cheap arch-family check) — gates whether we even
    // need to do per-CPU detection. If we're on armv7/amd64, there's
    // only one variant per family. armv6 (armel / Pi Zero W / Pi 1) is
    // no longer supported and errors out here so the user sees a
    // diagnosable failure instead of a 404 on the download.
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

    // Tier 2: aarch64 per-CPU detection — mirrors sentryusb-pick-binary's
    // rules so an updater-side detection on a pre-picker install lands on
    // the same variant the picker would have chosen.
    debug_assert_eq!(family, "aarch64");
    if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
        // HWCAP_ATOMICS = LSE = ARMv8.1+ = Cortex-A76 and newer. The a76
        // build also keeps the ARMv8 crypto extension enabled (Pi 5 has
        // it), so require the `aes` hwcap too — a v8.1+ board without
        // crypto must get the a72 build instead of SIGILLing in SHA/AES.
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

async fn self_update(target_version: Option<String>) -> anyhow::Result<String> {
    let suffix = detect_release_suffix().await?;
    let repo = update_repo();

    // Build the download URL — tag-specific if a target version was requested
    // (Revert to Stable / Install Pre-release), otherwise the latest release.
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

    // HEAD-check the binary exists before downloading so a typo'd version or
    // a release that didn't get a binary uploaded surfaces as a clear error
    // instead of an empty mv'd file.
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

    // Remount root read-write. TeslaUSB-style images mount / read-only
    // and put the writable portion behind an overlay or a per-script
    // `remountfs_rw` helper. Try the helper first (which handles the
    // overlay correctly on those setups), fall back to plain
    // `mount -o remount,rw /` for vanilla rootfs images, and only then
    // try the legacy `mount / -o remount,rw` ordering that used to be
    // here. None of these are fatal individually — we try all three so
    // at least one succeeds on every install layout. (Previously the
    // single `mount / -o remount,rw` call silently failed on some
    // images, which then caused every downstream `mv` into /root/bin
    // to fail without surfacing an error — that's the root cause of
    // the "UI says updated to v3.3.1 but binary on disk is still
    // v3.3.0" bug we hit on the Rock Pi 4C+ tester.)
    let _ = sentryusb_shell::run("/root/bin/remountfs_rw", &[]).await;
    let _ = sentryusb_shell::run("mount", &["-o", "remount,rw", "/"]).await;
    let _ = sentryusb_shell::run("mount", &["/", "-o", "remount,rw"]).await;

    // Stage the download on the SAME filesystem as the destination so the
    // mv below is an atomic rename(2). The old staging path was /tmp
    // (tmpfs): mv across filesystems falls back to unlink-dest + copy,
    // and a power cut mid-copy — routine on a Pi that loses power the
    // moment the car cuts accessory — left a partial (or no) binary at
    // /opt/sentryusb and a service that can't start on the next boot.
    // A power cut mid-download now only orphans the hidden .new file;
    // the running binary is untouched until the rename. Bonus: the
    // ~15 MB binary no longer transits tmpfs RAM on a 1 GB device.
    sentryusb_shell::run("mkdir", &["-p", "/opt/sentryusb"]).await?;
    let tmp = "/opt/sentryusb/.sentryusb-update.new";
    sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(120),
        "curl", &["-fsSL", &url, "-o", tmp],
    ).await?;

    sentryusb_shell::run("chmod", &["+x", tmp]).await?;

    // Write to the per-variant path so the picker symlink keeps resolving
    // to a valid binary. Layout:
    //   /opt/sentryusb/sentryusb-{suffix}            ← we write here
    //   /opt/sentryusb/sentryusb-current → ↑         ← picker symlink
    //   /opt/sentryusb/sentryusb         → -current  ← back-compat symlink
    //
    // Detection: if /opt/sentryusb/sentryusb-current exists (new layout),
    // write to the variant path. Otherwise we're on a pre-multi-binary
    // install — write to the legacy /opt/sentryusb/sentryusb path so the
    // existing systemd unit still finds the binary. (The next install-pi.sh
    // run will migrate the layout.)
    let dest = if std::path::Path::new("/opt/sentryusb/sentryusb-current").exists() {
        format!("/opt/sentryusb/sentryusb-{}", suffix)
    } else {
        "/opt/sentryusb/sentryusb".to_string()
    };
    sentryusb_shell::run("mv", &[tmp, &dest]).await?;

    // ── Tesla BLE telemetry sampler binary ──
    //
    // Pulled from the same release as the main binary so the schema
    // version the sampler writes is locked to the schema the main
    // binary expects. Best-effort: if the release doesn't include
    // the telemetry binary (older release, unfinished CI) the update
    // succeeds anyway and the sampler service stays inactive via its
    // ConditionPathExists guard. Same arch-suffix, same repo, parallel
    // URL shape — kept here rather than in migrate.rs so a single
    // update pulls both binaries in lockstep.
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
    // Track per-binary install outcomes so the response message
    // tells the user exactly what landed and what didn't. Previously
    // these were all `let _ = ...` which silently swallowed failures
    // — a read-only /root/bin (TeslaUSB safety pattern) would make
    // the mv fail but the response still said "Updated to v3.3.1",
    // leaving the user with v3.3.0 on disk and v3.3.1 in the UI.
    let mut install_warnings: Vec<String> = Vec::new();
    let head_ok = sentryusb_shell::run_with_timeout(
        std::time::Duration::from_secs(15),
        "curl",
        &["-sfI", "--max-time", "10", &telemetry_url],
    )
    .await
    .is_ok();
    if head_ok {
        // Staged next to its destination (not /tmp) so the mv below is an
        // atomic same-fs rename — see the main-binary staging note above.
        let telemetry_tmp = "/opt/sentryusb/.sentryusb-tesla-telemetry-update.new";
        match sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(120),
            "curl",
            &["-fsSL", &telemetry_url, "-o", telemetry_tmp],
        )
        .await
        {
            Ok(_) => {
                // mkdir + chmod failures are tolerable individually
                // (the dirs likely exist; non-executable still gets
                // executed if we fix perms later). The mv is the
                // one we MUST surface — if it fails the binary on
                // disk doesn't get replaced.
                //
                // Install layout: the binary lands at its per-CPU
                // variant path under /opt/sentryusb and /root/bin
                // becomes a symlink to it — the same scheme the boot
                // picker (sentryusb-pick-binary) manages for the main
                // binary. The picker re-validates the symlink every
                // boot, so a wrong-variant binary (SD card moved to a
                // different Pi model) self-heals instead of SIGILL
                // crash-looping forever (issue #88). `ln -sfn` also
                // migrates legacy installs in place: it atomically
                // replaces the old regular file at /root/bin.
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
                // mv first, ln only on success — a failed mv must NOT
                // re-point /root/bin at a path that doesn't exist (that
                // would replace a working legacy binary with a dangling
                // symlink).
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
                        // Service file is installed by migrate.rs
                        // (sentryusb's startup script). Restart here
                        // so the freshly-installed binary picks up
                        // immediately rather than waiting for the
                        // post-reboot start.
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
                        // Clean up the temp file so it doesn't sit
                        // around eating /tmp forever.
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
        // Release doesn't have a telemetry binary at this URL. This
        // is a legitimate skip on older releases that predate the
        // crate; surface it at info level only (no warning) since
        // it's not actionable for the user.
        tracing::info!(
            "release does not include a telemetry binary at {} — skipping",
            telemetry_url
        );
    }

    // ── BLE-action one-shot CLI ──
    //
    // Replaces the tesla-control shell-outs in run/awake_start
    // (wake / sentry-mode / charge-port). Pulled from the same
    // release as the main binary so action wire format stays in
    // lockstep with whatever crypto/protocol changes ship together.
    // Same best-effort pattern as the telemetry fetch above —
    // missing artifact (older release) is a no-op rather than an
    // update failure.
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
        // Staged next to its destination for the same atomic-rename
        // reason as the two binaries above.
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
                // Same variant-path + picker-managed-symlink layout as
                // the telemetry binary above; see that block for the
                // issue #88 rationale. mv first, ln only on success.
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
                // No service to restart — awake_start invokes it on demand.
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

    // Determine the tag to record. Use the requested target if any (it
    // matches the binary we just installed); otherwise resolve /latest.
    // Resolve via the shared HTTP client, like check_for_update already
    // does — this was the last curl|grep|sed bash pipeline left here, and
    // it interpolated the (config-controlled) repo name into a shell
    // string. reqwest + serde needs no quoting at all.
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

    // Roll any per-binary install warnings into the user-visible
    // success message. Without this the response was always "Updated
    // to vX.Y.Z" regardless of whether the auxiliary binaries (telemetry
    // sampler, ble-action CLI) actually landed on disk — causing the
    // exact "UI says updated but binary on disk is the old one"
    // confusion we hit on the Rock Pi 4C+ tester. Surface failures
    // here so the user knows to investigate (usually: read-only
    // rootfs needs a remount, or a release missing one of the assets).
    // ── Re-apply install-time patches that must survive an OTA swap ──
    //
    // The standalone /usr/local/bin/sentryusb-apply-runtime-patches script
    // re-applies things the binary swap can't own — e.g. the BCM4345C0
    // non-fatal-adv patch to /root/bin/sentryusb-ble.py on Rock 4C+ which
    // otherwise crash-loops the BLE daemon after every update. The script
    // is idempotent + detection-gated, so it's a no-op on non-applicable
    // boards and a no-op on already-patched files.
    //
    // Bootstrap: the script first ships in v3.11.4. Users updating from a
    // pre-v3.11.4 release won't have it on disk yet — fetch it from the
    // repo BEFORE running so the very first auto-heal cycle works. The
    // script lives at a stable URL (main branch, setup/pi/) so it's
    // fetchable as long as the repo is reachable.
    let patches_path = "/usr/local/bin/sentryusb-apply-runtime-patches";
    if !std::path::Path::new(patches_path).exists() {
        let patches_url = format!(
            "https://raw.githubusercontent.com/{}/main/setup/pi/apply-runtime-patches.sh",
            repo
        );
        tracing::info!(
            "update.rs: runtime-patches script missing — bootstrapping from {}",
            patches_url
        );
        match sentryusb_shell::run_with_timeout(
            std::time::Duration::from_secs(20),
            "curl",
            &[
                "-fsSL",
                "--max-time",
                "15",
                "-o",
                patches_path,
                &patches_url,
            ],
        )
        .await
        {
            Ok(_) => {
                let _ = sentryusb_shell::run("chmod", &["+x", patches_path]).await;
                tracing::info!("update.rs: runtime-patches script bootstrapped");
            }
            Err(e) => install_warnings.push(format!(
                "runtime-patches bootstrap FAILED ({e}): board-specific fixes \
                 (BCM4345C0 BLE on Rock 4C+, etc.) won't auto-reapply after this \
                 update. Re-run install-pi.sh manually if BLE pairing breaks."
            )),
        }
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
        // Log full detail to the journal for ops, return a condensed
        // version to the UI (4kB cap so a flood of warnings doesn't
        // blow up the WebSocket message).
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

/// GET /api/system/version
pub async fn get_version(State(_s): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let version = env!("CARGO_PKG_VERSION");
    let sbc_model = get_sbc_model();

    // Read installed version tag if available (installer writes it here).
    let installed = std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .unwrap_or_else(|_| version.to_string());

    (StatusCode::OK, Json(serde_json::json!({
        "version": installed.trim(),
        "binary_version": version,
        "sbc_model": sbc_model,
    })))
}

/// Parse semver string like "v1.2.3" or "v1.2.3-beta.1" → (major, minor, patch, prerelease).
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
        (false, true) => true,   // user on prerelease, candidate stable → newer
        (true, false) => false,  // user on stable, candidate prerelease → older
        (false, false) => c.3 > u.3,
    }
}

fn read_current_version() -> String {
    std::fs::read_to_string("/opt/sentryusb/version")
        .or_else(|_| std::fs::read_to_string("/root/.sentryusb_version"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string())
}

/// POST /api/system/check-update
///
/// Fetches the GitHub "latest release" JSON via reqwest and parses it
/// properly. The previous implementation shelled to `curl | grep | head`
/// which hid curl failures (pipeline exit code is `head`'s, always 0
/// on empty input) — a 403 rate limit or DNS blip would silently
/// return `available: false` and the UI would tell the user they were
/// up to date when they weren't.
///
/// The response shape carries both the simple fields (`available`,
/// `latest`, `current`) kept for backward compatibility with earlier
/// Rust clients **and** the richer fields the current web UI reads
/// (`update_available`, `latest_version`, `release_url`,
/// `release_notes`). Settings.tsx checks for `data.update_available`
/// / `data.latest_version`; without them the UI defaults to "up to
/// date" regardless of the actual result. This was the root cause of
/// the user-reported "update never appears" bug even when the backend
/// correctly found a newer release.
pub async fn check_for_update(
    State(_s): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> (StatusCode, Json<serde_json::Value>) {
    let current = read_current_version();
    let can_update = !current.is_empty() && current != "dev";

    // Include prereleases if requested via query param OR if the user's
    // update_channel preference is set to "prerelease".
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
            // Fire a basic telemetry heartbeat so the support server still sees
            // the device when GitHub is unreachable.
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

    // Detect whether the user is currently on a prerelease so we can offer
    // the latest stable as a downgrade option when no forward upgrade is
    // available.
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

        // If user is on a prerelease and the latest stable isn't flagged as
        // a newer version (e.g. prerelease has a higher base version), offer
        // the stable release as a revert/downgrade option.
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

    // Cache the result so the Settings page load can render last-check info
    // without re-hitting GitHub.
    if let Ok(data) = serde_json::to_vec(&result) {
        let _ = std::fs::write(UPDATE_CHECK_CACHE, data);
    }

    // Telemetry — only report stable updates, never prereleases.
    let cur_clone = current.clone();
    let new_ver_clone = new_stable_version.clone();
    tokio::spawn(async move {
        send_telemetry(&cur_clone, !new_ver_clone.is_empty(), &new_ver_clone).await;
    });

    (StatusCode::OK, Json(result))
}

/// Minimal release info parsed from a GitHub release object.
#[derive(Clone)]
struct ReleaseInfo {
    tag_name: String,
    html_url: String,
    body: String,
    prerelease: bool,
    draft: bool,
}

/// Fetch the most recent releases (stable + prerelease) from GitHub.
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

/// Pick the first stable and the first prerelease from the list. Mirrors
/// Go's `findLatestReleases` — assumes the GitHub API returns releases in
/// publish-newest-first order. Draft releases are skipped.
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

/// Marker file. Once it exists, the install beacon has fired for this
/// install and won't fire again. Lives under `/mutable/` so it survives
/// SentryUSB updates but resets on a full SD-card reflash (which is
/// indistinguishable from a fresh install anyway).
const INSTALL_BEACON_MARKER: &str = "/mutable/.beaconed";

/// POST update-check telemetry to the support server. The payload always
/// carries `{current_version, update_available, new_version, arch, model}`.
/// A device fingerprint is included **only** if the user has explicitly
/// opted in via the `analytics_opt_in` preference (set by the setup wizard
/// or Settings → Privacy). This is the GDPR Art. 6(1)(a) consent gate —
/// without an opt-in, the backend treats the call as an opted-out heartbeat
/// (no DB row, IP-rate-limited).
///
/// Best-effort — errors are logged, never surfaced to the caller.
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

/// Fire the anonymous install beacon exactly once per install. The beacon
/// POSTs an **empty body** to `/sentryusb/install-beacon` — no fingerprint,
/// no identifier, nothing. The backend just increments a daily counter.
/// This is what gives us gross-install volume independent of the opt-in
/// cohort, and it carries no personal data so there's nothing to opt out of.
///
/// Guarded by `/mutable/.beaconed` — once that file exists, the beacon
/// never fires again for this install (until /mutable is wiped, which on
/// SentryUSB only happens on a full reflash).
pub fn spawn_install_beacon() {
    tokio::spawn(async move {
        if std::path::Path::new(INSTALL_BEACON_MARKER).exists() {
            return;
        }
        // Retry on transient errors so a cold DNS cache at first boot
        // doesn't drop the beacon. Three attempts max, then give up —
        // if we can't reach the server after that, we'll just stay
        // un-beaconed and try again next boot.
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
                    // 4xx won't fix with retry; 5xx might.
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

/// GET /api/system/update-status
///
/// Returns the cached result of the last `check_for_update` call so the
/// Settings page can render last-known release info without forcing a
/// fresh GitHub round-trip on every page load.
///
/// Live install progress is delivered via the `update_status` WebSocket
/// channel (see `run_update`), not this endpoint.
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
