set -euo pipefail

# GUEST_USER is exported by the orchestrator (setup.rs / lima.rs).
: "${GUEST_USER:?GUEST_USER must be set by the orchestrator}"

# Skip if a profile already provided a grok binary (e.g. a test stub).
# Profile post_install scripts run before this script.
# Using if/else (not early `exit 0`) because this file is concatenated
# with the other guest installers; an unconditional exit would skip them.
if [ -x "/home/${GUEST_USER}/.grok/bin/grok" ]; then
    echo '  [guest] Grok Build CLI already installed, skipping.'
else
    echo '  [guest] Installing Grok Build CLI...'

    # Download the installer to a file first. The `curl | bash` pattern causes
    # the installer's prompts to inherit curl's pipe as stdin, hanging in
    # non-interactive contexts (cloud-init, chroot). Running from a file with
    # stdin from /dev/null avoids this.
    INSTALLER=$(mktemp)
    chmod 644 "$INSTALLER"

    # Retry with exponential backoff — transient network errors are common
    # during cloud-init (DNS not ready, CDN hiccups, etc.).
    MAX_RETRIES=4
    RETRY_DELAY=5
    for attempt in $(seq 1 "$MAX_RETRIES"); do
        if curl -fsSL -o "$INSTALLER" https://x.ai/cli/install.sh 2>/tmp/grok-curl-err; then
            break
        fi
        CURL_EXIT=$?
        CURL_ERR=$(cat /tmp/grok-curl-err 2>/dev/null || true)
        if [ "$attempt" -eq "$MAX_RETRIES" ]; then
            echo "  [guest] ERROR: Failed to download Grok Build installer" \
                 "after $MAX_RETRIES attempts." >&2
            echo "  [guest] curl exit code: $CURL_EXIT" >&2
            echo "  [guest] curl error: ${CURL_ERR:-none}" >&2
            rm -f "$INSTALLER"
            exit 1
        fi
        echo "  [guest] Download failed (attempt $attempt/$MAX_RETRIES," \
             "curl exit $CURL_EXIT), retrying in ${RETRY_DELAY}s..."
        sleep "$RETRY_DELAY"
        RETRY_DELAY=$((RETRY_DELAY * 2))
    done

    su - "${GUEST_USER}" -c "bash '$INSTALLER'" </dev/null
    rm -f "$INSTALLER"

    if [ ! -x "/home/${GUEST_USER}/.grok/bin/grok" ]; then
        echo "  [guest] ERROR: Grok Build installer finished but" \
             "/home/${GUEST_USER}/.grok/bin/grok is missing." >&2
        exit 1
    fi
fi
