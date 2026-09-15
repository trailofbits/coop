# shellcheck shell=bash
set -euo pipefail

echo '  [guest] Installing codex-account shortcut...'
cat >/usr/local/bin/codex-account <<'CODEXACCOUNTEOF'
#!/usr/bin/env bash
set -euo pipefail

CODEX_BIN="/usr/local/bin/codex"
CODEX_CONFIG="${CODEX_HOME:-$HOME/.codex}/config.toml"
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

# Every terminal, nested invocation and desktop uses the PAM/systemd user bus.
# The helper ignores old COOP_CODEX_ACCOUNT_* markers and verifies live state.
/usr/local/bin/codex-keyring >/dev/null
export DBUS_SESSION_BUS_ADDRESS="unix:path=${XDG_RUNTIME_DIR:?}/bus"
export GNOME_KEYRING_CONTROL="$XDG_RUNTIME_DIR/keyring"

# Codex 0.154.0 can reuse a desktop daemon on another D-Bus session when
# there are no explicit config overrides. Keep terminal auth on the keyring
# shared by the guest user. Prepend the default so caller overrides retain precedence.
exec "$CODEX_BIN" -c 'cli_auth_credentials_store="keyring"' "$@"
CODEXACCOUNTEOF
chmod 755 /usr/local/bin/codex-account
