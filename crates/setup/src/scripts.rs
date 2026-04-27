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
# Thin wrapper around `sentryusb snapshot make` — kept because
# archiveloop and external scripts call this path by filename.
# Forwards "$@" so `make_snapshot.sh nofsck` reaches the Rust binary
# which actually handles the flag (skips the loop-mount + fsck pass).
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

/// autofs map script for `/tmp/snapshots` — resolves snap-NNN names to the
/// right disk image + fstype for on-demand read-only mounts.
const AUTO_SENTRYUSB: &str = r#"#!/bin/dash

diskimage="/backingfiles/snapshots/$1/snap.bin"
mountpoint="/backingfiles/snapshots/$1/mnt"
optfile="${diskimage}.opts"

case $1 in
  snap-*)
    ;;
  *)
    exit 1
    ;;
esac

if [ ! -r "$diskimage" ]
then
  /root/bin/release_snapshot.sh "$1"
  exit 1
fi

if [ ! -L "$mountpoint" ] && [ -d "$mountpoint" ]
then
  rmdir "$mountpoint"
  ln -s "/tmp/snapshots/$1" "$mountpoint"
fi

if [ ! -f "$optfile" ]
then
  rm -rf "$optfile"
  /root/bin/mountoptsforimage "${diskimage}" | {
    read -r fstype opts
    echo "-fstype=${fstype},ro,${opts} :${diskimage}" > "$optfile"
  }
fi

cat "$optfile"
"#;

/// autofs map script for `/var/www/html/fs` — resolves Music/LightShow/Boombox
/// to the corresponding backing disk image with an rw mount.
const AUTO_WWW: &str = r#"#!/bin/dash

case "$1" in
  Music)
    diskimage="/backingfiles/music_disk.bin"
    ;;
  LightShow)
    diskimage="/backingfiles/lightshow_disk.bin"
    ;;
  Boombox)
    diskimage="/backingfiles/boombox_disk.bin"
    ;;
  *)
    exit 1
    ;;
esac

optfile="${diskimage}.opts"

if [ ! -r "$diskimage" ]
then
  exit 1
fi

if [ -f "$optfile" ] && [ "$diskimage" -nt "$optfile" ]
then
  rm -f "$optfile"
fi

if [ ! -f "$optfile" ]
then
  rm -rf "$optfile"
  /root/bin/mountoptsforimage "${diskimage}" | {
    read -r fstype opts
    if [ -z "$fstype" ]
    then
      exit 1
    fi
    echo "-fstype=${fstype},rw,${opts} :${diskimage}" > "$optfile"
  }
fi

cat "$optfile"
"#;

// ── archiveloop + supporting bash scripts ──────────────────────────────────
//
// Pulled in via `include_str!` from the vendored `run/` tree at compile
// time. Before this, the Rust setup runner only wrote out the small
// helper scripts above and silently relied on the Go-era pi-gen image
// having pre-installed `archiveloop`, `archive-clips.sh`, etc. Anyone
// running `curl | bash install-pi.sh` on a clean Pi OS would end up
// with a working binary, a perfectly-formatted /root/sentryusb.conf,
// systemd-archive enabled… and an empty /root/bin/ where the script
// the service tries to exec is supposed to live. Service crashloops,
// no archive ever runs.

const ARCHIVELOOP: &str = include_str!("../../../run/archiveloop");
const POST_ARCHIVE_PROCESS: &str = include_str!("../../../run/post-archive-process.sh");
const AWAKE_START: &str = include_str!("../../../run/awake_start");
const AWAKE_STOP: &str = include_str!("../../../run/awake_stop");
const SEND_LIVE_ACTIVITY: &str = include_str!("../../../run/send-live-activity");
const SEND_PUSH_MESSAGE: &str = include_str!("../../../run/send-push-message");
const TEMPERATURE_MONITOR: &str = include_str!("../../../run/temperature_monitor");
const WAITFORIDLE: &str = include_str!("../../../run/waitforidle");

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
        ("auto.sentryusb", AUTO_SENTRYUSB),
        ("auto.www", AUTO_WWW),
        // Archive flow — these are the universal scripts that don't depend
        // on which archive system the user picked. The per-system variants
        // (archive-clips.sh, archive-is-reachable.sh, connect-archive.sh,
        // disconnect-archive.sh, copy-music.sh, verify-and-configure-
        // archive.sh) are installed by `archive::install_archive_scripts`
        // based on ARCHIVE_SYSTEM, since each system has its own copy.
        ("archiveloop", ARCHIVELOOP),
        ("post-archive-process.sh", POST_ARCHIVE_PROCESS),
        ("awake_start", AWAKE_START),
        ("awake_stop", AWAKE_STOP),
        ("send-live-activity", SEND_LIVE_ACTIVITY),
        ("send-push-message", SEND_PUSH_MESSAGE),
        ("temperature_monitor", TEMPERATURE_MONITOR),
        ("waitforidle", WAITFORIDLE),
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
