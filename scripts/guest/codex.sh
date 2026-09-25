# Keep the installer cleanup local when provisioning snippets are concatenated.
(
    set -euo pipefail
    : "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"

    # Profiles may supply a complete Codex pair; explicit updates replace it.
    if [ -z "${COOP_FORCE_INSTALL:-}" ] \
        && [ -x /usr/local/bin/codex ] \
        && [ -x /usr/local/bin/codex-code-mode-host ]; then
        echo '  [guest] Codex CLI and Code Mode host already installed, skipping.'
        exit 0
    fi

    echo '  [guest] Installing Codex CLI and Code Mode host with the native installer...'
    CODEX_NATIVE_BIN="/home/${GUEST_USER}/.local/bin/codex"
    CODEX_NATIVE_CODE_MODE_HOST="/home/${GUEST_USER}/.local/bin/codex-code-mode-host"
    CODEX_INSTALLER=$(mktemp)
    CODEX_LINK_TMP="/usr/local/bin/codex.new.$$"
    CODEX_CODE_MODE_HOST_LINK_TMP="/usr/local/bin/codex-code-mode-host.new.$$"
    trap 'rm -f "$CODEX_INSTALLER" "$CODEX_LINK_TMP" "$CODEX_CODE_MODE_HOST_LINK_TMP"' EXIT
    chmod 0644 "$CODEX_INSTALLER"

    # curl retries transient failures without running a partially downloaded script.
    curl -fsSL --retry 3 --retry-all-errors \
        -o "$CODEX_INSTALLER" https://chatgpt.com/codex/install.sh

    # Retain the upstream package layout and guest ownership for `codex update`.
    # PATH is already managed by coop; the installer need not edit shell profiles.
    su - "$GUEST_USER" -c \
        "PATH='/home/${GUEST_USER}/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' CODEX_NON_INTERACTIVE=1 sh '$CODEX_INSTALLER'" \
        </dev/null

    if [ ! -x "$CODEX_NATIVE_BIN" ] || [ ! -x "$CODEX_NATIVE_CODE_MODE_HOST" ]; then
        echo '  [guest] ERROR: Codex native package did not contain both required binaries.' >&2
        exit 1
    fi

    # Publish the host first and Codex last as the complete-pair commit point.
    # Both links follow the native installer's `current` release across updates.
    ln -s "$CODEX_NATIVE_CODE_MODE_HOST" "$CODEX_CODE_MODE_HOST_LINK_TMP"
    ln -s "$CODEX_NATIVE_BIN" "$CODEX_LINK_TMP"
    mv -Tf "$CODEX_CODE_MODE_HOST_LINK_TMP" /usr/local/bin/codex-code-mode-host
    mv -Tf "$CODEX_LINK_TMP" /usr/local/bin/codex

    # The native installer validates its package; verify both coop links as its user.
    if ! su - "$GUEST_USER" -c \
        '/usr/local/bin/codex --version >/dev/null && test -x /usr/local/bin/codex-code-mode-host' \
        </dev/null; then
        echo '  [guest] ERROR: Codex CLI or Code Mode host is not usable through /usr/local/bin' >&2
        exit 1
    fi
)
