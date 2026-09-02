set -euo pipefail

# Using if/else (not early `exit 0`) because this file is concatenated with
# other guest installers; an unconditional exit would skip everything after it.

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
CODEX_SUMS_ASSET="codex-package_SHA256SUMS"
CODEX_INSTALL_ROOT="/usr/local/lib/codex"
CODEX_RELEASES_DIR="$CODEX_INSTALL_ROOT/releases"
CODEX_BIN_LINK=""

codex_package_is_complete() {
    local package_dir="$1"
    local manifest_version reported_version

    [ -x "$package_dir/bin/codex" ] \
        && [ -x "$package_dir/bin/codex-code-mode-host" ] \
        && [ -x "$package_dir/codex-path/rg" ] \
        && [ -x "$package_dir/codex-resources/bwrap" ] \
        && [ -x "$package_dir/codex-resources/zsh/bin/zsh" ] \
        && [ -f "$package_dir/codex-package.json" ] \
        && jq -e --arg target "$CODEX_TARGET" \
            '.layoutVersion == 1 and .target == $target and .entrypoint == "bin/codex"' \
            "$package_dir/codex-package.json" >/dev/null \
        || return 1

    manifest_version=$(jq -er '.version | strings | select(length > 0)' \
        "$package_dir/codex-package.json") || return 1
    reported_version=$("$package_dir/bin/codex" --version) || return 1
    [ "$reported_version" = "codex-cli $manifest_version" ] || return 1
}

codex_installed_package_is_safe() {
    local package_dir="$1"
    local resolved_dir

    resolved_dir=$(readlink -f "$package_dir") || return 1

    codex_package_is_complete "$package_dir" \
        && [ -z "$(find "$resolved_dir" ! -type l ! -user root -print -quit)" ] \
        && [ -z "$(find "$resolved_dir" ! -type l -perm /022 -print -quit)" ]
}

codex_reconcile_public_entrypoints() {
    local executable

    # Install the helper first and Codex last. On migration from coop's legacy
    # single-binary layout, Codex becomes the commit point only after its helper
    # is ready. Both links resolve through `current`, so later release switches
    # update the pair together.
    for executable in codex-code-mode-host codex; do
        CODEX_BIN_LINK="/usr/local/bin/.$executable-$$"
        rm -f -- "$CODEX_BIN_LINK"
        ln -s "$CODEX_INSTALL_ROOT/current/bin/$executable" "$CODEX_BIN_LINK"
        mv -Tf "$CODEX_BIN_LINK" "/usr/local/bin/$executable"
        CODEX_BIN_LINK=""
    done
}

# COOP_FORCE_INSTALL bypasses the installed-package guard so `coop agent
# update --codex` always checks upstream. Normal setup skips only a complete,
# root-owned package; two manually copied binaries are not enough.
if [ -z "${COOP_FORCE_INSTALL:-}" ] \
    && codex_installed_package_is_safe "$CODEX_INSTALL_ROOT/current"; then
    codex_reconcile_public_entrypoints
    /usr/local/bin/codex --version >/dev/null
    echo '  [guest] Codex CLI package already installed, skipping.'
