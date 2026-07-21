#!/usr/bin/env bash
set -euo pipefail

# Runner for coop integration tests.
#
# Usage:
#   Local:   ./tests/run-integration.sh [test flags...]
#   Remote:  ./tests/run-integration.sh --remote user@host [test flags...]
#
# Local mode builds the binary and runs tests/integration.sh directly.
# Remote mode detects the remote host's architecture, cross-compiles
# the matching musl binary, copies it and the test script, and runs tests there.
#
# All flags other than --remote are forwarded to integration.sh.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
TEST_SCRIPT="$SCRIPT_DIR/integration.sh"

REMOTE_HOST=""
FORWARD_ARGS=()

while [[ $# -gt 0 ]]; do
    case "$1" in
        --remote)  REMOTE_HOST="$2";  shift 2 ;;
        *)         FORWARD_ARGS+=("$1"); shift ;;
    esac
done

# ── Local mode ───────────────────────────────────────────────────

if [[ -z "$REMOTE_HOST" ]]; then
    echo "Building coop (release)..."
    cargo build --release --manifest-path "$PROJECT_DIR/Cargo.toml"

    # coop-proxy (issue #411) is a separate workspace member — it needs cmake
    # (aws-lc-rs), so it is intentionally not a default member. Build it
    # best-effort next to coop so the credential-proxy phase can run; if it
    # fails (e.g. cmake missing) that phase skips rather than blocking the suite.
    if ! cargo build --release -p coop-proxy --manifest-path "$PROJECT_DIR/Cargo.toml"; then
        echo "warning: coop-proxy build failed (cmake missing?) — proxy phase will skip" >&2
    fi

    BINARY="$PROJECT_DIR/target/release/coop"
    exec "$TEST_SCRIPT" --binary "$BINARY" "${FORWARD_ARGS[@]+"${FORWARD_ARGS[@]}"}"
fi

# ── Remote mode ──────────────────────────────────────────────────

REMOTE_OS=$(ssh "$REMOTE_HOST" uname -s)
REMOTE_ARCH=$(ssh "$REMOTE_HOST" uname -m)

case "$REMOTE_OS-$REMOTE_ARCH" in
    Linux-x86_64)   TARGET="x86_64-unknown-linux-musl" ;;
    Linux-aarch64)  TARGET="aarch64-unknown-linux-musl" ;;
    Darwin-arm64)   TARGET="aarch64-apple-darwin" ;;
    Darwin-x86_64)  TARGET="x86_64-apple-darwin" ;;
    *)              echo "Unsupported remote platform: $REMOTE_OS $REMOTE_ARCH" >&2; exit 1 ;;
esac

echo "Cross-compiling for $TARGET..."
cargo build --release --target "$TARGET" \
    --manifest-path "$PROJECT_DIR/Cargo.toml"

# Best-effort coop-proxy (issue #411): a separate workspace member needing
# cmake + a C toolchain for the target. Ship it alongside coop so `coop` can
# spawn it (it looks next to itself); the proxy phase skips if it isn't present.
if ! cargo build --release --target "$TARGET" -p coop-proxy \
    --manifest-path "$PROJECT_DIR/Cargo.toml"; then
    echo "warning: coop-proxy build failed for $TARGET (cmake/toolchain?) — proxy phase will skip" >&2
fi

LOCAL_BINARY="$PROJECT_DIR/target/$TARGET/release/coop"
LOCAL_PROXY="$PROJECT_DIR/target/$TARGET/release/coop-proxy"

REMOTE_DIR=$(ssh "$REMOTE_HOST" mktemp -d)
trap 'ssh "$REMOTE_HOST" rm -rf "$REMOTE_DIR"' EXIT

echo "Copying binary and test script to $REMOTE_HOST:$REMOTE_DIR..."
scp -q "$LOCAL_BINARY" "$TEST_SCRIPT" "$REMOTE_HOST:$REMOTE_DIR/"
if [[ -f "$LOCAL_PROXY" ]]; then
    scp -q "$LOCAL_PROXY" "$REMOTE_HOST:$REMOTE_DIR/"
fi

# Build the remote command as an array, then printf %q to safely quote for ssh.
# ssh doesn't forward arbitrary env vars, so pass opt-in flags explicitly.
REMOTE_CMD=()
if [[ -n "${COOP_TEST_DESTRUCTIVE:-}" ]]; then
    REMOTE_CMD+=("COOP_TEST_DESTRUCTIVE=$COOP_TEST_DESTRUCTIVE")
fi
REMOTE_CMD+=("$REMOTE_DIR/integration.sh" --binary "$REMOTE_DIR/coop"
    "${FORWARD_ARGS[@]+"${FORWARD_ARGS[@]}"}")

echo "Running tests on $REMOTE_HOST..."
echo ""
# shellcheck disable=SC2029 # client-side expansion is intentional (printf %q handles quoting)
ssh "$REMOTE_HOST" "$(printf '%q ' "${REMOTE_CMD[@]}")"
