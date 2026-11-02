#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for `coop uninstall`.
#
# Runs the uninstall flow with HOME pointed at a throwaway tempdir, so the
# tests never touch the developer's real ~/.coop. Verifies:
#
#   1. --yes --keep-data removes the binary and leaves the data dir alone.
#   2. --yes --purge removes binary, data dir, update-check state, and
#      any coop SSH config blocks.
#   3. Non-interactive (non-TTY) without --yes exits non-zero with a hint.
#   4. The dev-build guard refuses to remove a binary that lives under
#      `target/release` or `target/debug`.
#
# Run manually:   ./tests/integration-uninstall.sh
# Run in CI:      ./tests/run-policy-integration.sh (shared build, no VM)
# Optional arg:   prebuilt ordinary coop binary (skips compilation)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

# Detach from the controlling terminal's stdin. coop gates interactive prompts
# (here, `uninstall`'s confirmation) on stdin being a TTY. The --yes calls skip
# it and Test 3 pins its own stdin to /dev/null, so nothing blocks today, but a
# future prompt-bearing case run from an interactive shell (the release
# preflight) would read real keystrokes and block — under CI stdin is already
# not a TTY, so it would never be caught there. Redirecting the whole script
# makes every coop subprocess see a non-TTY stdin regardless of how the suite is
# invoked. The script itself never reads stdin.
exec </dev/null

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

# ── Obtain a development-profile binary before isolating HOME ────────────────

if [[ $# -gt 1 ]]; then
    echo "Usage: $0 [COOP_BINARY]" >&2
    exit 2
fi
if [[ $# -eq 0 ]]; then
    "$SCRIPT_DIR/build-policy-binaries.sh" "$TMPDIR/artifacts" dev
    set -- "$TMPDIR/artifacts/coop-dev"
fi
STABLE_BIN="$TMPDIR/bin/coop-stable"
cp "$1" "$STABLE_BIN"

# Exercise the path-based guard using a disposable target/debug tree. Never
# uninstall Cargo's mutable output or a shared input, even if the guard breaks.
mkdir -p "$TMPDIR/target/debug"
TARGET_BIN="$TMPDIR/target/debug/coop"
cp "$STABLE_BIN" "$TARGET_BIN"
unset RUST_LOG

# ── Isolate $HOME / XDG dirs ─────────────────────────────────────────────────

export HOME="$TMPDIR/home"
export XDG_STATE_HOME="$HOME/.local/state"
export XDG_DATA_HOME="$HOME/.local/share"
mkdir -p "$XDG_STATE_HOME" "$XDG_DATA_HOME" "$HOME/.ssh"

# Where coop writes the background update-check state. Must mirror
# `state_path()` in src/update.rs, which uses `dirs::state_dir()` on Linux and
# falls back to `dirs::data_local_dir()` (~/Library/Application Support) on
# macOS, where `state_dir()` returns None.
case "$(uname -s)" in
    Darwin) STATE_FILE="$HOME/Library/Application Support/coop/update-check.json" ;;
    *)      STATE_FILE="$XDG_STATE_HOME/coop/update-check.json" ;;
esac

# Pre-populate the state file so we can assert it's wiped by --purge.
seed_state() {
    mkdir -p "$(dirname "$STATE_FILE")"
    cat > "$STATE_FILE" << 'JSON'
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
    if [[ -f "$STATE_FILE" ]]; then
        pass "update-check state preserved"
    else
        fail "update-check state was removed despite --keep-data"
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

echo "==> Test 2: --yes --purge removes binary + data + update-check state"
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
    if [[ -e "$STATE_FILE" ]]; then
        fail "update-check state survived --purge" "expected $STATE_FILE to be removed"
    else
        pass "update-check state removed"
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

# ── Test 4: dev-build guard refuses to remove target/debug/coop ────────────

echo "==> Test 4: dev-build guard refuses target/debug binary"
seed_data_dir

# The fixture has the same consecutive `target/debug` components as a Cargo
# development build, independently of the caller's Cargo target directory.
if "$TARGET_BIN" uninstall --yes --keep-data > "$TMPDIR/t4.log" 2>&1; then
    if [[ -e "$TARGET_BIN" ]]; then
        if grep -qi "cargo build artifact" "$TMPDIR/t4.log"; then
            pass "dev-build guard refused to remove $TARGET_BIN"
        else
            fail "binary preserved but guard message missing" \
                "$(tail -5 "$TMPDIR/t4.log")"
        fi
    else
        fail "guard failed — target/debug/coop was deleted"
    fi
else
    fail "uninstall returned non-zero on dev-build guard path" \
        "$(tail -5 "$TMPDIR/t4.log")"
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo
echo "  $pass_count passed, $fail_count failed"
[[ $fail_count -eq 0 ]]
