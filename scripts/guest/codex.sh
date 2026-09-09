# Keep the installer cleanup local when provisioning snippets are concatenated.
(
    set -euo pipefail
    : "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"

    # Profiles may supply their own Codex; explicit updates replace it.
    if [ -z "${COOP_FORCE_INSTALL:-}" ] && [ -x /usr/local/bin/codex ]; then
        echo '  [guest] Codex CLI already installed, skipping.'
        exit 0
    fi

    echo '  [guest] Installing Codex CLI with the native installer...'
    CODEX_NATIVE_BIN="/home/${GUEST_USER}/.local/bin/codex"
    CODEX_INSTALLER=$(mktemp)
    CODEX_LINK_TMP="/usr/local/bin/codex.new.$$"
    trap 'rm -f "$CODEX_INSTALLER" "$CODEX_LINK_TMP"' EXIT
    chmod 0644 "$CODEX_INSTALLER"

    # curl retries transient failures without running a partially downloaded script.
    curl -fsSL --retry 3 --retry-all-errors \
        -o "$CODEX_INSTALLER" https://chatgpt.com/codex/install.sh

    # Retain the upstream package layout and guest ownership for `codex update`.
    # PATH is already managed by coop; the installer need not edit shell profiles.
    su - "$GUEST_USER" -c \
        "PATH='/home/${GUEST_USER}/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin' CODEX_NON_INTERACTIVE=1 sh '$CODEX_INSTALLER'" \
        </dev/null

    # Keep existing coop wrappers and absolute-path callers working after updates.
    ln -s "$CODEX_NATIVE_BIN" "$CODEX_LINK_TMP"
    mv -Tf "$CODEX_LINK_TMP" /usr/local/bin/codex

    # The native installer validates its package; verify coop's link as its user.
    if ! su - "$GUEST_USER" -c '/usr/local/bin/codex --version' </dev/null; then
        echo '  [guest] ERROR: Codex is not usable through /usr/local/bin/codex' >&2
        exit 1
    fi
)
