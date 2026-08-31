#! /bin/bash

shopt -s globstar nullglob extglob

# Record the ShellCheck version used by CI.
shellcheck -V

# Runtime-sourced files are absent in the checkout.
shellcheck --exclude=SC1091 \
           ./install-pi.sh \
           ./setup/pi/setup-sentryusb \
           ./pi-gen-sources/00-sentryusb-tweaks/files/rc.local \
           ./pi-gen-sources/00-sentryusb-tweaks/files/sentryusb-pick-binary \
           ./run/archiveloop \
           ./run/auto.sentryusb \
           ./run/awake_start \
           ./run/awake_stop \
           ./run/mountimage \
           ./run/mountoptsforimage \
           ./run/remountfs_rw \
           ./run/send-push-message \
           ./run/temperature_monitor \
           ./run/waitforidle \
           ./test/install-pi-safety.test.sh \
           ./test/sentryusb-pick-binary.test.sh
