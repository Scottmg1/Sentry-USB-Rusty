#!/bin/bash
# Regression coverage for reinstalling onto an already read-only SentryUSB.
set -euo pipefail

script=${1:-install-pi.sh}
script=$(cd "$(dirname "$script")" && pwd)/$(basename "$script")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# curl | bash ignores the shebang, so strict mode must be enabled by the body.
grep -q '^set -Eeuo pipefail$' "$script"

# Install the EXIT warning before the first remount attempt: root may become RW
# and a later boot remount can still fail inside the same function.
trap_line=$(grep -n "^trap 'readonly_reinstall_exit_notice" "$script" | cut -d: -f1)
remount_line=$(grep -n '^ensure_install_filesystems_writable$' "$script" | cut -d: -f1)
[ -n "$trap_line" ] && [ -n "$remount_line" ] && [ "$trap_line" -lt "$remount_line" ]

# Functions and variables below are consumed by functions loaded via eval.
# shellcheck disable=SC2034,SC2329
load_installer_functions() {
  REPO=Sentry-Six/Sentry-USB-Rusty
  INSTALL_DIR="${SENTRYUSB_INSTALL_DIR:-/opt/sentryusb}"
  REMOUNT_HELPER="${SENTRYUSB_REMOUNT_HELPER:-/root/bin/remountfs_rw}"
  BINARY_NAME=sentryusb
  RELEASE_TAG=v-test
  RED='' GREEN='' BLUE='' YELLOW='' NC=''
  info() { echo "[INFO] $1"; }
  ok() { echo "[OK] $1"; }
  warn() { echo "[WARN] $1"; }
  error_exit() { echo "[ERROR] $1"; exit 1; }
  eval "$(awk '/^mount_is_writable\(\)/,/^}/' "$script")"
  eval "$(awk '/^ensure_install_filesystems_writable\(\)/,/^}/' "$script")"
  eval "$(awk '/^install_release_asset\(\)/,/^}/' "$script")"
  eval "$(awk '/^install_release_bundle\(\)/,/^}/' "$script")"
  eval "$(awk '/^readonly_reinstall_exit_notice\(\)/,/^}/' "$script")"
}

# A failed final move must never print OK or return success.
set +e
# shellcheck disable=SC2030
(
  export SENTRYUSB_INSTALL_DIR="$tmp/install-fail"
  load_installer_functions
  mkdir -p "$INSTALL_DIR"
  # Invoked indirectly by install_release_asset loaded via eval.
  # shellcheck disable=SC2329
  curl() {
    local output=""
    while [ "$#" -gt 0 ]; do
      if [ "$1" = -o ]; then output="$2"; shift 2; else shift; fi
    done
    printf 'downloaded payload\n' > "$output"
  }
  # shellcheck disable=SC2329
  mv() { printf 'attempt\n' >> "$tmp/mv-attempts"; return 1; }
  # shellcheck disable=SC2329
  sleep() { :; }
  install_release_asset sentryusb linux-arm64-a72
) >"$tmp/fail.out" 2>&1
status=$?
set -e
[ "$status" -ne 0 ] || { echo "failed move returned success" >&2; exit 1; }
[ "$(wc -l < "$tmp/mv-attempts")" -eq 5 ] || { echo "failed move was not retried" >&2; exit 1; }
! grep -q 'Downloaded sentryusb-linux-arm64-a72' "$tmp/fail.out" \
  || { echo "failed move printed a false OK" >&2; exit 1; }

# The happy path publishes an executable from a same-filesystem staging file.
# shellcheck disable=SC2031
(
  export SENTRYUSB_INSTALL_DIR="$tmp/install-ok"
  load_installer_functions
  mkdir -p "$INSTALL_DIR"
  # Invoked indirectly by install_release_asset loaded via eval.
  # shellcheck disable=SC2329
  curl() {
    local output=""
    while [ "$#" -gt 0 ]; do
      if [ "$1" = -o ]; then output="$2"; shift 2; else shift; fi
    done
    printf '#!/bin/sh\nexit 0\n' > "$output"
  }
  # shellcheck disable=SC2329
  sleep() { :; }
  install_release_asset sentryusb linux-arm64-a72
  [ -x "$INSTALL_DIR/sentryusb-linux-arm64-a72" ]
  [ ! -e "$INSTALL_DIR/.sentryusb-linux-arm64-a72.new" ]
) >"$tmp/success.out" 2>&1
grep -q 'Downloaded sentryusb-linux-arm64-a72' "$tmp/success.out"

# If remounting fails, stop before claiming that installation can proceed.
set +e
(
  export SENTRYUSB_REMOUNT_HELPER="$tmp/missing-remount-helper"
  load_installer_functions
  # Invoked indirectly by ensure_install_filesystems_writable loaded via eval.
  # shellcheck disable=SC2329
  findmnt() {
    if [ "${2:-}" = TARGET ]; then printf '%s\n' "${3:-/}"; else printf 'ro,noatime\n'; fi
  }
  # shellcheck disable=SC2329
  mount() { return 1; }
  ensure_install_filesystems_writable
) >"$tmp/remount.out" 2>&1
status=$?
set -e
[ "$status" -ne 0 ] || { echo "read-only remount failure returned success" >&2; exit 1; }
grep -q 'could not be remounted read-write' "$tmp/remount.out"

# Release installs must not mix latest binaries with scripts from moving main.
# shellcheck disable=SC2016
grep -q 'SOURCE_REF="${RELEASE_TAG:-main}"' "$script"
# shellcheck disable=SC2016
if grep -q 'raw.githubusercontent.com/${REPO}/main/' "$script"; then
  echo "release-managed scripts still bypass SOURCE_REF" >&2
  exit 1
fi

# A release is a protocol-compatible bundle: every CPU variant gets the main
# daemon and both native BLE helpers from the same pinned tag.
(
  load_installer_functions
  # Consumed by functions loaded dynamically above.
  # shellcheck disable=SC2034
  SUFFIXES='linux-arm64-a53 linux-arm64-a72 linux-arm64-a76'
  RELEASE_TAG=v9.9.9
  # Invoked indirectly by install_release_bundle loaded via eval.
  # shellcheck disable=SC2329
  install_release_asset() { printf '%s %s %s\n' "$RELEASE_TAG" "$1" "$2"; }
  install_release_bundle
) >"$tmp/bundle.out"
[ "$(wc -l < "$tmp/bundle.out")" -eq 9 ]
for suffix in linux-arm64-a53 linux-arm64-a72 linux-arm64-a76; do
  grep -qx "v9.9.9 sentryusb $suffix" "$tmp/bundle.out"
  grep -qx "v9.9.9 sentryusb-tesla-telemetry $suffix" "$tmp/bundle.out"
  grep -qx "v9.9.9 sentryusb-ble-action $suffix" "$tmp/bundle.out"
done

# Failure after remounting a read-only install must leave an explicit warning;
# otherwise the appliance silently remains writable until reboot.
(
  load_installer_functions
  # Consumed by readonly_reinstall_exit_notice loaded via eval.
  # shellcheck disable=SC2034
  ROOT_WAS_READONLY=1
  # shellcheck disable=SC2034
  BOOT_WAS_READONLY=0
  readonly_reinstall_exit_notice 1
) >"$tmp/exit-notice.out" 2>&1
grep -q 'installation failed after remounting' "$tmp/exit-notice.out"

echo "install-pi safety tests passed"
