# shellcheck shell=bash
set -euo pipefail

echo '  [guest] Installing codex-account shortcut...'
cat >/usr/local/bin/codex-account <<'CODEXACCOUNTEOF'
#!/usr/bin/env bash
set -euo pipefail

CODEX_BIN="/usr/local/bin/codex"
CODEX_CONFIG="${CODEX_HOME:-$HOME/.codex}/config.toml"
KEYRING_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/keyrings"
PROBE_SERVICE="coop-codex"
PROBE_ACCOUNT="keyring-probe"

die() {
    echo "codex-account: $*" >&2
    exit 1
}

# coop writes `cli_auth_credentials_store = "keyring"` into the guest Codex
# config only under `[codex] auth = "chatgpt"`. Every other mode reads
# credentials from auth.json, where the D-Bus/keyring session is pure overhead
# (and its password prompt is an outright regression). Gating on the config the
# guest actually has lets every Codex entry point route through this wrapper.
keyring_mode_in() {
    local config=$1
    local first_line=""
    [ -r "$config" ] \
        && IFS= read -r first_line < "$config" \
        && [ "$first_line" = 'cli_auth_credentials_store = "keyring"' ]
}

keyring_mode() {
    keyring_mode_in "$CODEX_CONFIG"
}

keyring_exists() {
    compgen -G "$KEYRING_DIR/*.keyring" >/dev/null 2>&1
}

# secret-tool's stderr from the last probe, so the failure path can report the
# real cause instead of only guessing at the password.
PROBE_ERROR=""

# Writing proves the collection is both reachable and unlocked; a lookup can
# succeed against a locked collection. The probe item is cleared again so
# repeated launches do not accumulate junk in the user's keyring.
probe_keyring() {
    PROBE_ERROR="$(printf 'ok' \
        | timeout 5 secret-tool store \
            --label="coop Codex keyring probe" \
            service "$PROBE_SERVICE" \
            account "$PROBE_ACCOUNT" \
            2>&1 >/dev/null)" || return 1
    timeout 5 secret-tool clear \
        service "$PROBE_SERVICE" \
        account "$PROBE_ACCOUNT" \
        >/dev/null 2>&1 || true
}

unlock_keyring() {
    local creating=0 password confirm output
    keyring_exists || creating=1

    if [ ! -t 0 ]; then
        die "Codex ChatGPT auth needs an interactive TTY to unlock the guest keyring"
    fi

    # On a fresh guest there is no keyring yet, so this prompt is choosing a
    # password rather than entering one. Say so, and confirm it — an
    # unnoticed typo would otherwise lock the credentials behind a password
    # the user cannot reproduce on the next launch.
    if [ "$creating" = 1 ]; then
        printf '%s\n' \
            'codex-account: this VM has no guest keyring yet.' \
            'Choose a password to create one. It encrypts the Codex account' \
            'credentials stored inside the guest, and later `coop codex` runs' \
            'ask for it again. It is not your ChatGPT or host password.' >&2
    fi

    if ! IFS= read -rsp "Codex keyring password: " password; then
        echo >&2
        die "failed to read keyring password"
    fi
    echo >&2

    if [ -z "$password" ]; then
        die "keyring password must not be empty"
    fi

    if [ "$creating" = 1 ]; then
        if ! IFS= read -rsp "Confirm keyring password: " confirm; then
            echo >&2
            die "failed to read keyring password"
        fi
        echo >&2
        if [ "$password" != "$confirm" ]; then
            die "passwords did not match; no keyring was created"
        fi
    fi

    # Let the daemon's stderr reach the terminal. Once it forks it redirects
    # its own fd 2, so this surfaces only its startup diagnostics — the probe
    # below is what reports a wrong password.
    output="$(printf '%s' "$password" \
        | timeout 30 gnome-keyring-daemon --unlock --components=secrets)" \
        || die "failed to unlock the guest keyring"

    # The daemon prints its session env as `NAME=value` lines. Take only
    # those, and export them: an unfiltered `eval` would execute any GLib
    # diagnostic the daemon puts on stdout as a command, and a bare
    # assignment would not reach Codex anyway.
    while IFS= read -r line; do
        case "$line" in
            [A-Za-z_]*=*) export "${line%%=*}=${line#*=}" ;;
        esac
    done <<<"$output"
}

if [ ! -x "$CODEX_BIN" ]; then
    die "$CODEX_BIN is missing; rebuild the coop image"
fi

# coop stages and maintains Codex state only in ~/.codex. In ChatGPT account
# mode, an explicit CODEX_HOME could put auth.json outside coop's cleanup path
# (including under /workspace, which is pulled back to the host). Refuse the
# unsupported override before inspecting its config. coop writes the managed
# credential-store key as the first line, so a same-named nested key cannot
# trigger this guard.
if [ -n "${CODEX_HOME:-}" ] \
    && keyring_mode_in "$HOME/.codex/config.toml"; then
    die "CODEX_HOME is set, but coop manages ~/.codex and it selects the keyring credential store; unset CODEX_HOME for Codex ChatGPT account auth"
fi

if ! keyring_mode; then
    exec "$CODEX_BIN" "$@"
fi

# The keyring speaks D-Bus, and a headless SSH session has no session bus.
# Re-exec under one, using the env guard to avoid recursing forever.
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

# A nested codex-account — an in-guest agent shelling out to `codex-account` or
# `codex-yolo` — inherits this bus and its already-unlocked keyring. Unlocking
# again there would fail, for the same reason the ordering below matters, so
# reuse the session instead of prompting a second time.
if [ "${COOP_CODEX_ACCOUNT_UNLOCKED:-0}" = "1" ]; then
    probe_keyring \
        || die "inherited guest Secret Service session is unusable${PROBE_ERROR:+: $PROBE_ERROR}"
else
    # Unlock before anything else touches the bus. `gnome-keyring-daemon
    # --unlock` only creates and unlocks the login collection when it is the
    # process that starts the daemon; once any daemon owns
    # `org.freedesktop.secrets` it hands the unlock to the graphical
    # gcr-prompter, which cannot render on a headless guest and exits
    # immediately. That includes a daemon the probe itself would D-Bus-activate,
    # so there is no cheap "is it already unlocked?" check to make first.
    unlock_keyring
    probe_keyring \
        || die "guest Secret Service is unavailable${PROBE_ERROR:+: $PROBE_ERROR}; check the keyring password"
    export COOP_CODEX_ACCOUNT_UNLOCKED=1
fi

exec "$CODEX_BIN" "$@"
CODEXACCOUNTEOF
chmod 755 /usr/local/bin/codex-account
