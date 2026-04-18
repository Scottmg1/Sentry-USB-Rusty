//! Embedded runtime scripts that get installed to /root/bin/.
//!
//! These are small shell scripts needed at runtime (after setup). They'll
//! eventually be ported to Rust subcommands, but for now they're installed
//! as bash scripts to maintain compatibility.

use anyhow::Result;

const REMOUNTFS_RW: &str = r#"#!/bin/bash
mount / -o remount,rw
for _mp in /sentryusb /boot/firmware /boot; do
  if findmnt "$_mp" > /dev/null 2>&1; then
    mount "$_mp" -o remount,rw
    break
  fi
done
"#;

const MOUNTOPTSFORIMAGE: &str = r#"#!/bin/bash -eu
source="$1"
read -r offset <<<"$(sfdisk -l -q -o START "$source" | tail -1)"
fstype=$(blkid --probe -o value -s TYPE --offset $((offset*512)) "$source")
offsetopt="offset=$((offset*512))"
timeopt="time_offset=-420"
case $fstype in
  vfat)  echo vfat "utf8,umask=000,$offsetopt,$timeopt" ;;
  exfat) echo exfat "umask=000,$offsetopt,$timeopt" ;;
  *)     echo "$fstype" "$offsetopt,$timeopt" ;;
esac
"#;

const MOUNTIMAGE: &str = r#"#!/bin/bash -eu
source="$1"
mountpoint="$2"
shift 3
opts="$*"
read -r fstype moreopts <<<"$(/root/bin/mountoptsforimage "$source")"
mount -t "$fstype" -o "$opts,$moreopts" "$source" "$mountpoint"
"#;

const MAKE_SNAPSHOT: &str = r#"#!/bin/bash -eu
# Thin wrapper — real logic is in the Rust binary.
# Kept for backward compatibility with scripts that call this.
sentryusb snapshot make "$@"
"#;

const RELEASE_SNAPSHOT: &str = r#"#!/bin/bash -eu
sentryusb snapshot release "$@"
"#;

const MANAGE_FREE_SPACE: &str = r#"#!/bin/bash -eu
sentryusb space manage "$@"
"#;

const FORCE_SYNC: &str = r#"#!/bin/bash -eu
# Force an immediate archive sync by sending SIGUSR1 to archiveloop.
pkill -USR1 -f archiveloop || echo "archiveloop not running"
"#;

const ENABLE_GADGET: &str = r#"#!/bin/bash -eu
sentryusb gadget enable "$@"
"#;

const DISABLE_GADGET: &str = r#"#!/bin/bash -eu
sentryusb gadget disable "$@"
"#;

/// Install all runtime helper scripts to /root/bin/.
///
/// Only announces a phase if at least one script is missing or has changed —
/// once installed, re-running setup is a no-op.
pub async fn install_runtime_scripts(emitter: &crate::SetupEmitter) -> Result<bool> {
    let _ = std::fs::create_dir_all("/root/bin");

    let scripts: &[(&str, &str)] = &[
        ("remountfs_rw", REMOUNTFS_RW),
        ("mountoptsforimage", MOUNTOPTSFORIMAGE),
        ("mountimage", MOUNTIMAGE),
        ("make_snapshot.sh", MAKE_SNAPSHOT),
        ("release_snapshot.sh", RELEASE_SNAPSHOT),
        ("manage_free_space.sh", MANAGE_FREE_SPACE),
        ("force_sync.sh", FORCE_SYNC),
        ("enable_gadget.sh", ENABLE_GADGET),
        ("disable_gadget.sh", DISABLE_GADGET),
    ];

    // Skip the phase entirely if every script is already present and
    // byte-for-byte identical to what we'd write.
    let all_current = scripts.iter().all(|(name, content)| {
        let path = format!("/root/bin/{}", name);
        std::fs::read_to_string(&path)
            .map(|existing| existing == *content)
            .unwrap_or(false)
    });
    if all_current {
        return Ok(false);
    }

    emitter.begin_phase("runtime_scripts", "Installing runtime scripts");
    emitter.progress("Installing runtime helper scripts...");

    for (name, content) in scripts {
        let path = format!("/root/bin/{}", name);
        std::fs::write(&path, content)?;
        let _ = sentryusb_shell::run("chmod", &["+x", &path]).await;
    }

    #[cfg(unix)]
    {
        let _ = std::os::unix::fs::symlink("/root/bin/mountimage", "/sbin/mount.sentryusb");
    }

    emitter.progress("Runtime scripts installed.");
    Ok(true)
}
