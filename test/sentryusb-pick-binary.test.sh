#!/bin/bash
# Regression coverage for auxiliary helper symlink repair on read-only roots.
set -euo pipefail

picker=${1:-pi-gen-sources/00-sentryusb-tweaks/files/sentryusb-pick-binary}
picker=$(cd "$(dirname "$picker")" && pwd)/$(basename "$picker")
tmp=$(mktemp -d)
trap 'chmod 755 "$tmp/install" "$tmp/root-bin" 2>/dev/null || true; rm -rf "$tmp"' EXIT

install="$tmp/install"
root_bin="$tmp/root-bin"
suffix=linux-arm64-a72
mkdir -p "$install" "$root_bin"

make_executable() {
  printf '#!/bin/sh\nexit 0\n' > "$1"
  chmod +x "$1"
}

make_executable "$install/sentryusb-$suffix"
make_executable "$install/sentryusb-tesla-telemetry-$suffix"
make_executable "$install/sentryusb-ble-action-$suffix"

run_picker() {
  SENTRYUSB_INSTALL_DIR="$install" \
  SENTRYUSB_ROOT_BIN="$root_bin" \
  SENTRYUSB_PICK_LOG_STDERR=1 \
    bash "$picker"
}

run_picker >"$tmp/first.out" 2>&1
[ "$(readlink "$install/sentryusb-current")" = "$install/sentryusb-$suffix" ]
[ "$(readlink "$root_bin/sentryusb-tesla-telemetry")" = "$install/sentryusb-tesla-telemetry-$suffix" ]
[ "$(readlink "$root_bin/sentryusb-ble-action")" = "$install/sentryusb-ble-action-$suffix" ]
[ -x "$root_bin/sentryusb-ble-action" ]

# Correct links must not be rewritten on the normal read-only boot path.
chmod 555 "$install" "$root_bin"
run_picker >"$tmp/readonly.out" 2>&1
if grep -qE 'FATAL|ERROR|dangling' "$tmp/readonly.out"; then
  echo "correct read-only links were treated as failures" >&2
  exit 1
fi
chmod 755 "$install" "$root_bin"

# A dangling link is not healthy and must be reported, not logged as repaired.
rm -f "$install/sentryusb-ble-action-$suffix"
run_picker >"$tmp/dangling.out" 2>&1
grep -q 'sentryusb-ble-action is a dangling symlink' "$tmp/dangling.out"
if grep -q "sentryusb-ble-action ->" "$tmp/dangling.out"; then
  echo "dangling helper was falsely logged as repaired" >&2
  exit 1
fi

# Once the expected binary appears, the picker repairs the dangling link.
make_executable "$install/sentryusb-ble-action-$suffix"
run_picker >"$tmp/repaired.out" 2>&1
[ -x "$root_bin/sentryusb-ble-action" ]
grep -q "sentryusb-ble-action -> $install/sentryusb-ble-action-$suffix" "$tmp/repaired.out"

# A helper for a newer CPU can still satisfy -x on the current host even though
# executing it would SIGILL. With no compatible fallback, remove that unsafe
# symlink instead of silently leaving it active.
rm -f "$install/sentryusb-ble-action-$suffix"
newer_suffix=linux-arm64-a76
make_executable "$install/sentryusb-ble-action-$newer_suffix"
ln -sfn "$install/sentryusb-ble-action-$newer_suffix" "$root_bin/sentryusb-ble-action"
run_picker >"$tmp/incompatible.out" 2>&1
[ ! -e "$root_bin/sentryusb-ble-action" ]
grep -q 'removed incompatible helper link' "$tmp/incompatible.out"

echo "sentryusb picker auxiliary-link tests passed"
