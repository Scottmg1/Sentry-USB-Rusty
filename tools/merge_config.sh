#!/bin/bash -eu

usage () {
  echo "Usage: $0 existingconfig blanknewconfig"
}

if [[ "${1:-}" = "" ]] || [[ "${2:-}" = "" ]] || [[ ! -r "$1" ]] || [[ ! -r "$2" ]]
then
  usage
  exit 1
fi

function getvarname () {
  local line="$@"
  local var=""
  if [[ "$line" =~ ^[[:space:]]*#?[[:space:]]*export[[:space:]]+([A-Za-z_][A-Za-z0-9_]*)= ]]
  then
    var="${BASH_REMATCH[1]}"
  else
    echo "Unexpected error parsing: $line" >&2
    exit 1
  fi
  echo "$var"
}

declare -A uncommented_outputs

while read -r line
do
  if [[ $line =~ ^export* ]] || [[ $line =~ ^#export* ]] || [[ $line =~ ^#\ export* ]]
  then
    # for each "export VAR=value" line in the current sample config:
    # existing user config    current sample config   action
    # nonexistent             exists                  copy template
    # exact match w/ template -                       copy
    # commented               commented               copy template
    # commented               uncommented             copy template add comment
    # uncommented             -                       copy existing
    var=$(getvarname "$line")
    exact=$(grep -m 1 "$line" "$1" || true)
    if [[ "$exact" = "$line" ]]
    then
      echo "$exact"
      continue
    fi
    existing=$(grep -m 1 "export $var" "$1" || true)
    if [[ -z "$existing" ]]
    then
      # setting doesn't exist in existing config
      echo "$line"
    fi
    if [[ $line == \#*export* ]] && [[ $existing == \#*export* ]]
    then
      # setting is commented in both user config and template
      echo "$line"
    fi
    if [[ $line =~ ^export* ]] && [[ $existing == \#*export* ]]
    then
      # uncommented in template but commented in user config
      echo "# $line"
    fi
    if [[ $existing =~ ^export* ]]
    then
      # uncommented in user config
      if [[ -v uncommented_outputs[$var] ]] && [[ $line == \#*export* ]]
      then
        # we already output this once, so this time
        # just output the template line
        echo "$line"
      else
        echo "$existing"
        uncommented_outputs[$var]=yes
      fi
    fi
  else
    echo "$line"
  fi
done < "$2"
