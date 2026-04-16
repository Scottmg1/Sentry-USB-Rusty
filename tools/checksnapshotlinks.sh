#!/bin/bash -eu

BASE=/mutable/TeslaCam
REPAIR=false
if [ "${1:-}" = "repair" ]
then
  REPAIR=true
fi

function recentpathfor {
  recent=${1#*/}
  filename=${recent##*/}
  filedate=${filename:0:10}
  echo "RecentClips/$filedate/$filename"
}

find -L /backingfiles/snapshots/ -type f -name \*.mp4 | sort -r | {
  while read -r path
  do
    name=${path##/*TeslaCam/}
    if [[ $name == SentryClips/* || $name == SavedClips/* ]]
    then
      if [ ! -L "$BASE/$name" ]
      then
        echo No link for "$path"
        if [ "$REPAIR" = "true" ]
        then
          dir=$BASE/$name
          dir=${dir%/*}
          mkdir -p "$dir"
          ln -sf "$path" "$BASE/$name"
        fi
      fi
      recentpath=$BASE/$(recentpathfor "$name")
      if [ ! -L "$recentpath" ]
      then
        echo No RecentClips link for "$path"
        if [ "$REPAIR" = "true" ]
        then
          recentdir=${recentpath%/*}
          mkdir -p "$recentdir"
          ln -sf "$path" "$recentpath"
        fi
      fi
    elif [[ $name == RecentClips/* ]]
    then
      recentpath=$BASE/$(recentpathfor "$name")
      if [ ! -L "$recentpath" ]
      then
        echo No link for "$path"
        if [ "$REPAIR" = "true" ]
        then
          recentdir=${recentpath%/*}
          mkdir -p "$recentdir"
          ln -sf "$path" "$recentpath"
        fi
      fi
    fi
  done
}

find /mutable/TeslaCam -xtype l | {
  while read -r path
  do
    echo "link target for '$path' does not exist"
    if [ "$REPAIR" = "true" ]
    then
      rm "$path"
    fi
  done
}
