#!/bin/bash -eu
#
# Pre-install script to make things look sufficiently like what
# the main Raspberry Pi centric install scripts expect.
#

if [[ $EUID -ne 0 ]]
then
  echo "STOP: Run sudo -i."
  exit 1
fi

if [ ! -L /sentryusb ]
then
  rm -rf /sentryusb
  if [ -d /boot/firmware ] && findmnt --fstab /boot/firmware &> /dev/null
  then
    ln -s /boot/firmware /sentryusb
  else
    ln -s /boot /sentryusb
  fi
fi

function error_exit {
  echo "STOP: $*"
  exit 1
}

function flash_rapidly {
  for led in /sys/class/leds/*
  do 
    if [ -e "$led/trigger" ]
    then
      if ! grep -q timer "$led/trigger"
      then
        modprobe ledtrig-timer || echo "timer LED trigger unavailable"
      fi
      echo timer > "$led/trigger" || true
      if [ -e "$led/delay_off" ]
      then
        echo 150 > "$led/delay_off" || true
        echo 50 > "$led/delay_on" || true
      fi
    fi
  done
}

rootpart=$(findmnt -n -o SOURCE /)
rootname=$(lsblk -no pkname "${rootpart}")
rootdev="/dev/${rootname}"
marker="/root/RESIZE_ATTEMPTED"
resize_result="/root/RESIZE_RESULT"

# Check if a previous initramfs resize left a result marker
if [ -f "$resize_result" ]
then
  result_content=$(cat "$resize_result")
  rm -f "$resize_result"
  case "$result_content" in
    success)
      echo "Root filesystem resize completed successfully during boot."
      rm -f "$marker"
      ;;
    fail:e2fsck:*)
      exit_code="${result_content##*:}"
      rm -f "$marker"
      error_exit "Root filesystem check (e2fsck) failed with exit code $exit_code during boot. The filesystem may be corrupted. Run 'e2fsck -f $rootpart' manually from a recovery environment."
      ;;
    fail:resize2fs:*)
      exit_code="${result_content##*:}"
      rm -f "$marker"
      error_exit "Root filesystem resize (resize2fs) failed with exit code $exit_code during boot. The filesystem may be too fragmented. Try running 'resize2fs $rootpart 3G' manually."
      ;;
    *)
      echo "WARNING: Unrecognized resize result: $result_content"
      rm -f "$marker"
      ;;
  esac
fi

# Check that the root partition is the last one.
lastpart=$(sfdisk -q -l "$rootdev" | tail +2 | sort -n -k 2 | tail -1 | awk '{print $1}')

# Check if there is sufficient unpartitioned space after the last
# partition to create the backingfiles and mutable partitions.
unpart=$(sfdisk -F "$rootdev" | grep -o '[0-9]* bytes' | head -1 | awk '{print $1}')
if [ "${1:-}" != "norootshrink" ] && [ "$unpart" -lt  $(( (1<<30) * 32)) ]
then
  # This script will only shrink the root partition, and if there's another
  # partition following the root partition, we won't be able to grow the
  # unpartitioned space at the end of the disk by shrinking the root partition.
  if [ "$rootpart" != "$lastpart" ]
  then
    error_exit "Insufficient unpartioned space, and root partition is not the last partition."
  fi

  # There is insufficient unpartitioned space.
  # Check if we've already shrunk the root filesystem, and shrink the root
  # partition to match if it hasn't been already

  devsectorsize=$(cat "/sys/block/${rootname}/queue/hw_sector_size")
  read -r fsblockcount fsblocksize < <(tune2fs -l "${rootpart}" | grep "Block count:\|Block size:" | awk ' {print $2}' FS=: | tr -d ' ' | tr '\n' ' ' | (cat; echo))
  fsnumsectors=$((fsblockcount * fsblocksize / devsectorsize))
  partnumsectors=$(sfdisk -q -l -o Sectors "${rootdev}" | tail +2 | sort -n | tail -1)
  partnumsectors=$((partnumsectors - 1));
  if [ "$partnumsectors" -le "$fsnumsectors" ]
  then
    if [ -f "$marker" ]
    then
      if [ -t 0 ]
      then
        echo "Previous resize attempt failed. Retrying..."
        rm -f "$marker"
      else
        error_exit "Previous resize attempt failed. Delete $marker before retrying."
      fi
    fi
    touch "$marker"

    # Calculate a safe resize target: current usage + 2G headroom, minimum 6G
    used_kb=$(df --output=used -k / | tail -1 | tr -d ' ')
    target_gb=$(( (used_kb / 1024 / 1024) + 2 ))
    if [ "$target_gb" -lt 6 ]
    then
      target_gb=6
    fi
    echo "Root filesystem uses ~$((used_kb / 1024 / 1024))G, will shrink to ${target_gb}G"

    echo "insufficient unpartitioned space, attempting to shrink root file system"

    cat <<- EOF > /etc/rc.local
		#!/bin/bash
		{
		  while ! curl -s https://raw.githubusercontent.com/Scottmg1/Sentry-USB/main-dev/setup/generic/install.sh
		  do
		    sleep 1
		  done
		} | bash
		EOF
    chmod a+x /etc/rc.local

    INITRD_NAME="initrd.img-$(uname -r)"
    # On Bookworm the boot partition is /boot/firmware/, not /boot/.
    # The bootloader loads files relative to the boot partition, so the
    # initramfs must live there, but update-initramfs writes to /boot/.
    BOOT_PART="$(readlink -f /sentryusb)"
    if [ ! -e "${BOOT_PART}/${INITRD_NAME}" ] && [ ! -e "/boot/${INITRD_NAME}" ]
    then
      # This device did not boot using an initramfs. If we're running
      # Raspberry Pi OS, we can switch it over to using initramfs first,
      # then revert back after.
      if [ -f /etc/os-release ] && grep -q Raspbian /etc/os-release && [ -e /sentryusb/config.txt ]
      then
        echo "Temporarily switching Raspberry Pi OS to use initramfs"
        update-initramfs -c -k "$(uname -r)"
        echo "initramfs ${INITRD_NAME} followkernel # SENTRYUSB-REMOVE" >> /sentryusb/config.txt
      else
        error_exit "can't automatically shrink root partition for this OS, please shrink it manually before proceeding"
      fi
    fi
    # Ensure initramfs is on the boot partition where the bootloader can find it
    if [ "/boot" != "${BOOT_PART}" ] && [ -e "/boot/${INITRD_NAME}" ]
    then
      cp "/boot/${INITRD_NAME}" "${BOOT_PART}/${INITRD_NAME}"
    fi

    {
      while ! curl -s https://raw.githubusercontent.com/Scottmg1/Sentry-USB/main-dev/tools/debian-resizefs.sh
      do
        sleep 1
      done
    } | bash -s "${target_gb}G"
    exit 0
  fi
  rm -f "$marker"
  # shrink root partition to match root file system size
  echo "shrinking root partition to match root fs, $fsnumsectors sectors"
  sleep 3
  rootpartstartsector=$(sfdisk -q -l -o Start "${rootdev}" | tail +2 | sort -n | tail -1)
  partnum=$(echo "$rootpart" | grep -o '[0-9]*$')

  echo "${rootpartstartsector},${fsnumsectors}" | sfdisk --force "${rootdev}" -N "${partnum}"

  if [ -e /sentryusb/config.txt ] && grep -q SENTRYUSB-REMOVE /sentryusb/config.txt
  then
    # switch Raspberry Pi OS back to not using initramfs
    sed -i '/SENTRYUSB-REMOVE/d' /sentryusb/config.txt
    rm -rf "/boot/initrd.img-$(uname -r)"
  else
    # restore initramfs without the resize code that debian-resizefs.sh added
    update-initramfs -u
  fi

  reboot
  exit 0
fi

# Copy the sample config file from github
if [ ! -e /sentryusb/sentryusb.conf ] && [ ! -e /root/sentryusb.conf ]
then
  while ! curl -o /sentryusb/sentryusb.conf https://raw.githubusercontent.com/Scottmg1/Sentry-USB/main-dev/pi-gen-sources/00-sentryusb-tweaks/files/sentryusb.conf.sample
  do
    sleep 1
  done
fi

# Download wifi config template (only needed for pre-Bookworm systems using wpa_supplicant)
if ! systemctl -q is-enabled NetworkManager.service 2>/dev/null
then
  if [ ! -e /sentryusb/wpa_supplicant.conf.sample ]
  then
    while ! curl -o /sentryusb/wpa_supplicant.conf.sample https://raw.githubusercontent.com/Scottmg1/Sentry-USB/main-dev/pi-gen-sources/00-sentryusb-tweaks/files/wpa_supplicant.conf.sample
    do
      sleep 1
    done
  fi
fi

# The user should have configured networking manually, so disable wifi setup
touch /sentryusb/WIFI_ENABLED

# Copy our rc.local from github, which will allow setup to
# continue using the regular "one step setup" process used
# for setting up a Raspberry Pi with the prebuilt image
rm -f /etc/rc.local
while ! curl -o /etc/rc.local https://raw.githubusercontent.com/Scottmg1/Sentry-USB/main-dev/pi-gen-sources/00-sentryusb-tweaks/files/rc.local
do
  sleep 1
done
chmod a+x /etc/rc.local

if [ ! -x "$(command -v dos2unix)" ]
then
  apt install -y dos2unix
fi

if [ ! -x "$(command -v sntp)" ] && [ ! -x "$(command -v ntpdig)" ]
then
  apt install -y sntp || apt install -y ntpsec-ntpdig
fi

if [ ! -x "$(command -v parted)" ]
then
  apt install -y parted
fi

if [ ! -x "$(command -v fdisk)" ]
then
  apt install -y fdisk
fi

if [ ! -x "$(command -v sudo)" ]
then
  apt install -y sudo
fi


# indicate we're waiting for the user to log in and finish setup
flash_rapidly

# If there is a user with id 1000, assume it is the default user
# the user will be logging in as.
DEFUSER=$(grep ":1000:1000:" /etc/passwd | awk -F : '{print $1}')
if [ -n "$DEFUSER" ]
then
  if [ ! -e "/home/$DEFUSER/.bashrc" ] || ! grep -q "SETUP_FINISHED" "/home/$DEFUSER/.bashrc"
  then
    cat <<- EOF >> "/home/$DEFUSER/.bashrc"
		if [ ! -e /sentryusb/SENTRYUSB_SETUP_FINISHED ]
		then
		  echo "+--------------------------------------------+"
		  echo "| To continue SentryUSB setup, run 'sudo -i' |"
		  echo "+-------------------------------------------+"
		fi
	EOF
    chown "$DEFUSER:$DEFUSER" "/home/$DEFUSER/.bashrc"
  fi
fi

if ! grep -q "SETUP_FINISHED" /root/.bashrc
then
  cat <<- EOF >> /root/.bashrc
	if [ ! -e /sentryusb/SENTRYUSB_SETUP_FINISHED ]
	then
	  echo "+------------------------------------------------------------------------+"
	  echo "| To continue SentryUSB setup, edit the file                             |"
	  echo "| /root/sentryusb.conf with your favorite editor, e.g.                  |"
	  echo "| 'nano /root/sentryusb.conf' and fill in the required variables.        |"
	  echo "| in the required variables. Instructions are in the file, and at        |"
	  echo "| https://github.com/Scottmg1/Sentry-USB/blob/main-dev/doc/OneStepSetup.md  |"
	  echo "| (though ignore the Raspberry Pi specific bits about flashing and       |"
	  echo "| mounting the sd card on a PC)                                          |"
	  echo "|                                                                        |"
	  echo "| When done, save changes and run /etc/rc.local                          |"
	  echo "+------------------------------------------------------------------------+"
	fi
	EOF
fi

# hack to print the above message without duplicating it here
grep -A 12 SETUP_FINISHED .bashrc  | grep echo | while read line; do eval "$line"; done
