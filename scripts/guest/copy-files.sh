#!/bin/bash
set -euo pipefail

stage=$1
destination=$2

fail() {
    printf 'guest_files: %s\n' "$1" >&2
    exit 1
}

[[ $(realpath -m -- "$destination") == "$destination" ]] ||
    fail "destination contains a symlink: $destination"

# Detect persisted live mounts too, including on a restart without --mount.
findmnt --json -o TARGET,FSTYPE >"$stage/mounts.json"
jq -j '.. | objects | select(.fstype? == "virtiofs" or .fstype? == "9p") |
  .target, "\u0000"' "$stage/mounts.json" >"$stage/mounts"
while IFS= read -r -d '' mount; do
    if [[ $destination == "$mount" || $destination == "$mount/"* ||
        $mount == "$destination/"* ]]; then
        fail "destination overlaps a live host mount: $destination"
    fi
done <"$stage/mounts"

if [[ -e $destination ]]; then
    find "$destination" -type l -print -quit >"$stage/links"
    [[ ! -s $stage/links ]] || fail "destination contains a symlink: $destination"
fi

if [[ -d $stage/payload ]]; then
    [[ ! -e $destination || -d $destination ]] || fail "destination is not a directory"
    mkdir -p -- "$destination"
    rsync -rltp --ignore-times --chmod=Du=rwx,Dgo=,Fu=rwX,Fgo= -- "$stage/payload/" "$destination/"
else
    [[ ! -d $destination ]] || fail "file destination is a directory"
    mkdir -p -- "$(dirname -- "$destination")"
    rsync -ltp --ignore-times --chmod=Fu=rwX,Fgo= -- "$stage/payload" "$destination"
fi
