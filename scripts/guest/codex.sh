set -euo pipefail

# Using if/else (not early `exit 0`) because this file is concatenated
# with claude-code.sh into a single shell invocation; an unconditional
# exit would short-circuit anything appended after it.
#
# COOP_FORCE_INSTALL bypasses the "already installed" guard so `coop agent
# update --codex` can re-run this same script. Unset during normal setup, so
# the skip-if-present behaviour is preserved there.
if [ -z "${COOP_FORCE_INSTALL:-}" ] \
    && [ -x /usr/local/bin/codex ] \
    && [ -x /usr/local/bin/codex-code-mode-host ]; then
    echo '  [guest] Codex CLI package already installed, skipping.'
else
    echo '  [guest] Installing Codex CLI package...'

    case "$(uname -m)" in
        x86_64)
            CODEX_TARGET="x86_64-unknown-linux-musl"
            ;;
        aarch64|arm64)
            CODEX_TARGET="aarch64-unknown-linux-musl"
            ;;
        *)
            echo "  [guest] ERROR: Unsupported architecture for Codex CLI: $(uname -m)" >&2
            exit 1
            ;;
    esac

    CODEX_ASSET="codex-package-$CODEX_TARGET.tar.gz"
    CODEX_TMP_DIR=$(mktemp -d)
    trap 'rm -rf "$CODEX_TMP_DIR"' EXIT
    CODEX_ARCHIVE="$CODEX_TMP_DIR/$CODEX_ASSET"
    CODEX_EXTRACTED="$CODEX_TMP_DIR/package"
    mkdir -p "$CODEX_EXTRACTED"

    MAX_RETRIES=4
    RETRY_DELAY=5
    CODEX_URL="https://github.com/openai/codex/releases/latest/download/$CODEX_ASSET"
    for attempt in $(seq 1 "$MAX_RETRIES"); do
        if curl -fsSL -o "$CODEX_ARCHIVE" "$CODEX_URL" 2>/tmp/codex-curl-err; then
            break
        fi
        CURL_EXIT=$?
        CURL_ERR=$(cat /tmp/codex-curl-err 2>/dev/null || true)
        if [ "$attempt" -eq "$MAX_RETRIES" ]; then
            echo "  [guest] ERROR: Failed to download Codex CLI package after $MAX_RETRIES attempts." >&2
            echo "  [guest] curl exit code: $CURL_EXIT" >&2
            echo "  [guest] curl error: ${CURL_ERR:-none}" >&2
            exit 1
        fi
        echo "  [guest] Download failed (attempt $attempt/$MAX_RETRIES, curl exit $CURL_EXIT), retrying in ${RETRY_DELAY}s..."
        sleep "$RETRY_DELAY"
        RETRY_DELAY=$((RETRY_DELAY * 2))
    done

    tar -xzf "$CODEX_ARCHIVE" -C "$CODEX_EXTRACTED"
    for executable in codex codex-code-mode-host; do
        if [ ! -x "$CODEX_EXTRACTED/bin/$executable" ]; then
            echo "  [guest] ERROR: Codex package did not contain executable bin/$executable." >&2
            exit 1
        fi
    done
    if [ ! -f "$CODEX_EXTRACTED/codex-package.json" ] \
        || [ ! -d "$CODEX_EXTRACTED/codex-resources" ] \
        || [ ! -d "$CODEX_EXTRACTED/codex-path" ]; then
        echo '  [guest] ERROR: Codex package did not contain its manifest and runtime resources.' >&2
        exit 1
    fi

    # Keep each upstream package intact so Codex can find its bundled runtime
    # resources relative to bin/codex. The archive digest is a stable release
    # directory name even though the `latest` download URL has no version in it.
    CODEX_PACKAGE_SHA=$(sha256sum "$CODEX_ARCHIVE" | cut -d ' ' -f 1)
    CODEX_INSTALL_ROOT="/usr/local/lib/codex"
    CODEX_RELEASES_DIR="$CODEX_INSTALL_ROOT/releases"
    CODEX_RELEASE_DIR="$CODEX_RELEASES_DIR/$CODEX_PACKAGE_SHA"
    install -d -m 755 "$CODEX_RELEASES_DIR"

    if [ ! -d "$CODEX_RELEASE_DIR" ]; then
        CODEX_STAGING_DIR="$CODEX_RELEASES_DIR/.staging-$CODEX_PACKAGE_SHA-$$"
        install -d -m 755 "$CODEX_STAGING_DIR"
        cp -a "$CODEX_EXTRACTED/." "$CODEX_STAGING_DIR/"
        mv -T "$CODEX_STAGING_DIR" "$CODEX_RELEASE_DIR"
    fi

    # Both public entrypoints resolve through one `current` link. Updating that
    # link switches codex and its version-matched code-mode host atomically.
    CODEX_NEXT_LINK="$CODEX_INSTALL_ROOT/.current-$$"
    ln -s "releases/$CODEX_PACKAGE_SHA" "$CODEX_NEXT_LINK"
    mv -Tf "$CODEX_NEXT_LINK" "$CODEX_INSTALL_ROOT/current"

    for executable in codex codex-code-mode-host; do
        CODEX_BIN_LINK="/usr/local/bin/.$executable-$$"
        ln -s "$CODEX_INSTALL_ROOT/current/bin/$executable" "$CODEX_BIN_LINK"
        mv -Tf "$CODEX_BIN_LINK" "/usr/local/bin/$executable"
    done
fi
