#!/bin/bash
set -euo pipefail

exec 9>"$HOME/.coop-creation.lock"
flock -w 30 9
child=

# shellcheck disable=SC2329 # Invoked by the EXIT trap, including on signal exits.
cleanup() {
    trap '' HUP INT TERM
    if [[ -n $child ]]; then
        kill -TERM -- "-$child" 2>/dev/null || true
        for ((attempt = 0; attempt < 20; attempt++)); do
            kill -0 -- "-$child" 2>/dev/null || break
            sleep 0.05
        done
        kill -KILL -- "-$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
    fi
}

trap 'cleanup' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
# A noninteractive shell keeps its background child in the shell's process
# group, so setsid does not fork and child is also the new process-group ID.
setsid bash "$0" --command 9>&- &
child=$!
status=0
wait "$child" || status=$?
if [[ $status == 0 ]]; then
    child=
fi
exit "$status"
