#!/bin/bash
# Executed only inside the running VM. stdin is a lease from coop.
set -euo pipefail
umask 077
export PATH=/usr/local/bin:/usr/bin:/bin
unset CODE_SERVER_CONFIG PASSWORD HASHED_PASSWORD NODE_OPTIONS NODE_PATH
IFS= read -r -t 15 password
[[ $password =~ ^[a-f0-9]{64}$ ]]
config="${socket%/*}/config.yaml"

version=4.141.0
case $(uname -m) in
    x86_64) arch=amd64; checksum=65bcd20b524d03eb1b5adb7f360244599ef0e5b74089a46a982d32c074b97a29 ;;
    aarch64|arm64) arch=arm64; checksum=e5cc78da1d631c6cf6162f9b18eeaa8ee9d3239e1a88bb99d0602cdc5fa1852d ;;
    *) exit 64 ;;
esac
test -d /workspace
base="$HOME/.local/share/coop-web-editor"
mkdir -p "$base"
exec 9>"$base/install.lock"
flock -w 30 9
package="code-server-$version-linux-$arch"
install="$base/$package"
staging=
child=
unit="${socket%/*}"
unit="${unit##*/}.service"
cleanup() {
    trap '' HUP INT TERM
    # KillMode=control-group also catches terminals that created new sessions.
    sudo -n timeout 5 systemctl stop "$unit" >/dev/null 2>&1 || true
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
    rm -f -- "$socket" "$session_socket" "$config"
    rmdir -- "${socket%/*}" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
if [[ ! -x "$install/bin/code-server" ]]; then
    # A single staging location under the install lock bounds disk usage even
    # after guest power loss or SIGKILL bypasses the cleanup trap.
    staging="$base/install-staging"
    rm -rf -- "$staging"
    mkdir -- "$staging"
    curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --connect-timeout 15 --max-time 300 --max-filesize 536870912 \
        "https://github.com/coder/code-server/releases/download/v$version/$package.tar.gz" \
        --output "$staging/package.tar.gz"
    printf '%s  %s\n' "$checksum" "$staging/package.tar.gz" | sha256sum --check --status
    tar --extract --gzip --no-same-owner --no-same-permissions --file "$staging/package.tar.gz" --directory "$staging"
    test -x "$staging/$package/bin/code-server"
    mv -- "$staging/$package" "$install"
    rm -rf -- "$staging"
    staging=
fi
flock -u 9
mkdir -p "$base/data" "$base/extensions"
printf 'auth: password\npassword: "%s"\n' "$password" > "$config"
unset password
# A transient guest service bounds every descendant, including terminal PTYs.
# The server itself runs as the existing guest user, never as root.
setsid sudo -n systemd-run --quiet --collect --wait --pipe --service-type=exec \
    --unit="$unit" --uid="$(id -u)" --gid="$(id -g)" \
    --property=WorkingDirectory=/workspace --property=KillMode=control-group \
    --property=TimeoutStopSec=3 --setenv="HOME=$HOME" --setenv="USER=$(id -un)" \
    --setenv=PATH=/usr/local/bin:/usr/bin:/bin \
    "$install/bin/code-server" --config "$config" \
    --auth password --socket "$socket" --socket-mode 0600 --session-socket "$session_socket" \
    --user-data-dir "$base/data" --extensions-dir "$base/extensions" \
    --disable-workspace-trust --disable-proxy --disable-telemetry --disable-update-check /workspace \
    </dev/null >"$base/editor.log" 2>&1 9>&- &
child=$!
ready=0
while kill -0 "$child" 2>/dev/null; do
    IFS= read -r -t 15 heartbeat || exit 0
    [[ $heartbeat == alive ]] || exit 0
    if [[ $ready == 0 ]] && curl --silent --fail --max-time 1 \
        --unix-socket "$socket" http://localhost/healthz >/dev/null; then
        printf 'ready\n'
        ready=1
    fi
done
wait "$child"
