//! TeslaCam FUSE mount wiring — port of `configure-web.sh`.
//!
//! The Go/bash project compiled `cttseraser.cpp` here; the Rust port ships
//! a native `cttseraser` binary built separately (installed by install-pi.sh
//! or the pi-gen image at /usr/local/bin/cttseraser). This phase wires
//! the binary into the system:
//!   * writes `/sbin/mount.ctts` so fstab's `mount.ctts#` syntax resolves
//!   * enables `user_allow_other` in /etc/fuse.conf
//!   * creates the /mutable/TeslaCam source + /var/www/html/TeslaCam target
//!   * adds the fstab entry that actually mounts it
//!   * installs the `auto.www` autofs map when music/lightshow/boombox disk
//!     images exist
//!
//! Without this phase the cttseraser binary is installed but **never mounted**,
//! so Chrome's recordings bypass the ctts fixup entirely.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::SetupEmitter;

/// Path at which install-pi.sh / pi-gen drops the Rust cttseraser binary.
/// `/usr/local/bin/cttseraser` is a symlink to `/opt/sentryusb/cttseraser`.
const CTTSERASER_BIN: &str = "/usr/local/bin/cttseraser";

pub async fn configure_web_mount(emitter: &SetupEmitter) -> Result<bool> {
    // Idempotency check — if the fstab entry, mount helper, and fuse conf
    // are all already in place, we have nothing to do.
    let fstab = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let fstab_has_mount = fstab.lines().any(|l| !l.starts_with('#') && l.contains("mount.ctts#"));
    let mount_helper_ok = std::fs::read_to_string("/sbin/mount.ctts")
        .map(|c| c.contains(CTTSERASER_BIN) || c.contains("cttseraser"))
        .unwrap_or(false);
    let fuse_ok = std::fs::read_to_string("/etc/fuse.conf")
        .map(|c| c.lines().any(|l| l.trim() == "user_allow_other"))
        .unwrap_or(false);

    if fstab_has_mount && mount_helper_ok && fuse_ok {
        return Ok(false);
    }

    emitter.begin_phase("web_mount", "TeslaCam FUSE mount");
    emitter.progress("configuring web (SentryUSB mode)");

    // Install the runtime packages the Rust binary needs. We skip the
    // build toolchain (g++, libfuse-dev) that the bash script pulled in
    // purely to compile cttseraser.cpp on-device; the Rust binary is
    // already compiled for us.
    sentryusb_shell::run_with_timeout(
        Duration::from_secs(300),
        "apt-get",
        &["-y", "install", "fuse3", "net-tools", "wireless-tools", "ethtool"],
    ).await.context("failed to install FUSE + networking runtime packages")?;

    // Nginx fight — SentryUSB owns port 80.
    if sentryusb_shell::run("systemctl", &["is-active", "--quiet", "nginx"]).await.is_ok() {
        let _ = sentryusb_shell::run("systemctl", &["stop", "nginx"]).await;
    }
    if sentryusb_shell::run("systemctl", &["is-enabled", "--quiet", "nginx"]).await.is_ok() {
        let _ = sentryusb_shell::run("systemctl", &["disable", "nginx"]).await;
    }

    // `/sbin/mount.ctts` — one-line wrapper that FUSE's mount helper
    // protocol invokes via the `mount.ctts#` fstab syntax.
    std::fs::write(
        "/sbin/mount.ctts",
        format!("#!/bin/bash -eu\n{} \"$@\" -o allow_other\n", CTTSERASER_BIN),
    )?;
    let _ = sentryusb_shell::run("chmod", &["+x", "/sbin/mount.ctts"]).await;

    // Source + target dirs.
    std::fs::create_dir_all("/mutable/TeslaCam")?;
    std::fs::create_dir_all("/var/www/html/TeslaCam")?;

    // Replace any stale mount.ctts entry with the canonical one.
    add_or_replace_fstab_ctts()?;

    // Allow non-root processes to traverse the FUSE mount. Without this
    // even `read only = yes` Samba shares of /var/www/html/TeslaCam 403.
    enable_user_allow_other()?;

    // Optional auto.www autofs for music/lightshow/boombox disk images.
    if Path::new("/backingfiles/music_disk.bin").exists()
        || Path::new("/backingfiles/lightshow_disk.bin").exists()
        || Path::new("/backingfiles/boombox_disk.bin").exists()
    {
        std::fs::create_dir_all("/var/www/html/fs")?;
        std::fs::create_dir_all("/etc/auto.master.d")?;
        std::fs::write(
            "/etc/auto.master.d/www.autofs",
            "/var/www/html/fs  /root/bin/auto.www --timeout=0\n",
        )?;
        // `zip` is used by the web UI to offer bulk download of music dirs.
        let _ = sentryusb_shell::run_with_timeout(
            Duration::from_secs(180),
            "apt-get", &["-y", "install", "zip"],
        ).await;
    }

    emitter.progress("done configuring web");
    Ok(true)
}

/// Drop any existing `mount.ctts` fstab line and add the canonical one.
fn add_or_replace_fstab_ctts() -> Result<()> {
    const ENTRY: &str = "mount.ctts#/mutable/TeslaCam /var/www/html/TeslaCam fuse defaults,nofail,x-systemd.requires=/mutable 0 0";
    let content = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    let kept: Vec<&str> = content
        .lines()
        .filter(|l| !l.trim_start().starts_with("mount.ctts#"))
        .collect();
    let mut new = kept.join("\n");
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str(ENTRY);
    new.push('\n');
    std::fs::write("/etc/fstab", new)?;
    Ok(())
}

/// Uncomment `#user_allow_other` in /etc/fuse.conf, or add it if the line
/// doesn't exist. Idempotent.
fn enable_user_allow_other() -> Result<()> {
    let path = "/etc/fuse.conf";
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == "user_allow_other") {
        return Ok(());
    }

    let mut found_commented = false;
    let new: String = existing
        .lines()
        .map(|l| {
            if l.trim_start() == "#user_allow_other" {
                found_commented = true;
                "user_allow_other".to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut out = if found_commented {
        new
    } else {
        // Append if the file exists but doesn't have the option at all.
        let mut s = existing;
        if !s.is_empty() && !s.ends_with('\n') {
            s.push('\n');
        }
        s.push_str("user_allow_other");
        s
    };
    if !out.ends_with('\n') {
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(())
}
