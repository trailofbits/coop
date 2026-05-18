#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for `coop uninstall`.
#
# Runs the uninstall flow with HOME pointed at a throwaway tempdir, so the
# tests never touch the developer's real ~/.coop. Verifies:
#
#   1. --yes --keep-data removes the binary and leaves the data dir alone.
#   2. --yes --purge removes binary, data dir, XDG update-check state, and
#      any coop SSH config blocks.
#   3. Non-interactive (non-TTY) without --yes exits non-zero with a hint.
#   4. The dev-build guard refuses to remove a binary that lives under
#      `target/release` or `target/debug`.
#
# Run manually:   ./tests/integration-uninstall.sh
# Run in CI:      same (fast — no VM, no network)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Temp workspace ───────────────────────────────────────────────────────────

TMPDIR="$(mktemp -d)"
cleanup() { rm -rf "$TMPDIR"; }
trap cleanup EXIT

mkdir -p "$TMPDIR/bin" "$TMPDIR/home"

pass_count=0
fail_count=0

pass() {
    pass_count=$((pass_count + 1))
    echo "  PASS  $1"
}

fail() {
    fail_count=$((fail_count + 1))
    echo "  FAIL  $1"
    # Detail line ($2) is optional. Return 0 explicitly so that calling `fail`
    # with one argument under `set -e` doesn't abort the whole script — the
    # short-circuit `[[ -n "" ]] && echo` would otherwise propagate rc=1 out of
    # the function.
    if [[ -n "${2:-}" ]]; then
        echo "        $2"
    fi
    return 0
}

# ── Build a release binary outside the project's target/ tree ────────────────
#
# The uninstall command refuses to delete binaries whose path contains
# `target/{debug,release}` (the dev-build guard). For the success-path tests we
# need a binary that *isn't* under that pattern, so we stash a copy in
# $TMPDIR/bin. Test 4 deliberately runs the in-target binary to exercise the
# guard.

echo "==> Building release binary..."
(cd "$PROJECT_DIR" && cargo build --release --quiet)
TARGET_BIN="$PROJECT_DIR/target/release/coop"
STABLE_BIN="$TMPDIR/bin/coop-stable"
cp "$TARGET_BIN" "$STABLE_BIN"

# ── Isolate $HOME / XDG dirs ─────────────────────────────────────────────────

export HOME="$TMPDIR/home"
export XDG_STATE_HOME="$HOME/.local/state"
export XDG_DATA_HOME="$HOME/.local/share"
mkdir -p "$XDG_STATE_HOME" "$XDG_DATA_HOME" "$HOME/.ssh"

# Pre-populate an XDG state file so we can assert it's wiped by --purge.
seed_state() {
    local state_dir="$XDG_STATE_HOME/coop"
    mkdir -p "$state_dir"
    cat > "$state_dir/update-check.json" << 'JSON'
{"last_checked_at": 0, "latest_known_version": "v9.9.9"}
JSON
}

# Pre-populate ~/.coop with stub files so we can assert it's preserved or wiped.
seed_data_dir() {
    local data_dir="$HOME/.coop"
    mkdir -p "$data_dir/images" "$data_dir/instances"
    # Touch a config so config_path_is_under_data_dir has something to look at.
    : > "$data_dir/config.toml"
}

# Pre-populate ~/.ssh/config with a coop marker block; uninstall should strip
# it. Markers must match the literal MARKER_PREFIX / MARKER_END strings used by
# src/workspace.rs (`# coop START <host>` and `# coop END`).
SSH_MARKER_BEGIN="# coop START coop-uninstall-test"
SSH_MARKER_END="# coop END"
seed_ssh_config() {
    cat > "$HOME/.ssh/config" << EOF
$SSH_MARKER_BEGIN
Host coop-uninstall-test
    HostName 172.16.0.42
$SSH_MARKER_END

# unrelated user block
Host github.com
    User git
EOF
}

# Fresh copy of the binary at $TMPDIR/bin/coop for each test that removes it.
fresh_binary() {
    cp "$STABLE_BIN" "$TMPDIR/bin/coop"
}

# ── Test 1: --yes --keep-data preserves the data directory ───────────────────

echo "==> Test 1: --yes --keep-data removes binary, keeps data"
fresh_binary
seed_data_dir
seed_state
seed_ssh_config

