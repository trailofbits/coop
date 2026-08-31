# shellcheck shell=bash
set -euo pipefail

echo '  [guest] Installing codex-account shortcut...'
cat >/usr/local/bin/codex-account <<'CODEXACCOUNTEOF'
#!/usr/bin/env bash
set -euo pipefail

CODEX_BIN="/usr/local/bin/codex"
PROBE_SERVICE="coop-codex"
PROBE_ACCOUNT="keyring-probe"

die() {
    echo "codex-account: $*" >&2
    exit 1
}

eval_env_output() {
    local output
    output="$("$@" 2>/dev/null)" || return 1
    if [ -n "$output" ]; then
        eval "$output"
    fi
}

start_keyring() {
    eval_env_output gnome-keyring-daemon --start --components=secrets || true
}

probe_keyring() {
    printf 'ok' \
        | timeout 5 secret-tool store \
            --label="coop Codex keyring probe" \
            service "$PROBE_SERVICE" \
            account "$PROBE_ACCOUNT" \
            >/dev/null 2>&1
}

unlock_keyring() {
    if [ ! -t 0 ]; then
        die "Codex ChatGPT auth needs an interactive TTY to unlock the guest keyring"
    fi

    local password output
    password=""
    if ! IFS= read -rsp "Codex keyring password: " password; then
        echo >&2
        die "failed to read keyring password"
    fi
    echo >&2

    output="$(printf '%s' "$password" \
        | gnome-keyring-daemon --unlock --components=secrets 2>/dev/null)" \
        || {
            unset password
            die "failed to unlock the guest keyring"
        }
    unset password

    if [ -n "$output" ]; then
        eval "$output"
    fi
}

if [ ! -x "$CODEX_BIN" ]; then
    die "$CODEX_BIN is missing; rebuild the coop image"
fi

if [ "${COOP_CODEX_ACCOUNT_DBUS:-0}" != "1" ]; then
    command -v dbus-run-session >/dev/null 2>&1 \
        || die "dbus-run-session is missing; install dbus-user-session or rebuild the coop image"
    export COOP_CODEX_ACCOUNT_DBUS=1
    exec dbus-run-session -- "$0" "$@"
fi

for tool in gnome-keyring-daemon secret-tool timeout; do
    command -v "$tool" >/dev/null 2>&1 \
        || die "$tool is missing; rebuild the coop image with Codex account-auth support"
done

start_keyring
if ! probe_keyring; then
    unlock_keyring
    start_keyring
    probe_keyring \
        || die "guest Secret Service is still unavailable; check the keyring password"
fi

exec "$CODEX_BIN" "$@"
CODEXACCOUNTEOF
chmod 755 /usr/local/bin/codex-account