else
    echo '  [guest] Installing Codex CLI package...'

    CODEX_TMP_DIR=$(mktemp -d)
    CODEX_STAGING_DIR=""
    CODEX_NEXT_LINK=""

    cleanup_codex_install() {
        rm -rf "$CODEX_TMP_DIR"
        [ -z "$CODEX_STAGING_DIR" ] || rm -rf -- "$CODEX_STAGING_DIR"
        [ -z "$CODEX_NEXT_LINK" ] || rm -f -- "$CODEX_NEXT_LINK"
        [ -z "$CODEX_BIN_LINK" ] || rm -f -- "$CODEX_BIN_LINK"
    }
    trap cleanup_codex_install EXIT

    download_codex_file() {
        local url="$1"
        local output="$2"
        local description="$3"
        local attempt curl_exit curl_error
        local retry_delay=5

        for attempt in $(seq 1 4); do
            if curl -fsSL -o "$output" "$url" 2>"$CODEX_TMP_DIR/curl.err"; then
                return 0
            else
                curl_exit=$?
            fi
            curl_error=$(cat "$CODEX_TMP_DIR/curl.err" 2>/dev/null || true)
            if [ "$attempt" -eq 4 ]; then
                echo "  [guest] ERROR: Failed to download $description after 4 attempts." >&2
                echo "  [guest] curl exit code: $curl_exit" >&2
                echo "  [guest] curl error: ${curl_error:-none}" >&2
                return 1
            fi
            echo "  [guest] Download failed (attempt $attempt/4, curl exit $curl_exit), retrying in ${retry_delay}s..."
            sleep "$retry_delay"
            retry_delay=$((retry_delay * 2))
        done
    }

    CODEX_SUMS_FILE="$CODEX_TMP_DIR/$CODEX_SUMS_ASSET"
    CODEX_ARCHIVE="$CODEX_TMP_DIR/$CODEX_ASSET"
    CODEX_PACKAGE_SHA=""
    CODEX_RELEASE_VERIFIED=0
    for release_attempt in $(seq 1 4); do
        download_codex_file \
            "https://github.com/openai/codex/releases/latest/download/$CODEX_SUMS_ASSET" \
            "$CODEX_SUMS_FILE" "Codex package checksum manifest"
        CODEX_PACKAGE_SHA=$(awk -v asset="$CODEX_ASSET" \
            '$2 == asset && length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ { print tolower($1) }' \
            "$CODEX_SUMS_FILE")

        download_codex_file \
            "https://github.com/openai/codex/releases/latest/download/$CODEX_ASSET" \
            "$CODEX_ARCHIVE" "Codex CLI package"
        if [[ "$CODEX_PACKAGE_SHA" =~ ^[0-9a-f]{64}$ ]] \
            && printf '%s  %s\n' "$CODEX_PACKAGE_SHA" "$CODEX_ARCHIVE" \
                | sha256sum -c - >/dev/null 2>&1; then
            CODEX_RELEASE_VERIFIED=1
            break
        fi

        if [ "$release_attempt" -lt 4 ]; then
            echo "  [guest] Codex release changed during download; retrying the package and checksums..."
            sleep 2
        fi
    done
    if [ "$CODEX_RELEASE_VERIFIED" -ne 1 ]; then
        echo "  [guest] ERROR: Codex package did not match its published checksum after 4 attempts." >&2
        exit 1
    fi

    install -d -m 755 "$CODEX_RELEASES_DIR"
    exec 9>"$CODEX_INSTALL_ROOT/install.lock"
    flock 9
    find "$CODEX_RELEASES_DIR" -mindepth 1 -maxdepth 1 -type d \
        -name '.staging-*' -exec rm -rf -- {} +

    CODEX_RELEASE_DIR="$CODEX_RELEASES_DIR/$CODEX_PACKAGE_SHA"
    if ! codex_installed_package_is_safe "$CODEX_RELEASE_DIR"; then
        CODEX_STAGING_DIR="$CODEX_RELEASES_DIR/.staging-$CODEX_PACKAGE_SHA-$$"
        install -d -m 755 "$CODEX_STAGING_DIR"
        tar --no-same-owner --no-same-permissions -xzf "$CODEX_ARCHIVE" \
            -C "$CODEX_STAGING_DIR"
        chown -R root:root "$CODEX_STAGING_DIR"
        chmod -R go-w "$CODEX_STAGING_DIR"
        codex_installed_package_is_safe "$CODEX_STAGING_DIR" || {
            echo '  [guest] ERROR: Staged Codex package failed validation.' >&2
            exit 1
        }
        if [ -e "$CODEX_RELEASE_DIR" ] || [ -L "$CODEX_RELEASE_DIR" ]; then
            rm -rf -- "$CODEX_RELEASE_DIR"
        fi
        mv -T "$CODEX_STAGING_DIR" "$CODEX_RELEASE_DIR"
        CODEX_STAGING_DIR=""
    fi

    # Once installed, both entrypoints resolve through this one link. Updating
    # it switches Codex and its version-matched code-mode host atomically.
    CODEX_NEXT_LINK="$CODEX_INSTALL_ROOT/.current-$$"
    ln -s "releases/$CODEX_PACKAGE_SHA" "$CODEX_NEXT_LINK"
    mv -Tf "$CODEX_NEXT_LINK" "$CODEX_INSTALL_ROOT/current"
    CODEX_NEXT_LINK=""

    codex_reconcile_public_entrypoints
    /usr/local/bin/codex --version >/dev/null
fi
