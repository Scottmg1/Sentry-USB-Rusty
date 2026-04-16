#!/bin/bash -eu

# macOS-compatible readlink -f replacement
resolve_path() {
  local target="$1"
  cd "$(dirname "$target")" 2>/dev/null
  target="$(basename "$target")"
  while [ -L "$target" ]; do
    target="$(readlink "$target")"
    cd "$(dirname "$target")" 2>/dev/null
    target="$(basename "$target")"
  done
  echo "$(pwd -P)/$target"
}

SRC=$(dirname "$(resolve_path "$0")")
DEST=$(cd . && pwd -P)

if [[ "$DEST" != */pi-gen ]]
then
  echo "$0 should be called from the RPi-Distro pi-gen folder"
  exit 1
fi

cp "$SRC/pi-gen-config" config
rm -rf stage2/EXPORT_NOOBS stage2/EXPORT_IMAGE export-image/01-user-rename/00-packages
mkdir -p stage_sentryusb
touch stage_sentryusb/EXPORT_IMAGE
cp stage2/prerun.sh stage_sentryusb/prerun.sh
cp -r "$SRC/00-sentryusb-tweaks" stage_sentryusb

echo 'Build config set. Now use "./build.sh" or "./build-docker.sh" to build the SentryUSB image.'


