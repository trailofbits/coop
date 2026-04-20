set -euo pipefail

if [ -x /usr/local/bin/codex ]; then
    echo '  [guest] Codex CLI already installed, skipping.'
    return 0 2>/dev/null || exit 0
fi

echo '  [guest] Installing Codex CLI...'

case "$(uname -m)" in
    x86_64)
        ASSET="codex-x86_64-unknown-linux-musl.tar.gz"
        ;;
    aarch64|arm64)
        ASSET="codex-aarch64-unknown-linux-musl.tar.gz"
        ;;
    *)
        echo "  [guest] ERROR: Unsupported architecture for Codex CLI: $(uname -m)" >&2
        exit 1
        ;;
esac

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT
ARCHIVE="$TMPDIR/$ASSET"

MAX_RETRIES=4
RETRY_DELAY=5
URL="https://github.com/openai/codex/releases/latest/download/$ASSET"
for attempt in $(seq 1 "$MAX_RETRIES"); do
    if curl -fsSL -o "$ARCHIVE" "$URL" 2>/tmp/codex-curl-err; then
        break
    fi
    CURL_EXIT=$?
    CURL_ERR=$(cat /tmp/codex-curl-err 2>/dev/null || true)
    if [ "$attempt" -eq "$MAX_RETRIES" ]; then
        echo "  [guest] ERROR: Failed to download Codex CLI archive after $MAX_RETRIES attempts." >&2
        echo "  [guest] curl exit code: $CURL_EXIT" >&2
        echo "  [guest] curl error: ${CURL_ERR:-none}" >&2
        exit 1
    fi
    echo "  [guest] Download failed (attempt $attempt/$MAX_RETRIES, curl exit $CURL_EXIT), retrying in ${RETRY_DELAY}s..."
    sleep "$RETRY_DELAY"
    RETRY_DELAY=$((RETRY_DELAY * 2))
done

tar -xzf "$ARCHIVE" -C "$TMPDIR"
BIN=$(find "$TMPDIR" -maxdepth 1 -type f -name 'codex-*' | head -n 1)
if [ -z "$BIN" ]; then
    echo '  [guest] ERROR: Codex archive did not contain the expected binary.' >&2
    exit 1
fi

install -m 755 "$BIN" /usr/local/bin/codex
