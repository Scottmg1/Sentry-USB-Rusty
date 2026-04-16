#!/bin/bash -eu

/root/bin/disable_gadget.sh

mount /mnt/cam || true

mkdir -p /mnt/cam/TeslaCam/SentryClips

cd /mnt/cam/TeslaCam/SentryClips

dir=$(date '+%Y-%m-%d_%H-%M-%S')
mkdir "$dir"
cd "$dir"

for t in {10..1}
do
  name=$(date -d "now-${t}min" "+%Y-%m-%d_%H-%M-%S")
  for c in front back left-repeater right-repeater
  do
    fullname="$name-$c.mp4"
    if [ "$t" = "5" ]
    then
      echo "creating short file $fullname"
      fallocate -l 1K "$fullname"
    else
      echo "creating $fullname"
      fallocate -l 29M "$fullname"
    fi
  done
done

cat << EOF > event.json
{
	"timestamp":"$(date -d "now-1min" "+%Y-%m-%d_%H-%M-%S")"
	"reason":"dummy_test_event"
}
EOF
