#!/bin/bash -eu

ping -q -w 1 -c 1 "$1" > /dev/null 2>&1
