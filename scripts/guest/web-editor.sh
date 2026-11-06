#!/bin/bash
# Executed only inside the running VM. stdin is a lease from coop.
set -euo pipefail
umask 077
export PATH=/usr/local/bin:/usr/bin:/bin
unset CODE_SERVER_CONFIG PASSWORD HASHED_PASSWORD NODE_OPTIONS NODE_PATH
stage=preparing
reason=setup_failed
staging=
child=
download=
service_attempt=0
service_started=0
config="${socket%/*}/config.yaml"
launch_marker="${socket%/*}/launched"
unit="${socket%/*}"
unit="${unit##*/}.service"
cleanup() {
    status=$?
    trap '' HUP INT TERM PIPE
    if ((status != 0)); then
        # Both fields are fixed status tokens, never command output.
        printf 'failed %s %s\n' "$stage" "$reason" || true
    fi
    if [[ -n $download ]]; then
        kill -KILL "$download" 2>/dev/null || true
        wait "$download" 2>/dev/null || true
    fi
    # KillMode=control-group also catches terminals that created new sessions.
    if ((service_attempt)); then
        sudo -n timeout 5 systemctl stop "$unit" >/dev/null 2>&1 || true
    fi
    if [[ -n $child ]]; then
        kill -TERM -- "-$child" 2>/dev/null || true
        for ((attempt=0; attempt<20; attempt++)); do
            kill -0 -- "-$child" 2>/dev/null || break
            sleep 0.05
        done
        kill -KILL -- "-$child" 2>/dev/null || true
        wait "$child" 2>/dev/null || true
    fi
    [[ -z $staging ]] || rm -rf -- "$staging"
    rm -f -- "$socket" "$session_socket" "$config" "$launch_marker"
    rmdir -- "${socket%/*}" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
printf 'preparing\n'
mkdir -- "${socket%/*}"
IFS= read -r -t 15 password
[[ $password =~ ^[a-f0-9]{64}$ ]]
lease_deadline=$((SECONDS + 15))
lease() {
    if IFS= read -r -t 1 heartbeat; then
        [[ $heartbeat == alive ]] || exit 0
        lease_deadline=$((SECONDS + 15))
    else
        read_status=$?
        # EOF cancels immediately; a read timeout still honors the lease.
        ((read_status > 128 && SECONDS < lease_deadline)) || exit 0
    fi
}
observe_launch() {
    if [[ $service_started == 0 && -f $launch_marker ]]; then
        service_started=1
        stage=code_server
        reason=exited
        printf 'launched\n'
    fi
}

version=4.145.0
reason=unsupported_architecture
case $(uname -m) in
    x86_64) arch=amd64; checksum=771bf6038658034d7caf509fcc44af50f0b8bdac5e738177b6cf9c4181dd3382 ;;
    aarch64|arm64) arch=arm64; checksum=425d3bc5167c85dc070427d2aa93760083b02aec852ea9ce75c2906c99138f45 ;;
    *) exit 64 ;;
esac
reason=project_missing
test -d "$project"
reason=setup_failed
base="$HOME/.local/share/coop-web-editor"
mkdir -p "$base"
exec 9>"$base/install.lock"
package="code-server-$version-linux-$arch"
install="$base/$package"
if [[ ! -x "$install/bin/code-server" ]]; then
    printf 'installing\n'
    stage=installing
    reason=lock_timeout