if "$TMPDIR/bin/coop" uninstall --yes --keep-data > "$TMPDIR/t1.log" 2>&1; then
    if [[ -e "$TMPDIR/bin/coop" ]]; then
        fail "binary still present after uninstall"
    else
        pass "binary removed"
    fi
    if [[ -d "$HOME/.coop" ]]; then
        pass "data directory preserved"
    else
        fail "data directory was removed despite --keep-data"
    fi
    if [[ -f "$XDG_STATE_HOME/coop/update-check.json" ]]; then
        pass "XDG update-check state preserved"
    else
        fail "XDG state was removed despite --keep-data"
    fi
    if grep -q "$SSH_MARKER_BEGIN" "$HOME/.ssh/config"; then
        fail "SSH coop block not stripped" "blocks should be removed even with --keep-data"
    else
        pass "SSH coop blocks stripped"
    fi
    if grep -q "github.com" "$HOME/.ssh/config"; then
        pass "unrelated SSH blocks preserved"
    else
        fail "unrelated SSH content was clobbered"
    fi
else
    fail "uninstall --yes --keep-data exited non-zero" "$(tail -5 "$TMPDIR/t1.log")"
fi

# ── Test 2: --yes --purge wipes everything ───────────────────────────────────

echo "==> Test 2: --yes --purge removes binary + data + XDG state"
fresh_binary
seed_data_dir
seed_state
seed_ssh_config

if "$TMPDIR/bin/coop" uninstall --yes --purge > "$TMPDIR/t2.log" 2>&1; then
    if [[ -e "$TMPDIR/bin/coop" ]]; then
        fail "binary still present after --purge"
    else
        pass "binary removed"
    fi
    if [[ -d "$HOME/.coop" ]]; then
        fail "data directory still present after --purge"
    else
        pass "data directory removed"
    fi
    if [[ -e "$XDG_STATE_HOME/coop/update-check.json" ]]; then
        fail "XDG update-check state survived --purge"
    else
        pass "XDG update-check state removed"
    fi
    if grep -q "$SSH_MARKER_BEGIN" "$HOME/.ssh/config" 2> /dev/null; then
        fail "SSH coop block survived --purge"
    else
        pass "SSH coop blocks stripped"
    fi
else
    fail "uninstall --yes --purge exited non-zero" "$(tail -5 "$TMPDIR/t2.log")"
fi

# ── Test 3: non-TTY without --yes fails with a helpful message ───────────────

echo "==> Test 3: non-TTY without --yes errors"
fresh_binary
seed_data_dir

# stdin is already not a TTY when running under bash via the script harness;
# redirect from /dev/null to be explicit.
if "$TMPDIR/bin/coop" uninstall < /dev/null > "$TMPDIR/t3.log" 2>&1; then
    fail "uninstall without --yes succeeded in non-interactive mode"
elif grep -qi "not a tty" "$TMPDIR/t3.log" && grep -q -- "--yes" "$TMPDIR/t3.log"; then
    pass "non-TTY without --yes errors with --yes hint"
else
    fail "exit was non-zero but message lacks the --yes hint" \
        "$(tail -5 "$TMPDIR/t3.log")"
fi

if [[ -e "$TMPDIR/bin/coop" ]]; then
    pass "binary preserved after refusal"
else
    fail "binary removed despite refusal"
fi

# ── Test 4: dev-build guard refuses to remove target/release/coop ────────────

echo "==> Test 4: dev-build guard refuses target/release binary"
seed_data_dir

# Run the in-target binary directly. It's under PROJECT_DIR/target/release/,
# which matches the consecutive `target/release` guard.
if "$TARGET_BIN" uninstall --yes --keep-data > "$TMPDIR/t4.log" 2>&1; then
    if [[ -e "$TARGET_BIN" ]]; then
        if grep -qi "cargo build artifact" "$TMPDIR/t4.log"; then
            pass "dev-build guard refused to remove $TARGET_BIN"
        else
            fail "binary preserved but guard message missing" \
                "$(tail -5 "$TMPDIR/t4.log")"
        fi
    else
        fail "guard failed — target/release/coop was deleted"
    fi
else
    fail "uninstall returned non-zero on dev-build guard path" \
        "$(tail -5 "$TMPDIR/t4.log")"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo
echo "  $pass_count passed, $fail_count failed"
[[ $fail_count -eq 0 ]]
