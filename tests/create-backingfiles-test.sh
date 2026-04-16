#!/bin/bash

if [[ $EUID -ne 0 ]]
then
  echo "STOP: Run this script as root."
  exit 1
fi

if [ "$(systemd-detect-virt)" == "none" ]
then
  echo "WARNING, it is recommended to run this script in a VM."
  echo "Press enter to continue, or ctrl-c to abort."
  read
fi

BACKINGFILES_FOLDER=$(mktemp -d backingfilestestXXX)
LOG="$BACKINGFILES_FOLDER-log.txt"

function checksuccess {
  name=$(printf '%6s %6s %6s %6s %6s' "$1" "$2" "$3" "$4" "$6")
  if ../setup/pi/create-backingfiles.sh "$@"
  then
    printf '%-45s %s\n' "$name" OK
    printf '%-45s %s\n' "$name" OK > /dev/tty
  else
    printf '%-45s %s\n' "$name" FAIL
    printf '%-45s %s\n' "$name" FAIL > /dev/tty
    SUCCESS=false
  fi
}

SUCCESS=true

FALLOCATE=$(which fallocate)

function fallocate {
  if [ "$1" != "-l" ]
  then
    echo "Can't emulate fallocate invocation with args " "$@"
    exit 1
  fi
  truncate -s "$2" "$3"
}

if ! $FALLOCATE -l 1M "$BACKINGFILES_FOLDER/test.bin"
then
  echo "emulating fallocate using truncate"
  export -f fallocate
fi
rm "$BACKINGFILES_FOLDER/test.bin"

{
  for cam in 30G 40G
  do
    for music in 0 4G
    do
      for lightshow in 0 100M 1G
      do
        for boombox in 0 100M
        do
          for exfat in true false
          do
            checksuccess "$cam" "$music" "$lightshow" "$boombox" "$BACKINGFILES_FOLDER" "$exfat" < /dev/null
	    find "$BACKINGFILES_FOLDER" -type f | xargs rm
          done
        done
      done
    done
  done


} &>> "$LOG"

if [ "$SUCCESS" = "true" ]
then
  rm -rf "$BACKINGFILES_FOLDER" "$LOG"
else
  echo "One or more tests failed, see $LOG for details."
  exit 1
fi