fi
flock -w 30 9
if [[ ! -x "$install/bin/code-server" ]]; then
    if [[ $stage == preparing ]]; then
        printf 'installing\n'
        stage=installing
    fi
    reason=setup_failed
    # A single staging location under the install lock bounds disk usage even
    # after guest power loss or SIGKILL bypasses the cleanup trap.
    staging="$base/install-staging"
    rm -rf -- "$staging"
    mkdir -- "$staging"
    # Keep one checksum-bound partial outside disposable extraction scratch.
    archive="$base/download-$checksum.tar.gz"
    for cached in "$base"/download-*.tar.gz; do
        [[ $cached == "$archive" ]] || rm -f -- "$cached"
    done
    [[ ! -L $archive && (! -e $archive || -f $archive) ]]
    # Keep output private and present before the first progress poll.
    : >> "$archive"
    chmod 600 -- "$archive"
    if ! printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --status; then
        stage=download
        reason=transfer_failed
        curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
            --connect-timeout 15 --max-time 1800 --max-filesize 536870912 --continue-at - \
            "https://github.com/coder/code-server/releases/download/v$version/$package.tar.gz" \
            --output "$archive" >/dev/null 2>&1 &
        download=$!
        last_progress=$SECONDS
        while kill -0 "$download" 2>/dev/null; do
            lease
            if ((SECONDS - last_progress >= 2)); then
                bytes=$(stat -c '%s' -- "$archive")
                [[ $bytes =~ ^[0-9]{1,20}$ ]]
                printf 'downloaded %s\n' "$bytes"
                last_progress=$SECONDS
            fi
        done
        if wait "$download"; then
            download=
        else
            status=$?
            download=
            case $status in
                5|6) reason=dns ;;
                7) reason=connection ;;
                22) reason=http; rm -f -- "$archive" ;;
                33|36) rm -f -- "$archive" ;; # A refused resume must allow a fresh retry.
                28) reason=timeout ;;
                35|51|58|60|77|83|90|91) reason=tls ;;
            esac
            exit "$status"
        fi
    fi
    stage=verification
    reason=checksum_mismatch
    if ! printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check --status; then
        rm -f -- "$archive"
        exit 1
    fi
    stage=extraction
    reason=archive_invalid
    tar --extract --gzip --no-same-owner --no-same-permissions --file "$archive" --directory "$staging"
    test -x "$staging/$package/bin/code-server"
    mv -- "$staging/$package" "$install"
    rm -rf -- "$staging"
    rm -f -- "$archive"
    staging=
fi
if [[ $stage != preparing ]]; then
    stage=installing
fi
reason=setup_failed
flock -u 9
mkdir -p "$base/data" "$base/extensions"
printf 'auth: password\npassword: "%s"\n' "$password" > "$config"
unset password
stage=systemd_launch
reason=launch_failed
printf 'starting\n'
# The wrapper acknowledges systemd launch only after opening the code-server
# log. No code-server output or systemd diagnostic reaches the protocol pipe.
service_attempt=1
# systemd expands environment references in argv; $$ preserves a literal dollar.
project_arg=${project//\$/\$\$}
setsid sudo -n systemd-run --quiet --collect --wait --pipe --service-type=exec \
    --unit="$unit" --uid="$(id -u)" --gid="$(id -g)" \
    --property=WorkingDirectory=/ --property=KillMode=control-group \
    --property=TimeoutStopSec=3 --setenv="HOME=$HOME" --setenv="USER=$(id -un)" \
    --setenv=PATH=/usr/local/bin:/usr/bin:/bin \
    /bin/bash -ec 'log=$1; marker=$2; shift 2; exec >"$log" 2>&1; : > "$marker"; exec "$@"' \
    bash "$base/editor.log" "$launch_marker" "$install/bin/code-server" --config "$config" \
    --auth password --socket "$socket" --socket-mode 0600 --session-socket "$session_socket" \
    --user-data-dir "$base/data" --extensions-dir "$base/extensions" \
    --disable-workspace-trust --disable-proxy --disable-telemetry --disable-update-check "$project_arg" \
    </dev/null >/dev/null 2>&1 9>&- &
child=$!
ready=0
while kill -0 "$child" 2>/dev/null; do
    observe_launch
    if [[ $service_started == 1 && $ready == 0 ]] && curl --silent --fail --max-time 1 \
        --unix-socket "$socket" http://localhost/healthz >/dev/null; then
        printf 'ready\n'
        ready=1
    fi
    lease
done
# A fast code-server exit can happen before the first liveness poll.
observe_launch
wait "$child" || true
child=
exit 1
