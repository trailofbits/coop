#!/usr/bin/env bash
set -euo pipefail

# Run on Linux with rsync, jq, and util-linux; no VM or root privileges needed.
# Pass a copy-files.sh path to test a deliberate regression independently.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
COPY_FILES_SCRIPT="${1:-$SCRIPT_DIR/../scripts/guest/copy-files.sh}"
TEST_ROOT="$(mktemp -d)"
trap 'rm -r -- "$TEST_ROOT"' EXIT
failures=0

for kind in directory file; do
    stage="$TEST_ROOT/$kind-stage"
    destination="$TEST_ROOT/$kind-destination"
    mkdir -p "$stage"
    if [[ $kind == directory ]]; then
        mkdir -p "$stage/payload/nested"
        source_file="$stage/payload/nested/.input"
        destination_file="$destination/nested/.input"
    else
        source_file="$stage/payload"
        destination_file="$destination"
    fi
    printf '%s\n' host-bytes >"$source_file"
    bash "$COPY_FILES_SCRIPT" "$stage" "$destination"
    cmp "$source_file" "$destination_file"
    echo "PASS $kind initial copy"

    printf '%s\n' guest-data >"$destination_file"
    touch -r "$source_file" "$destination_file"
    [[ $(cat "$destination_file") == guest-data ]]
    [[ $(stat -c '%s:%y' "$source_file") == $(stat -c '%s:%y' "$destination_file") ]]
    if [[ $kind == directory ]]; then
        printf '%s\n' retained >"$destination/nested/guest-only"
    fi

    bash "$COPY_FILES_SCRIPT" "$stage" "$destination"
    if cmp -s "$source_file" "$destination_file"; then
        echo "PASS $kind refreshes changed bytes with equal size and mtime"
    else
        echo "FAIL $kind retains changed bytes with equal size and mtime" >&2
        failures=$((failures + 1))
    fi
    if [[ $kind == directory ]]; then
        [[ $(cat "$destination/nested/guest-only") == retained ]]
        echo "PASS directory retains guest-only files"
    fi
done

[[ $failures -eq 0 ]]
