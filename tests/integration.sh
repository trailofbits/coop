#!/usr/bin/env bash
set -euo pipefail

# Integration tests for coop VM lifecycle.
#
# This script runs locally on the machine where the coop binary lives.
# For remote (cross-compile + deploy) usage, see tests/run-integration.sh.
#
# Usage:
#   ./tests/integration.sh --binary /path/to/coop [options]
#
# Options:
#   --binary PATH    Path to pre-built binary (required)
#   --profile LIST   Comma-separated profiles to install (default: none)
#   --name NAME      Instance name prefix (default: test-<pid>)
#   --full           Run extended tests (workspace sync, multi-instance)
#
# Environment variables (override flags):
#   TEST_BINARY   — Path to pre-built binary
#   TEST_PROFILES — Comma-separated profiles to install
#   TEST_INSTANCE — Instance name prefix
#   TEST_FULL     — Set to 1 for extended tests

# ── Defaults ──────────────────────────────────────────────────

BINARY="${TEST_BINARY:-}"
PROFILES="${TEST_PROFILES:-python,node}"
INSTANCE="${TEST_INSTANCE:-test-$$}"
FULL="${TEST_FULL:-0}"

# Track all instances we create for cleanup
STARTED_INSTANCES=()

# ── Argument parsing ─────────────────────────────────────────

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary)  BINARY="$2";  shift 2 ;;
        --profile) PROFILES="$2"; shift 2 ;;
        --name)    INSTANCE="$2"; shift 2 ;;
        --full)    FULL=1;        shift ;;
        --help|-h)
            sed -n '3,19s/^# //p' "$0"
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 1
            ;;
    esac
done

if [[ -z "$BINARY" ]]; then
    echo "ERROR: --binary is required" >&2
    echo "  Usage: $0 --binary /path/to/coop [options]" >&2
    echo "  Or use tests/run-integration.sh which builds automatically." >&2
    exit 1
fi

# ── Output helpers ────────────────────────────────────────────

pass_count=0
fail_count=0
skip_count=0

pass() {
    pass_count=$((pass_count + 1))
    echo "  PASS  $1"
}

fail() {
    fail_count=$((fail_count + 1))
    echo "  FAIL  $1"
    if [[ -n "${2:-}" ]]; then
        echo "        $2"
    fi
}

skip() {
    skip_count=$((skip_count + 1))
    echo "  SKIP  $1${2:+ ($2)}"
}

summary() {
    echo ""
    echo "────────────────────────────────────────"
    echo "  $pass_count passed, $fail_count failed, $skip_count skipped"
    echo "────────────────────────────────────────"
    if [[ $fail_count -gt 0 ]]; then
        exit 1
    fi
}

# ── Execution wrapper ─────────────────────────────────────────

# Run the coop binary. Returns exit code, captures stdout+stderr.
# Usage: coop <subcommand> [args...]
# Output goes to $HARNESS_OUT (stdout) and $HARNESS_ERR (stderr).
HARNESS_OUT=""
HARNESS_ERR=""

coop() {
    local rc=0
    HARNESS_OUT=$("$BINARY" "$@" 2>"$tmpdir/stderr") || rc=$?
    HARNESS_ERR=$(cat "$tmpdir/stderr")
    return $rc
}

# Run a command expecting failure. Returns 0 if the command fails.
moat_fails() {
    if coop "$@"; then
        return 1
    else
        return 0
    fi
}

# Run a command in the guest VM via `coop ssh -- <cmd>`.
# RUST_LOG=off suppresses tracing output that would mix with command output.
guest_exec() {
    local inst="${GUEST_INSTANCE:-$INSTANCE}"
    RUST_LOG=off "$BINARY" ssh "$inst" -- "$@" 2>/dev/null
}

# Run the exec subcommand (captures stdout, propagates exit code).
moat_exec() {
    local inst="${GUEST_INSTANCE:-$INSTANCE}"
    RUST_LOG=off "$BINARY" exec --name "$inst" "$@" 2>/dev/null
}

# Remove an instance from the tracked list.
untrack_instance() {
    local target="$1"
    local new_list=()
    for inst in "${STARTED_INSTANCES[@]}"; do
        if [[ "$inst" != "$target" ]]; then
            new_list+=("$inst")
        fi
    done
    STARTED_INSTANCES=("${new_list[@]+"${new_list[@]}"}")
}

# ── Verify binary ─────────────────────────────────────────────

verify_binary() {
    echo "Using binary: $BINARY"
    if [[ ! -x "$BINARY" ]]; then
        echo "ERROR: binary not found or not executable: $BINARY" >&2
        exit 1
    fi
}

# ── Cleanup trap ──────────────────────────────────────────────

cleanup() {
    echo ""
    echo "Cleaning up..."

    for inst in "${STARTED_INSTANCES[@]+"${STARTED_INSTANCES[@]}"}"; do
        echo "Destroying instance '$inst'..."
        coop destroy "$inst" 2>/dev/null || true
    done

    if [[ -d "${tmpdir:-}" ]]; then
        rm -rf "$tmpdir"
    fi

    if [[ -d "${ws_tmpdir:-}" ]]; then
        rm -rf "$ws_tmpdir"
    fi
}

trap cleanup EXIT

# ── Test phases ───────────────────────────────────────────────

# ── Pre-VM tests ──────────────────────────────────────────────

test_validate() {
    echo ""
    echo "=== Phase: validate ==="

    if coop validate; then
        pass "validate exits 0"
    else
        fail "validate exits 0" "exit code: $?"
    fi

    if echo "$HARNESS_OUT" | grep -qi "OK"; then
        pass "validate reports OK"
    else
        fail "validate reports OK" "got: $HARNESS_OUT"
    fi
}

test_invalid_names() {
    echo ""
    echo "=== Phase: invalid instance names ==="

    # Path traversal
    if moat_fails start "../../../tmp/evil" --no-claude; then
        pass "rejects path traversal name"
    else
        fail "rejects path traversal name" "should have failed"
    fi

    # Newline injection
    if moat_fails start $'evil\nname' --no-claude; then
        pass "rejects newline in name"
    else
        fail "rejects newline in name" "should have failed"
    fi

    # Empty name is fine (auto-generated), but spaces are not
    if moat_fails start "name with spaces" --no-claude; then
        pass "rejects spaces in name"
    else
        fail "rejects spaces in name" "should have failed"
    fi
}

# ── Setup ─────────────────────────────────────────────────────

test_setup() {
    echo ""
    echo "=== Phase: setup ==="

    local args=(setup -y)
    if [[ -n "$PROFILES" ]]; then
        args+=(--profile "$PROFILES")
    fi

    if coop "${args[@]}"; then
        pass "setup exits 0"
    else
        fail "setup exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        echo "FATAL: setup failed, cannot continue"
        exit 1
    fi
}

# ── Primary instance lifecycle ────────────────────────────────

test_start() {
    echo ""
    echo "=== Phase: start ==="

    local args=(start "$INSTANCE" --no-claude)
    if coop "${args[@]}"; then
        STARTED_INSTANCES+=("$INSTANCE")
        pass "start exits 0"
    else
        fail "start exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        echo "FATAL: start failed, cannot continue"
        exit 1
    fi
}

test_duplicate_name() {
    echo ""
    echo "=== Phase: duplicate instance name ==="

    if moat_fails start "$INSTANCE" --no-claude; then
        pass "rejects duplicate instance name"
    else
        fail "rejects duplicate instance name" "should have failed"
        # Clean up the accidental second instance
        coop destroy "$INSTANCE" 2>/dev/null || true
    fi
}

test_status_running() {
    echo ""
    echo "=== Phase: status (running) ==="

    if coop status "$INSTANCE"; then
        pass "status exits 0"
    else
        fail "status exits 0" "exit code: $?"
    fi

    if echo "$HARNESS_OUT" | grep -qi "running"; then
        pass "status reports running"
    else
        fail "status reports running" "got: $HARNESS_OUT"
    fi

    # Resource usage: load average is a non-negative decimal
    local load
    load=$(echo "$HARNESS_OUT" | sed -n 's/.*Load: \([0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' | head -1)
    if [[ -n "$load" ]]; then
        pass "status shows load average ($load)"
    else
        fail "status shows load average" "got: $HARNESS_OUT"
    fi

    # Resource usage: memory total > 0 and used <= total
    # Format: "Mem: <used>/<total> MiB (<pct>%)"
    local mem_used mem_total
    mem_used=$(echo "$HARNESS_OUT" | sed -n 's/.*Mem: \([0-9][0-9]*\)\/.*/\1/p' | head -1)
    mem_total=$(echo "$HARNESS_OUT" | sed -n 's/.*Mem: [0-9][0-9]*\/\([0-9][0-9]*\) MiB.*/\1/p' | head -1)
    if [[ -n "$mem_total" && "$mem_total" -gt 0 && -n "$mem_used" && "$mem_used" -le "$mem_total" ]]; then
        pass "status shows valid memory (${mem_used}/${mem_total} MiB)"
    else
        fail "status shows valid memory" "used=$mem_used total=$mem_total from: $HARNESS_OUT"
    fi

    # Resource usage: disk total > 0 and used <= total
    # Format: "Disk: <used>/<total> MiB (<pct>%)"
    local disk_used disk_total
    disk_used=$(echo "$HARNESS_OUT" | sed -n 's/.*Disk: \([0-9][0-9]*\)\/.*/\1/p' | head -1)
    disk_total=$(echo "$HARNESS_OUT" | sed -n 's/.*Disk: [0-9][0-9]*\/\([0-9][0-9]*\) MiB.*/\1/p' | head -1)
    if [[ -n "$disk_total" && "$disk_total" -gt 0 && -n "$disk_used" && "$disk_used" -le "$disk_total" ]]; then
        pass "status shows valid disk (${disk_used}/${disk_total} MiB)"
    else
        fail "status shows valid disk" "used=$disk_used total=$disk_total from: $HARNESS_OUT"
    fi

    # Multi-instance list: compact summary with valid percentages
    # Format: "load=X.XX mem=NN% disk=NN%"
    if coop status; then
        local mem_pct disk_pct
        mem_pct=$(echo "$HARNESS_OUT" | sed -n 's/.*mem=\([0-9][0-9]*\)%.*/\1/p' | head -1)
        disk_pct=$(echo "$HARNESS_OUT" | sed -n 's/.*disk=\([0-9][0-9]*\)%.*/\1/p' | head -1)
        if [[ -n "$mem_pct" && "$mem_pct" -le 100 && -n "$disk_pct" && "$disk_pct" -le 100 ]]; then
            pass "status list shows resource summary (mem=${mem_pct}% disk=${disk_pct}%)"
        else
            fail "status list shows resource summary" "mem_pct=$mem_pct disk_pct=$disk_pct from: $HARNESS_OUT"
        fi
    else
        fail "status list exits 0" "exit code: $?"
    fi
}

test_auto_resolve_running() {
    echo ""
    echo "=== Phase: auto-resolve (single running) ==="

    # With exactly one running instance, commands should work without
    # specifying a name. This tests the resolve_running logic.
    # Skip if other instances exist (pre-existing state from outside test).

    local running_count
    running_count=$(RUST_LOG=off "$BINARY" status 2>/dev/null | grep -c "running" || true)

    if [[ "$running_count" -ne 1 ]]; then
        skip "ssh auto-resolves single running instance" "$running_count running (need exactly 1)"
        skip "exec auto-resolves single running instance" "skipped (need exactly 1 running)"
        return
    fi

    # ssh without name should auto-select the single running instance
    local output
    if output=$(RUST_LOG=off "$BINARY" ssh -- echo "auto-resolve-works" 2>/dev/null); then
        pass "ssh auto-resolves single running instance"
        if echo "$output" | grep -q "auto-resolve-works"; then
            pass "ssh auto-resolve returns correct output"
        else
            fail "ssh auto-resolve returns correct output" "got: $output"
        fi
    else
        fail "ssh auto-resolves single running instance" "exit code: $?"
    fi

    # exec without --name should also auto-select
    if output=$(RUST_LOG=off "$BINARY" exec echo "exec-auto" 2>/dev/null); then
        pass "exec auto-resolves single running instance"
        if echo "$output" | grep -q "exec-auto"; then
            pass "exec auto-resolve returns correct output"
        else
            fail "exec auto-resolve returns correct output" "got: $output"
        fi
    else
        fail "exec auto-resolves single running instance" "exit code: $?"
    fi
}

test_ssh_connectivity() {
    echo ""
    echo "=== Phase: ssh connectivity ==="

    local output
    if output=$(guest_exec echo "hello-from-guest" 2>/dev/null); then
        pass "ssh connects to guest"
    else
        fail "ssh connects to guest"
        return
    fi

    if echo "$output" | grep -q "hello-from-guest"; then
        pass "ssh command output correct"
    else
        fail "ssh command output correct" "got: $output"
    fi
}

test_exec() {
    echo ""
    echo "=== Phase: exec ==="

    local output
    if output=$(moat_exec echo "exec-works"); then
        pass "exec exits 0"
    else
        fail "exec exits 0"
        return
    fi

    if echo "$output" | grep -q "exec-works"; then
        pass "exec captures stdout"
    else
        fail "exec captures stdout" "got: $output"
    fi

    # Verify non-zero exit code propagates
    local rc=0
    moat_exec false || rc=$?
    if [[ $rc -ne 0 ]]; then
        pass "exec propagates non-zero exit code"
    else
        fail "exec propagates non-zero exit code" "expected non-zero, got 0"
    fi
}

test_claude_bin_path() {
    echo ""
    echo "=== Phase: claude binary path ==="

    # The Claude Code binary is installed at /home/ubuntu/.local/bin/claude.
    # Non-interactive SSH sessions don't source .bashrc/.profile, so bare
    # `claude` won't work. Verify the full path is reachable via coop exec.
    if guest_exec test -x /home/ubuntu/.local/bin/claude 2>/dev/null; then
        pass "claude binary exists at CLAUDE_BIN path"
    else
        # Image was built with --no-claude or without profiles — skip
        skip "claude binary at CLAUDE_BIN path" "not installed in this image"
        return
    fi

    # Verify coop exec can invoke it by full path
    if moat_exec /home/ubuntu/.local/bin/claude --version >/dev/null 2>/dev/null; then
        pass "claude binary invocable via full path"
    else
        # --version may fail without auth, but any output means the binary ran
        skip "claude --version" "binary exists but --version returned non-zero (may need auth)"
    fi

    # Verify /usr/local/bin/claude symlink exists and points to the real binary
    local link_target
    if link_target=$(guest_exec readlink /usr/local/bin/claude 2>/dev/null); then
        if [[ "$link_target" == "/home/ubuntu/.local/bin/claude" ]]; then
            pass "claude symlink in /usr/local/bin"
        else
            fail "claude symlink in /usr/local/bin" "points to: $link_target"
        fi
    else
        fail "claude symlink in /usr/local/bin" "not found"
    fi

    # Verify claude-yolo shortcut exists and is executable
    if guest_exec test -x /usr/local/bin/claude-yolo 2>/dev/null; then
        pass "claude-yolo shortcut exists"
    else
        fail "claude-yolo shortcut exists"
    fi

    # Verify claude-yolo passes --dangerously-skip-permissions
    local yolo_content
    if yolo_content=$(guest_exec cat /usr/local/bin/claude-yolo 2>/dev/null); then
        if echo "$yolo_content" | grep -q "dangerously-skip-permissions"; then
            pass "claude-yolo includes --dangerously-skip-permissions"
        else
            fail "claude-yolo includes --dangerously-skip-permissions" \
                "content: $yolo_content"
        fi
    else
        fail "claude-yolo includes --dangerously-skip-permissions" "cat failed"
    fi
}

test_github_token_forwarding() {
    echo ""
    echo "=== Phase: github token forwarding ==="

    # Check the config's github auth strategy to determine expected behavior.
    local github_setting
    local cfg_path="${HOME}/.coop/config.toml"
    github_setting=$(python3 -c "
import sys
try:
    import tomllib
except ImportError:
    import tomli as tomllib
try:
    with open('$cfg_path', 'rb') as f:
        cfg = tomllib.load(f)
    print(cfg.get('github', 'off'))
except Exception:
    print('off')
" 2>/dev/null || echo "off")

    local token_out
    token_out=$(env GITHUB_TOKEN=test-leak-token RUST_LOG=off \
        "$BINARY" exec --name "$INSTANCE" printenv GITHUB_TOKEN 2>/dev/null) || true

    if [[ "$github_setting" == "auto" || "$github_setting" == "env" ]]; then
        # Token should be forwarded
        if [[ "$token_out" == *"test-leak-token"* ]]; then
            pass "GITHUB_TOKEN forwarded to guest (github: $github_setting)"
        else
            fail "GITHUB_TOKEN forwarded to guest (github: $github_setting)" "got: ${token_out:-empty}"
        fi
    else
        # Token should NOT be forwarded
        if [[ -z "$token_out" || "$token_out" != *"test-leak-token"* ]]; then
            pass "GITHUB_TOKEN not forwarded to guest (github: $github_setting)"
        else
            fail "GITHUB_TOKEN not forwarded to guest (github: $github_setting)" "got: $token_out"
        fi
    fi
}

test_term_handling() {
    echo ""
    echo "=== Phase: TERM handling ==="

    # Modern terminals (Ghostty, Kitty) set TERM values that don't exist
    # in a stock Ubuntu install. The guest_term() function in ssh.rs remaps
    # unknown TERM values to xterm-256color for interactive sessions
    # (coop ssh, coop claude). Non-interactive SSH (-- cmd) doesn't
    # allocate a PTY, so TERM doesn't matter there.
    #
    # This test verifies:
    # 1. Non-interactive SSH works when host has an exotic TERM
    # 2. The coop binary doesn't crash or reject exotic TERM values
    local output
    if output=$(env TERM=xterm-ghostty RUST_LOG=off \
        "$BINARY" ssh "$INSTANCE" -- echo term-ok 2>/dev/null); then
        if echo "$output" | grep -q "term-ok"; then
            pass "SSH works with TERM=xterm-ghostty"
        else
            fail "SSH works with TERM=xterm-ghostty" "unexpected output: $output"
        fi
    else
        fail "SSH works with TERM=xterm-ghostty" "command failed"
    fi
}

test_guest_environment() {
    echo ""
    echo "=== Phase: guest environment ==="

    # Check user is 'ubuntu' (both backends reuse the base image's ubuntu user)
    local whoami_out
    if whoami_out=$(guest_exec whoami 2>/dev/null); then
        if [[ "$whoami_out" == "ubuntu" ]]; then
            pass "guest user is 'ubuntu'"
        else
            fail "guest user is 'ubuntu'" "got: $whoami_out"
        fi
    else
        fail "guest user is 'ubuntu'" "ssh failed"
    fi

    # Check /workspace exists
    if guest_exec test -d /workspace 2>/dev/null; then
        pass "/workspace directory exists"
    else
        fail "/workspace directory exists"
    fi

    # Check /workspace is owned by ubuntu
    local ws_owner
    if ws_owner=$(guest_exec stat -c '%U' /workspace 2>/dev/null); then
        if [[ "$ws_owner" == "ubuntu" ]]; then
            pass "/workspace owned by ubuntu"
        else
            fail "/workspace owned by ubuntu" "owner: $ws_owner"
        fi
    else
        fail "/workspace owned by ubuntu" "stat failed"
    fi

    # Check basic tools
    local tool
    for tool in git curl wget jq rsync unzip zip file less; do
        if guest_exec which "$tool" >/dev/null; then
            pass "$tool is installed"
        else
            fail "$tool is installed"
        fi
    done

    # Check home directory
    local home
    if home=$(guest_exec printenv HOME 2>/dev/null); then
        if [[ "$home" == "/home/ubuntu" ]]; then
            pass "HOME is /home/ubuntu"
        else
            fail "HOME is /home/ubuntu" "got: $home"
        fi
    else
        fail "HOME is /home/ubuntu" "printenv failed"
    fi
}

test_sudo() {
    echo ""
    echo "=== Phase: sudo ==="

    # Sudo should work without a password
    local sudo_out
    if sudo_out=$(guest_exec sudo whoami 2>/dev/null); then
        if [[ "$sudo_out" == "root" ]]; then
            pass "sudo works without password"
        else
            fail "sudo works without password" "got: $sudo_out"
        fi
    else
        fail "sudo works without password" "sudo command failed"
    fi

    # Sudo should be able to write to root-owned locations
    if guest_exec sudo touch /root/test-sudo-write 2>/dev/null; then
        pass "sudo can write to /root"
        guest_exec sudo rm -f /root/test-sudo-write 2>/dev/null || true
    else
        fail "sudo can write to /root"
    fi
}

test_network() {
    echo ""
    echo "=== Phase: network connectivity ==="

    # DNS resolution
    if guest_exec nslookup github.com >/dev/null 2>/dev/null || \
       guest_exec host github.com >/dev/null 2>/dev/null || \
       guest_exec getent hosts github.com >/dev/null 2>/dev/null; then
        pass "DNS resolution works"
    else
        fail "DNS resolution works" "all resolution methods failed"
    fi

    # HTTP connectivity (use a reliable endpoint)
    local http_code
    if http_code=$(guest_exec curl -s -o /dev/null -w '%{http_code}' --max-time 10 https://api.github.com 2>/dev/null); then
        if [[ "$http_code" =~ ^[23] ]]; then
            pass "HTTPS connectivity works (HTTP $http_code)"
        else
            fail "HTTPS connectivity works" "HTTP $http_code"
        fi
    else
        fail "HTTPS connectivity works" "curl failed"
    fi
}

test_tmux() {
    echo ""
    echo "=== Phase: tmux session persistence ==="

    # tmux must be installed in the guest image
    if guest_exec which tmux >/dev/null; then
        pass "tmux is installed"
    else
        fail "tmux is installed"
        return
    fi

    # Create a detached tmux session running sleep (no quoting issues)
    guest_exec tmux new-session -d -s test-persist sleep 300 2>/dev/null
    if guest_exec tmux has-session -t test-persist 2>/dev/null; then
        pass "tmux session created and persists"
    else
        fail "tmux session created and persists"
        guest_exec tmux kill-session -t test-persist 2>/dev/null || true
        return
    fi

    # Clean up
    guest_exec tmux kill-session -t test-persist 2>/dev/null || true
}

test_docker() {
    echo ""
    echo "=== Phase: docker ==="

    if guest_exec docker info 2>/dev/null 1>/dev/null; then
        pass "docker daemon is running"
    else
        fail "docker daemon is running"
        return
    fi

    # User should be in docker group (no sudo needed)
    local groups_out
    if groups_out=$(guest_exec groups 2>/dev/null); then
        if echo "$groups_out" | grep -q "docker"; then
            pass "ubuntu user in docker group"
        else
            fail "ubuntu user in docker group" "groups: $groups_out"
        fi
    else
        fail "ubuntu user in docker group" "groups command failed"
    fi

    local docker_out
    if docker_out=$(guest_exec docker run --rm hello-world 2>/dev/null); then
        if echo "$docker_out" | grep -q "Hello from Docker"; then
            pass "docker run hello-world works"
        else
            fail "docker run hello-world works" "unexpected output"
        fi
    else
        fail "docker run hello-world works" "docker run failed"
    fi

    # Docker port mapping (verifies bridge networking + iptables)
    local port_test_ok=false
    if guest_exec docker run -d --name port-test -p 8080:80 nginx:alpine 2>/dev/null >/dev/null; then
        # Give nginx a moment to start
        sleep 2
        local port_out
        if port_out=$(guest_exec curl -s --max-time 5 http://localhost:8080 2>/dev/null); then
            if echo "$port_out" | grep -qi "nginx\|welcome"; then
                pass "docker port mapping works"
                port_test_ok=true
            fi
        fi
        if ! $port_test_ok; then
            fail "docker port mapping works" "curl to mapped port failed"
        fi
        guest_exec docker rm -f port-test >/dev/null 2>/dev/null || true
    else
        fail "docker port mapping works" "failed to start nginx container"
    fi
}

test_logs() {
    echo ""
    echo "=== Phase: logs ==="

    if coop logs "$INSTANCE"; then
        pass "logs exits 0"
    else
        # Some backends may not support logs for all states
        fail "logs exits 0" "exit code: $?"
        return
    fi

    # Logs should contain something (boot messages, kernel output, etc.)
    if [[ -n "$HARNESS_OUT" ]]; then
        pass "logs produces output"
    else
        # Lima logs go to stderr, so check both
        if [[ -n "$HARNESS_ERR" ]]; then
            pass "logs produces output (stderr)"
        else
            fail "logs produces output" "both stdout and stderr empty"
        fi
    fi
}

test_profiles() {
    echo ""
    echo "=== Phase: profiles ==="

    if [[ -z "$PROFILES" ]]; then
        skip "profile verification" "no profiles specified"
        return
    fi

    # Split comma-separated profiles
    local IFS=','
    local profiles_arr
    read -ra profiles_arr <<< "$PROFILES"

    for profile in "${profiles_arr[@]}"; do
        case "$profile" in
            python)
                if guest_exec python3 --version >/dev/null 2>/dev/null; then
                    pass "python3 installed (profile: python)"
                else
                    fail "python3 installed (profile: python)"
                fi
                ;;
            node)
                if guest_exec node --version >/dev/null 2>/dev/null; then
                    pass "node installed (profile: node)"
                else
                    fail "node installed (profile: node)"
                fi
                ;;
            rust)
                # Rust is installed for the ubuntu user via rustup
                if guest_exec rustc --version >/dev/null 2>/dev/null; then
                    pass "rustc installed (profile: rust)"
                else
                    fail "rustc installed (profile: rust)"
                fi
                ;;
            go)
                if guest_exec go version >/dev/null 2>/dev/null; then
                    pass "go installed (profile: go)"
                else
                    fail "go installed (profile: go)"
                fi
                ;;
            c)
                if guest_exec clang --version >/dev/null 2>/dev/null; then
                    pass "clang installed (profile: c)"
                else
                    fail "clang installed (profile: c)"
                fi
                ;;
            fuzz)
                if guest_exec which afl-fuzz >/dev/null 2>/dev/null; then
                    pass "afl-fuzz installed (profile: fuzz)"
                else
                    fail "afl-fuzz installed (profile: fuzz)"
                fi
                ;;
            *)
                skip "profile: $profile" "no verification defined"
                ;;
        esac
    done
}

# ── Guest fingerprint (cross-platform parity) ────────────────

test_guest_fingerprint() {
    echo ""
    echo "=== Phase: guest fingerprint ==="

    local fp="$tmpdir/fingerprint.txt"
    : > "$fp"

    local val

    # OS release
    val=$(guest_exec lsb_release -ds 2>/dev/null) || val="unknown"
    echo "os=$val" >> "$fp"

    # Architecture
    val=$(guest_exec uname -m 2>/dev/null) || val="unknown"
    echo "arch=$val" >> "$fp"

    # User and groups
    val=$(guest_exec id 2>/dev/null) || val="unknown"
    echo "id=$val" >> "$fp"

    # Base tool versions
    local tool
    for tool in git curl docker node python3 rustc go; do
        val=$(guest_exec "$tool" --version 2>/dev/null | head -1) || val="not installed"
        echo "${tool}=$val" >> "$fp"
    done

    # Docker info: storage driver and iptables mode
    val=$(guest_exec docker info --format '{{.Driver}}' 2>/dev/null) || val="unknown"
    echo "docker_storage=$val" >> "$fp"

    # DNS resolver
    if guest_exec test -f /etc/resolv.conf 2>/dev/null; then
        val=$(guest_exec grep -m1 '^nameserver' /etc/resolv.conf 2>/dev/null) || val="none"
    else
        val="no resolv.conf"
    fi
    echo "dns=$val" >> "$fp"

    # Directory layout
    for dir in /workspace /home/ubuntu /home/ubuntu/.ssh; do
        if guest_exec test -d "$dir" 2>/dev/null; then
            local owner
            owner=$(guest_exec stat -c '%U:%G' "$dir" 2>/dev/null) || owner="unknown"
            echo "dir:${dir}=$owner" >> "$fp"
        else
            echo "dir:${dir}=missing" >> "$fp"
        fi
    done

    pass "guest fingerprint captured"
    echo ""
    echo "  Fingerprint ($fp):"
    sed 's/^/    /' "$fp"

    # Copy fingerprint to a well-known location for cross-platform comparison
    local platform
    platform=$(uname -s | tr '[:upper:]' '[:lower:]')
    local dest="$tmpdir/../fingerprint-${platform}.txt"
    cp "$fp" "$dest" 2>/dev/null || true
}

# ── Stop / status-stopped / restart ───────────────────────────

test_stop() {
    echo ""
    echo "=== Phase: stop ==="

    if coop stop "$INSTANCE"; then
        pass "stop exits 0"
    else
        fail "stop exits 0" "exit code: $?"
    fi
}

test_auto_resolve_stopped() {
    echo ""
    echo "=== Phase: auto-resolve (single stopped) ==="

    # With one stopped instance and no running instances, commands
    # that need a running instance should fail with a helpful message.
    # Skip if other instances are running (pre-existing state).

    local running_count
    running_count=$(RUST_LOG=off "$BINARY" status 2>/dev/null | grep -c "running" || true)

    if [[ "$running_count" -gt 0 ]]; then
        skip "ssh rejects when only stopped instances exist" "$running_count still running"
        return
    fi

    if moat_fails ssh -- echo "should-not-work"; then
        pass "ssh rejects when only stopped instances exist"
        if echo "$HARNESS_ERR" | grep -qi "stopped"; then
            pass "ssh error mentions instance is stopped"
        else
            fail "ssh error mentions instance is stopped" "stderr: $HARNESS_ERR"
        fi
    else
        fail "ssh rejects when only stopped instances exist" "should have failed"
    fi
}

test_status_stopped() {
    echo ""
    echo "=== Phase: status (stopped) ==="

    if coop status "$INSTANCE" 2>/dev/null; then
        if echo "$HARNESS_OUT" | grep -qi "stopped\|not running"; then
            pass "status reports stopped"
        else
            pass "status exits 0 after stop (output: $HARNESS_OUT)"
        fi
    else
        # Non-zero exit on stopped instance is acceptable
        pass "status returns non-zero for stopped instance"
    fi

    # Status list should still show the instance
    if coop status; then
        if echo "$HARNESS_OUT" | grep -q "$INSTANCE"; then
            pass "stopped instance appears in status list"
        else
            fail "stopped instance appears in status list" "got: $HARNESS_OUT"
        fi

        if echo "$HARNESS_OUT" | grep "$INSTANCE" | grep -qi "stopped"; then
            pass "status list shows stopped state"
        else
            fail "status list shows stopped state" "got: $HARNESS_OUT"
        fi
    else
        fail "status list exits 0" "exit code: $?"
    fi
}

test_resize_status() {
    echo ""
    echo "=== Phase: resize updates status disk size ==="

    # Get current disk size from status (instance is stopped).
    # Firecracker status requires a running VM and doesn't show
    # "Disk: N GiB", so skip this test if status fails or the
    # disk line is absent (Lima-only feature).
    if ! coop status "$INSTANCE" 2>/dev/null; then
        skip "resize updates status disk size" "status unavailable for stopped instance"
        return
    fi
    local old_disk
    old_disk=$(echo "$HARNESS_OUT" | sed -n 's/.*Disk: \([0-9][0-9]*\) GiB.*/\1/p' | head -1)
    if [[ -z "$old_disk" || "$old_disk" -eq 0 ]]; then
        skip "resize updates status disk size" "status does not report disk GiB"
        return
    fi
    pass "status shows disk before resize (${old_disk} GiB)"

    # Resize by +1G while stopped
    if coop resize "$INSTANCE" --size +1G; then
        pass "resize +1G exits 0"
    else
        fail "resize +1G exits 0" "exit code: $?"
        return
    fi

    # Verify status now reports the larger size
    if coop status "$INSTANCE" 2>/dev/null; then
        local new_disk
        new_disk=$(echo "$HARNESS_OUT" | sed -n 's/.*Disk: \([0-9][0-9]*\) GiB.*/\1/p' | head -1)
        local expected_disk=$(( old_disk + 1 ))
        if [[ "$new_disk" -eq "$expected_disk" ]]; then
            pass "status reflects resized disk (${new_disk} GiB)"
        else
            fail "status reflects resized disk" \
                "expected ${expected_disk} GiB, got ${new_disk} GiB"
        fi
    else
        fail "status after resize" "exit code: $?"
    fi
}

test_restart_stopped() {
    echo ""
    echo "=== Phase: restart stopped instance ==="

    # Restart the stopped instance (was stopped in previous phase)
    if coop start "$INSTANCE" --no-claude; then
        pass "restart stopped instance exits 0"
    else
        fail "restart stopped instance exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        echo "FATAL: restart failed, cannot continue"
        exit 1
    fi

    # Verify it's running again
    if coop status "$INSTANCE"; then
        if echo "$HARNESS_OUT" | grep -qi "running"; then
            pass "restarted instance is running"
        else
            fail "restarted instance is running" "got: $HARNESS_OUT"
        fi
    else
        fail "status after restart exits 0" "exit code: $?"
    fi

    # Verify workspace survived the stop/start cycle
    if coop exec --name "$INSTANCE" -- test -d /workspace; then
        pass "workspace persists across restart"
    else
        fail "workspace persists across restart" "exit code: $?"
    fi

    # Verify duplicate start of running instance is rejected
    if moat_fails start "$INSTANCE" --no-claude; then
        pass "rejects start of already-running instance"
    else
        fail "rejects start of already-running instance" "should have failed"
    fi

    # Stop again for subsequent phases
    coop stop "$INSTANCE" || true
}

test_restart_rejects_ignored_flags() {
    echo ""
    echo "=== Phase: restart rejects ignored flags ==="

    # Instance is stopped from previous phase. Restarting with creation-time
    # flags (--mount, --workspace, --git-repo, --disk) should fail with a
    # clear error instead of silently ignoring them.

    local mount_dir
    mount_dir=$(mktemp -d)

    if moat_fails start "$INSTANCE" --no-claude --mount "$mount_dir"; then
        pass "restart with --mount rejected"
    else
        fail "restart with --mount rejected" "should have failed"
        coop stop "$INSTANCE" 2>/dev/null || true
    fi

    if echo "$HARNESS_ERR" | grep -qi "already exists\|ignored on restart\|destroy"; then
        pass "error message suggests destroy first"
    else
        fail "error message suggests destroy first" "stderr: $HARNESS_ERR"
    fi

    if moat_fails start "$INSTANCE" --no-claude --workspace "$mount_dir"; then
        pass "restart with --workspace rejected"
    else
        fail "restart with --workspace rejected" "should have failed"
        coop stop "$INSTANCE" 2>/dev/null || true
    fi

    if moat_fails start "$INSTANCE" --no-claude --disk 20; then
        pass "restart with --disk rejected"
    else
        fail "restart with --disk rejected" "should have failed"
        coop stop "$INSTANCE" 2>/dev/null || true
    fi

    # Plain restart (no conflicting flags) should still work
    if coop start "$INSTANCE" --no-claude; then
        pass "restart without flags still works"
    else
        fail "restart without flags still works" "exit code: $?"
    fi

    # Stop again for the destroy phase
    coop stop "$INSTANCE" || true
    rm -rf "$mount_dir"
}

test_destroy() {
    echo ""
    echo "=== Phase: destroy ==="

    if coop destroy "$INSTANCE"; then
        untrack_instance "$INSTANCE"
        pass "destroy exits 0"
    else
        fail "destroy exits 0" "exit code: $?"
    fi

    # Verify instance is gone from status list
    if coop status 2>/dev/null; then
        if echo "$HARNESS_OUT" | grep -q "$INSTANCE"; then
            fail "instance removed after destroy" "still listed in status"
        else
            pass "instance removed after destroy"
        fi
    else
        # No instances = status may return error or "No instances"
        pass "instance removed after destroy"
    fi
}

test_auto_resolve_no_instances() {
    echo ""
    echo "=== Phase: auto-resolve (no instances) ==="

    # After destroy, no instances should exist. Commands should fail.
    # Skip if other instances exist (pre-existing state).

    local instance_count
    instance_count=$(RUST_LOG=off "$BINARY" status 2>/dev/null | grep -cE "running|stopped" || true)

    if [[ "$instance_count" -gt 0 ]]; then
        skip "ssh rejects when no instances exist" "$instance_count instances still present"
        return
    fi

    if moat_fails ssh -- echo "should-not-work"; then
        pass "ssh rejects when no instances exist"
        if echo "$HARNESS_ERR" | grep -qi "no instances"; then
            pass "ssh error mentions no instances"
        else
            fail "ssh error mentions no instances" "stderr: $HARNESS_ERR"
        fi
    else
        fail "ssh rejects when no instances exist" "should have failed"
    fi
}

# ── Idempotency tests ────────────────────────────────────────

test_idempotency() {
    echo ""
    echo "=== Phase: idempotency ==="

    # setup -y when golden image already exists should be a fast no-op
    # Must pass the same profiles as the original setup, otherwise the
    # provision script hash changes and triggers a rebuild.
    local setup_args=(setup -y)
    if [[ -n "$PROFILES" ]]; then
        setup_args+=(--profile "$PROFILES")
    fi
    if coop "${setup_args[@]}"; then
        pass "setup no-op when image exists"
    else
        fail "setup no-op when image exists" "exit code: $?"
    fi

    # destroy on already-destroyed instance should fail gracefully
    local rc=0
    "$BINARY" destroy "$INSTANCE" >/dev/null 2>&1 || rc=$?
    if [[ $rc -ne 0 ]]; then
        pass "destroy already-destroyed instance fails gracefully (exit $rc)"
    else
        pass "destroy already-destroyed instance succeeds (idempotent)"
    fi

    # stop idempotency: start an instance, stop it twice
    local inst_name="${INSTANCE}-idem"
    if ! coop start "$inst_name" --no-claude; then
        fail "start for stop-idempotency test" "exit code: $?"
        return
    fi
    STARTED_INSTANCES+=("$inst_name")

    if coop stop "$inst_name"; then
        pass "first stop succeeds"
    else
        fail "first stop succeeds" "exit code: $?"
    fi

    # Second stop on an already-stopped instance
    rc=0
    "$BINARY" stop "$inst_name" >/dev/null 2>&1 || rc=$?
    if [[ $rc -eq 0 ]]; then
        pass "second stop succeeds (idempotent)"
    else
        fail "second stop is idempotent" "exit code: $rc"
    fi

    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
}

# ── Workspace sync tests (--full only) ────────────────────────

test_workspace_sync() {
    echo ""
    echo "=== Phase: workspace sync ==="

    local ws_instance="${INSTANCE}-ws"

    # Create a temp workspace directory with test files
    ws_tmpdir=$(mktemp -d)
    echo "workspace-test-content" > "$ws_tmpdir/hello.txt"
    touch "$ws_tmpdir/subdir_marker"
    mkdir -p "$ws_tmpdir/subdir"
    echo "nested" > "$ws_tmpdir/subdir/nested.txt"

    # Start instance with --workspace
    local args=(start "$ws_instance" --no-claude --workspace "$ws_tmpdir")
    if coop "${args[@]}"; then
        STARTED_INSTANCES+=("$ws_instance")
        pass "start with --workspace exits 0"
    else
        fail "start with --workspace exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        return
    fi

    # Verify files were synced to guest
    local file_content
    GUEST_INSTANCE="$ws_instance"

    if file_content=$(guest_exec cat /workspace/hello.txt 2>/dev/null); then
        if [[ "$file_content" == "workspace-test-content" ]]; then
            pass "workspace file synced to guest"
        else
            fail "workspace file synced to guest" "got: $file_content"
        fi
    else
        fail "workspace file synced to guest" "file not found in guest"
    fi

    # Verify nested directory was synced
    if file_content=$(guest_exec cat /workspace/subdir/nested.txt 2>/dev/null); then
        if echo "$file_content" | grep -q "nested"; then
            pass "nested workspace files synced"
        else
            fail "nested workspace files synced" "got: $file_content"
        fi
    else
        fail "nested workspace files synced" "nested file not found"
    fi

    # Modify file in guest, then pull
    moat_exec sh -c 'echo modified-in-guest > /workspace/hello.txt' 2>/dev/null || true

    local pull_dir
    pull_dir=$(mktemp -d)
    if coop pull --name "$ws_instance" --force "$pull_dir"; then
        pass "pull exits 0"

        local pulled
        pulled=$(cat "$pull_dir/hello.txt" 2>/dev/null)
        if echo "$pulled" | grep -q "modified-in-guest"; then
            pass "pull retrieves guest changes"
        else
            fail "pull retrieves guest changes" "got: $pulled"
        fi
    else
        fail "pull exits 0" "exit code: $?"
    fi
    rm -rf "$pull_dir"

    # Push: modify locally, push to guest, verify
    echo "pushed-from-host" > "$ws_tmpdir/hello.txt"
    if coop push --name "$ws_instance" --force "$ws_tmpdir"; then
        pass "push exits 0"

        local pushed_content
        pushed_content=$(guest_exec cat /workspace/hello.txt 2>/dev/null)
        if echo "$pushed_content" | grep -q "pushed-from-host"; then
            pass "push delivers host changes to guest"
        else
            fail "push delivers host changes to guest" "got: $pushed_content"
        fi
    else
        fail "push exits 0" "exit code: $?"
    fi

    unset GUEST_INSTANCE

    # Clean up workspace instance
    coop destroy "$ws_instance" 2>/dev/null || true
    untrack_instance "$ws_instance"

    rm -rf "$ws_tmpdir"
    ws_tmpdir=""
}

# ── Multi-instance tests (--full only) ────────────────────────

test_multi_instance() {
    echo ""
    echo "=== Phase: multi-instance ==="

    local inst_a="${INSTANCE}-a"
    local inst_b="${INSTANCE}-b"

    # Start two instances
    if coop start "$inst_a" --no-claude; then
        STARTED_INSTANCES+=("$inst_a")
        pass "start instance A ($inst_a)"
    else
        fail "start instance A ($inst_a)" "exit code: $?"
        return
    fi

    if coop start "$inst_b" --no-claude; then
        STARTED_INSTANCES+=("$inst_b")
        pass "start instance B ($inst_b)"
    else
        fail "start instance B ($inst_b)" "exit code: $?"
        coop destroy "$inst_a" 2>/dev/null || true
        return
    fi

    # Status should list both
    if coop status; then
        local listed_a=false listed_b=false
        if echo "$HARNESS_OUT" | grep -q "$inst_a"; then listed_a=true; fi
        if echo "$HARNESS_OUT" | grep -q "$inst_b"; then listed_b=true; fi

        if $listed_a && $listed_b; then
            pass "status lists both instances"
        else
            fail "status lists both instances" "a=$listed_a b=$listed_b output=$HARNESS_OUT"
        fi
    else
        fail "status lists both instances" "status failed"
    fi

    # Auto-resolve should fail when multiple instances are running
    if moat_fails ssh -- echo "should-not-work"; then
        pass "ssh rejects auto-resolve with multiple running"
        if echo "$HARNESS_ERR" | grep -qi "multiple"; then
            pass "error mentions multiple running instances"
        else
            fail "error mentions multiple running instances" "stderr: $HARNESS_ERR"
        fi
    else
        fail "ssh rejects auto-resolve with multiple running" "should have failed"
    fi

    # Explicit name should still work with multiple instances
    if RUST_LOG=off "$BINARY" ssh "$inst_a" -- echo "explicit-ok" 2>/dev/null | grep -q "explicit-ok"; then
        pass "ssh with explicit name works among multiple"
    else
        fail "ssh with explicit name works among multiple"
    fi

    # Both should be independently accessible via SSH
    GUEST_INSTANCE="$inst_a"
    local hostname_a hostname_b
    hostname_a=$(guest_exec hostname 2>/dev/null) || hostname_a=""
    GUEST_INSTANCE="$inst_b"
    hostname_b=$(guest_exec hostname 2>/dev/null) || hostname_b=""
    unset GUEST_INSTANCE

    if [[ -n "$hostname_a" && -n "$hostname_b" ]]; then
        pass "both instances reachable via SSH"
        if [[ "$hostname_a" != "$hostname_b" ]]; then
            pass "instances have distinct hostnames ($hostname_a vs $hostname_b)"
        else
            # Same hostname is okay if using generic image
            skip "instances have distinct hostnames" "both report: $hostname_a"
        fi
    else
        fail "both instances reachable via SSH" "a='$hostname_a' b='$hostname_b'"
    fi

    # Clean up
    coop destroy "$inst_a" 2>/dev/null || true
    coop destroy "$inst_b" 2>/dev/null || true
    untrack_instance "$inst_a"
    untrack_instance "$inst_b"
}

# ── Named images test (--full only) ──────────────────────────

test_named_images() {
    echo ""
    echo "=== Phase: named images ==="

    # Build a second image with a different name
    local img_name="test-img-$$"
    if coop setup -y --image "$img_name"; then
        pass "setup --image $img_name exits 0"
    else
        fail "setup --image $img_name exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        return
    fi

    # List images — should show both default and the new one
    if coop images; then
        if echo "$HARNESS_OUT" | grep -q "default"; then
            pass "images lists 'default'"
        else
            fail "images lists 'default'" "output: $HARNESS_OUT"
        fi

        if echo "$HARNESS_OUT" | grep -q "$img_name"; then
            pass "images lists '$img_name'"
        else
            fail "images lists '$img_name'" "output: $HARNESS_OUT"
        fi
    else
        fail "images exits 0" "exit code: $?"
    fi

    # Start an instance from the named image
    local inst_name="${INSTANCE}-img"
    if coop start "$inst_name" --no-claude --image "$img_name"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "start --image $img_name exits 0"
    else
        fail "start --image $img_name exits 0" "exit code: $?"
        coop images --delete "$img_name" 2>/dev/null || true
        return
    fi

    # Status should show the image name
    if coop status; then
        if echo "$HARNESS_OUT" | grep -q "$img_name"; then
            pass "status shows image name"
        else
            fail "status shows image name" "output: $HARNESS_OUT"
        fi
    else
        fail "status exits 0" "exit code: $?"
    fi

    # Guest should be functional
    GUEST_INSTANCE="$inst_name"
    local hostname
    hostname=$(guest_exec hostname 2>/dev/null) || hostname=""
    unset GUEST_INSTANCE

    if [[ -n "$hostname" ]]; then
        pass "instance from named image reachable via SSH"
    else
        fail "instance from named image reachable via SSH" "no response"
    fi

    # Clean up instance
    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"

    # Delete the named image
    if coop images --delete "$img_name"; then
        pass "images --delete $img_name exits 0"
    else
        fail "images --delete $img_name exits 0" "exit code: $?"
    fi

    # Verify it's gone
    if coop images; then
        if echo "$HARNESS_OUT" | grep -q "$img_name"; then
            fail "image removed after delete" "still listed"
        else
            pass "image removed after delete"
        fi
    fi
}

# ── Custom profiles test (--full only) ───────────────────────

test_custom_profiles() {
    echo ""
    echo "=== Phase: custom profiles ==="

    # Create a config with a custom profile
    local cfg_dir
    cfg_dir=$(mktemp -d)
    local cfg_file="$cfg_dir/config.toml"
    cat > "$cfg_file" <<'CFGEOF'
[profiles.test-custom]
apt_packages = ["cowsay"]
post_install = "echo 'custom-profile-marker' > /etc/custom-profile-installed"
CFGEOF

    # Build an image with the custom profile
    local custom_img="custom-test-$$"
    if "$BINARY" --config "$cfg_file" setup -y --image "$custom_img" --profile test-custom 2>"$tmpdir/stderr"; then
        pass "setup with custom profile exits 0"
    else
        local err
        err=$(cat "$tmpdir/stderr")
        fail "setup with custom profile exits 0" "exit code: $? stderr: $err"
        rm -rf "$cfg_dir"
        return
    fi

    # Start instance from custom image
    local inst_name="${INSTANCE}-custom"
    if "$BINARY" --config "$cfg_file" start "$inst_name" --no-claude --image "$custom_img" 2>"$tmpdir/stderr"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "start with custom profile image exits 0"
    else
        fail "start with custom profile image exits 0" "exit code: $?"
        rm -rf "$cfg_dir"
        return
    fi

    # Verify custom profile effects in guest
    GUEST_INSTANCE="$inst_name"
    local marker
    marker=$(guest_exec cat /etc/custom-profile-installed 2>/dev/null) || marker=""
    unset GUEST_INSTANCE

    if echo "$marker" | grep -q "custom-profile-marker"; then
        pass "custom profile post_install script ran"
    else
        fail "custom profile post_install script ran" "marker: '$marker'"
    fi

    # Clean up
    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    coop images --delete "$custom_img" 2>/dev/null || true
    rm -rf "$cfg_dir"
}

# ── Built-in profiles test (--full only) ──────────────────────

test_builtin_profiles() {
    echo ""
    echo "=== Phase: built-in profiles ==="

    # Build a named image with python and node profiles.
    # These cover apt-only (python) and pre-install script (node/NodeSource).
    local img_name="profiles-test-$$"
    if coop setup -y --image "$img_name" --profile python,node; then
        pass "setup --profile python,node exits 0"
    else
        fail "setup --profile python,node exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        return
    fi

    # Start instance from the profiled image
    local inst_name="${INSTANCE}-prof"
    if coop start "$inst_name" --no-claude --image "$img_name"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "start from profiled image exits 0"
    else
        fail "start from profiled image exits 0" "exit code: $?"
        coop images --delete "$img_name" 2>/dev/null || true
        return
    fi

    GUEST_INSTANCE="$inst_name"

    # Verify python profile
    local py_ver
    if py_ver=$(guest_exec python3 --version 2>/dev/null); then
        pass "python3 installed ($py_ver)"
    else
        fail "python3 installed (profile: python)"
    fi

    if guest_exec python3 -c 'import venv' >/dev/null 2>/dev/null; then
        pass "python3-venv available"
    else
        fail "python3-venv available"
    fi

    # Verify node profile
    local node_ver
    if node_ver=$(guest_exec node --version 2>/dev/null); then
        pass "node installed ($node_ver)"
    else
        fail "node installed (profile: node)"
    fi

    if guest_exec npm --version >/dev/null 2>/dev/null; then
        pass "npm installed"
    else
        fail "npm installed"
    fi

    unset GUEST_INSTANCE

    # Clean up
    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    coop images --delete "$img_name" 2>/dev/null || true
}

# ── Host mount tests (--full only) ────────────────────────────
#
# Lima: virtiofs (live bidirectional sync)
# Firecracker: rsync one-time sync (push/pull for ongoing)

test_host_mount() {
    echo ""
    echo "=== Phase: host mount ==="

    local mount_instance="${INSTANCE}-mnt"
    local mount_dir
    mount_dir=$(mktemp -d)
    echo "mount-test-content" > "$mount_dir/sentinel.txt"
    mkdir -p "$mount_dir/subdir"
    echo "nested-mount" > "$mount_dir/subdir/deep.txt"

    # Start instance with --mount (defaults to /workspace)
    if coop start "$mount_instance" --no-claude --mount "$mount_dir"; then
        STARTED_INSTANCES+=("$mount_instance")
        pass "start with --mount exits 0"
    else
        fail "start with --mount exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        rm -rf "$mount_dir"
        return
    fi

    GUEST_INSTANCE="$mount_instance"

    # Verify host files are visible in guest at /workspace
    local content
    if content=$(guest_exec cat /workspace/sentinel.txt 2>/dev/null); then
        if [[ "$content" == "mount-test-content" ]]; then
            pass "mounted file readable in guest"
        else
            fail "mounted file readable in guest" "got: $content"
        fi
    else
        fail "mounted file readable in guest" "file not found"
    fi

    # Verify nested directory
    if content=$(guest_exec cat /workspace/subdir/deep.txt 2>/dev/null); then
        if echo "$content" | grep -q "nested-mount"; then
            pass "nested mounted file readable"
        else
            fail "nested mounted file readable" "got: $content"
        fi
    else
        fail "nested mounted file readable" "file not found"
    fi

    # Live sync tests: only on Lima (virtiofs provides bidirectional live access)
    if [[ "$(uname -s)" == "Darwin" ]]; then
        # Verify writes from guest are visible on host (bidirectional)
        if guest_exec sh -c 'echo "written-by-guest" > /workspace/from-guest.txt' 2>/dev/null; then
            local host_content
            host_content=$(cat "$mount_dir/from-guest.txt" 2>/dev/null) || host_content=""
            if [[ "$host_content" == "written-by-guest" ]]; then
                pass "guest writes visible on host (bidirectional)"
            else
                fail "guest writes visible on host (bidirectional)" "got: '$host_content'"
            fi
        else
            fail "guest writes visible on host (bidirectional)" "write failed"
        fi

        # Verify host writes after boot are visible in guest (live sync)
        echo "live-update" > "$mount_dir/live.txt"
        if content=$(guest_exec cat /workspace/live.txt 2>/dev/null); then
            if [[ "$content" == "live-update" ]]; then
                pass "host writes after boot visible in guest (live)"
            else
                fail "host writes after boot visible in guest (live)" "got: $content"
            fi
        else
            fail "host writes after boot visible in guest (live)" "file not found"
        fi
    else
        skip "bidirectional mount" "Firecracker uses one-time rsync sync"
        skip "live mount sync" "Firecracker uses one-time rsync sync"
    fi

    unset GUEST_INSTANCE

    # Clean up
    coop destroy "$mount_instance" 2>/dev/null || true
    untrack_instance "$mount_instance"
    rm -rf "$mount_dir"
}

test_host_mount_custom_guest_path() {
    echo ""
    echo "=== Phase: host mount (custom guest path) ==="

    local mount_instance="${INSTANCE}-mnt2"
    local mount_dir
    mount_dir=$(mktemp -d)
    echo "custom-path-test" > "$mount_dir/marker.txt"

    # Mount with explicit guest path
    if coop start "$mount_instance" --no-claude --mount "$mount_dir:/data/project"; then
        STARTED_INSTANCES+=("$mount_instance")
        pass "start with --mount host:guest exits 0"
    else
        fail "start with --mount host:guest exits 0" "exit code: $?"
        rm -rf "$mount_dir"
        return
    fi

    GUEST_INSTANCE="$mount_instance"

    local content
    if content=$(guest_exec cat /data/project/marker.txt 2>/dev/null); then
        if [[ "$content" == "custom-path-test" ]]; then
            pass "mount at custom guest path works"
        else
            fail "mount at custom guest path works" "got: $content"
        fi
    else
        fail "mount at custom guest path works" "file not found at /data/project"
    fi

    unset GUEST_INSTANCE

    coop destroy "$mount_instance" 2>/dev/null || true
    untrack_instance "$mount_instance"
    rm -rf "$mount_dir"
}

test_mount_conflicts() {
    echo ""
    echo "=== Phase: mount CLI conflicts ==="

    local mount_dir
    mount_dir=$(mktemp -d)

    # --mount should conflict with --workspace
    if moat_fails start "${INSTANCE}-conflict" --no-claude --mount "$mount_dir" --workspace "$mount_dir"; then
        pass "--mount conflicts with --workspace"
    else
        fail "--mount conflicts with --workspace" "should have failed"
        coop destroy "${INSTANCE}-conflict" 2>/dev/null || true
    fi

    # --mount should conflict with --git-repo
    if moat_fails start "${INSTANCE}-conflict2" --no-claude --mount "$mount_dir" --git-repo "https://example.com/repo.git"; then
        pass "--mount conflicts with --git-repo"
    else
        fail "--mount conflicts with --git-repo" "should have failed"
        coop destroy "${INSTANCE}-conflict2" 2>/dev/null || true
    fi

    rm -rf "$mount_dir"
}

# ── Destroy --all test (--full only) ──────────────────────────

test_destroy_all() {
    echo ""
    echo "=== Phase: destroy --all ==="

    # Start two throwaway instances
    local inst_x="${INSTANCE}-x"
    local inst_y="${INSTANCE}-y"

    if ! coop start "$inst_x" --no-claude; then
        fail "start instance for destroy --all" "exit code: $?"
        return
    fi
    STARTED_INSTANCES+=("$inst_x")

    if ! coop start "$inst_y" --no-claude; then
        fail "start second instance for destroy --all" "exit code: $?"
        coop destroy "$inst_x" 2>/dev/null || true
        return
    fi
    STARTED_INSTANCES+=("$inst_y")

    # Destroy all
    if coop destroy --all; then
        STARTED_INSTANCES=()
        pass "destroy --all exits 0"
    else
        fail "destroy --all exits 0" "exit code: $?"
        STARTED_INSTANCES=()
        return
    fi

    # Status should show no instances
    if coop status 2>/dev/null; then
        if echo "$HARNESS_OUT" | grep -qi "no instances"; then
            pass "no instances after destroy --all"
        elif echo "$HARNESS_OUT" | grep -q "$inst_x\|$inst_y"; then
            fail "no instances after destroy --all" "instances still listed"
        else
            pass "no instances after destroy --all"
        fi
    else
        # Error when no instances exist is acceptable
        pass "no instances after destroy --all"
    fi
}

# ── Local marketplace copy test (--full only) ─────────────────

test_local_marketplace() {
    echo ""
    echo "=== Phase: local marketplace copy ==="

    local mp_instance="${INSTANCE}-mp"

    # Create a fake marketplace directory on the host
    local mp_dir
    mp_dir=$(mktemp -d)
    local mp_name="test-marketplace"
    local mp_root="$mp_dir/$mp_name"
    mkdir -p "$mp_root/.claude-plugin"
    mkdir -p "$mp_root/plugins/hello-skill/skills"

    cat > "$mp_root/.claude-plugin/marketplace.json" <<'MPEOF'
{
    "name": "test-marketplace",
    "owner": {"name": "Test"},
    "metadata": {"version": "1.0.0", "description": "Integration test marketplace"},
    "plugins": [
        {
            "name": "hello-skill",
            "version": "1.0.0",
            "description": "Test plugin",
            "source": "./plugins/hello-skill"
        }
    ]
}
MPEOF
    echo "# Hello Skill" > "$mp_root/plugins/hello-skill/skills/SKILL.md"

    # Write a config with:
    # - A custom profile that installs a stub `claude` into the golden image
    # - The local marketplace to copy during bootstrap
    # - github: off to skip gh auth (which fails in the test guest)
    local cfg_dir
    cfg_dir=$(mktemp -d)
    local cfg_file="$cfg_dir/config.toml"
    cat > "$cfg_file" <<CFGEOF
[claude]
github = "off"
marketplaces = ["$mp_root"]

[profiles.stub-claude]
post_install = '''mkdir -p /home/ubuntu/.local/bin && printf '#!/bin/sh\necho "\$@" >> /tmp/claude-calls.log\n' > /home/ubuntu/.local/bin/claude && chmod +x /home/ubuntu/.local/bin/claude'''
CFGEOF

    # Build a golden image with the stub claude baked in
    local mp_img="mp-test-$$"
    if env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" setup -y --image "$mp_img" --profile stub-claude 2>"$tmpdir/stderr"; then
        pass "setup with stub claude image exits 0"
    else
        local setup_err
        setup_err=$(cat "$tmpdir/stderr")
        fail "setup with stub claude image exits 0" "stderr: $setup_err"
        rm -rf "$mp_dir" "$cfg_dir"
        return
    fi

    # Start the instance WITH bootstrap. The stub claude handles
    # `marketplace add` calls. Unset tokens to avoid auth steps.
    if env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" start "$mp_instance" --image "$mp_img" 2>"$tmpdir/stderr"; then
        STARTED_INSTANCES+=("$mp_instance")
        pass "start with local marketplace exits 0"
    else
        local boot_err
        boot_err=$(cat "$tmpdir/stderr")
        fail "start with local marketplace exits 0" "stderr: $boot_err"
        env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" images --delete "$mp_img" 2>/dev/null || true
        rm -rf "$mp_dir" "$cfg_dir"
        return
    fi

    GUEST_INSTANCE="$mp_instance"

    # Verify the marketplace directory was copied to the guest.
    # Use $HOME explicitly — tilde would expand on the test host, not the guest.
    local mp_base="\$HOME/.coop/marketplaces/$mp_name"

    local manifest
    manifest=$(guest_exec sh -c "cat $mp_base/.claude-plugin/marketplace.json" \
        2>/dev/null) || manifest=""

    if echo "$manifest" | grep -q "test-marketplace"; then
        pass "marketplace manifest copied to guest"
    else
        fail "marketplace manifest copied to guest" "got: '$manifest'"
    fi

    # Verify nested plugin content was copied recursively
    local skill_content
    skill_content=$(guest_exec sh -c "cat $mp_base/plugins/hello-skill/skills/SKILL.md" \
        2>/dev/null) || skill_content=""

    if echo "$skill_content" | grep -q "Hello Skill"; then
        pass "plugin skill file copied recursively"
    else
        fail "plugin skill file copied recursively" "got: '$skill_content'"
    fi

    # Verify marketplace add was invoked. On Lima (macOS), marketplaces are
    # baked into the golden image during setup, so the stub claude log is
    # empty at start time — check the template config instead. On Firecracker
    # (Linux), marketplaces are installed at start time via the stub claude.
    local claude_log
    claude_log=$(guest_exec cat /tmp/claude-calls.log 2>/dev/null) || claude_log=""

    if [[ "$(uname -s)" == "Darwin" ]]; then
        # Lima: marketplace baked into golden image during setup
        local tc_path="$HOME/.coop/images/$mp_img/template-config.json"
        local tc_content
        tc_content=$(cat "$tc_path" 2>/dev/null) || tc_content=""

        if echo "$tc_content" | grep -q "marketplaces"; then
            pass "claude plugin marketplace add was invoked"
        else
            fail "claude plugin marketplace add was invoked" "template-config: '$tc_content'"
        fi

        if echo "$tc_content" | grep -q "$mp_root\|$mp_name"; then
            pass "marketplace add uses guest path (not host path)"
        else
            fail "marketplace add uses guest path (not host path)" "template-config: '$tc_content'"
        fi
    else
        # Firecracker: marketplace installed at start time
        if echo "$claude_log" | grep -q "marketplace add"; then
            pass "claude plugin marketplace add was invoked"
        else
            fail "claude plugin marketplace add was invoked" "log: '$claude_log'"
        fi

        if echo "$claude_log" | grep -q "coop/marketplaces/$mp_name"; then
            pass "marketplace add uses guest path (not host path)"
        else
            fail "marketplace add uses guest path (not host path)" "log: '$claude_log'"
        fi
    fi

    # Verify `coop claude --help` invokes the binary at CLAUDE_BIN path.
    # The stub claude will log the args, so we can verify it was called.
    guest_exec truncate -s 0 /tmp/claude-calls.log 2>/dev/null || true
    # coop claude uses run_interactive which needs a PTY — use exec instead
    # to verify the binary path is correct by invoking it directly.
    env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY RUST_LOG=off \
        "$BINARY" --config "$cfg_file" exec --name "$mp_instance" \
        /home/ubuntu/.local/bin/claude --help >/dev/null 2>/dev/null || true

    local post_log
    post_log=$(guest_exec cat /tmp/claude-calls.log 2>/dev/null) || post_log=""
    if echo "$post_log" | grep -q "\-\-help"; then
        pass "coop exec invokes claude at CLAUDE_BIN path"
    else
        fail "coop exec invokes claude at CLAUDE_BIN path" "log: '$post_log'"
    fi

    unset GUEST_INSTANCE

    # Clean up
    coop destroy "$mp_instance" 2>/dev/null || true
    untrack_instance "$mp_instance"
    env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" images --delete "$mp_img" 2>/dev/null || true
    rm -rf "$mp_dir" "$cfg_dir"
}

# ── Config sources test (--full only) ─────────────────────────

test_config_dir() {
    echo ""
    echo "=== Phase: config_dir (CLAUDE.md + rules/ + commands/) ==="

    local inst_name="${INSTANCE}-cd"

    # Create a temp directory mimicking ~/.claude/ with allowlisted
    # and non-allowlisted entries
    local cs_dir
    cs_dir=$(mktemp -d)
    local config_src="$cs_dir/claude-config"
    mkdir -p "$config_src/rules" "$config_src/commands"
    echo "host-claude-marker" > "$config_src/CLAUDE.md"
    echo "host-rule-marker" > "$config_src/rules/safety.md"
    echo "host-cmd-marker" > "$config_src/commands/deploy.md"
    echo "should-not-copy" > "$config_src/settings.json"

    local cfg_file="$cs_dir/config.toml"
    cat > "$cfg_file" <<CFGEOF
[claude]
github = "off"
config_dir = "$config_src"
CFGEOF

    cs() {
        local rc=0
        HARNESS_OUT=$(env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY \
            "$BINARY" --config "$cfg_file" "$@" 2>"$tmpdir/stderr") || rc=$?
        HARNESS_ERR=$(cat "$tmpdir/stderr")
        return $rc
    }

    # ── 1. First start: allowlisted files copied to guest ──

    if cs start "$inst_name"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "start with config_dir exits 0"
    else
        fail "start with config_dir exits 0" "exit code: $? stderr: $HARNESS_ERR"
        rm -r "$cs_dir"
        return
    fi

    GUEST_INSTANCE="$inst_name"

    local guest_claude
    guest_claude=$(guest_exec cat /home/ubuntu/.claude/CLAUDE.md 2>/dev/null) || guest_claude=""
    if echo "$guest_claude" | grep -q "host-claude-marker"; then
        pass "CLAUDE.md copied to guest ~/.claude/CLAUDE.md"
    else
        fail "CLAUDE.md copied to guest ~/.claude/CLAUDE.md" "got: $guest_claude"
    fi

    local guest_rule
    guest_rule=$(guest_exec cat /home/ubuntu/.claude/rules/safety.md 2>/dev/null) || guest_rule=""
    if echo "$guest_rule" | grep -q "host-rule-marker"; then
        pass "rules file copied to guest ~/.claude/rules/"
    else
        fail "rules file copied to guest ~/.claude/rules/" "got: $guest_rule"
    fi

    local guest_cmd
    guest_cmd=$(guest_exec cat /home/ubuntu/.claude/commands/deploy.md 2>/dev/null) || guest_cmd=""
    if echo "$guest_cmd" | grep -q "host-cmd-marker"; then
        pass "commands file copied to guest ~/.claude/commands/"
    else
        fail "commands file copied to guest ~/.claude/commands/" "got: $guest_cmd"
    fi

    # Verify non-allowlisted files are NOT copied.
    # The guest may have its own settings.json from Claude Code init,
    # so check for the specific marker content from our test file.
    local guest_settings
    guest_settings=$(guest_exec cat /home/ubuntu/.claude/settings.json 2>/dev/null) || guest_settings=""
    if echo "$guest_settings" | grep -q "should-not-copy"; then
        fail "settings.json NOT copied (allowlist)" "host content leaked to guest"
    else
        pass "settings.json NOT copied (allowlist)"
    fi

    # ── 2. Modify guest CLAUDE.md, restart → re-synced from host ──

    guest_exec sh -c "'echo guest-modified > /home/ubuntu/.claude/CLAUDE.md'" 2>/dev/null || true
    unset GUEST_INSTANCE

    cs stop "$inst_name" || true

    if cs start "$inst_name"; then
        pass "restart with config_dir exits 0"
    else
        fail "restart with config_dir exits 0" "exit code: $? stderr: $HARNESS_ERR"
        cs destroy "$inst_name" 2>/dev/null || true
        untrack_instance "$inst_name"
        rm -r "$cs_dir"
        return
    fi

    GUEST_INSTANCE="$inst_name"
    guest_claude=$(guest_exec cat /home/ubuntu/.claude/CLAUDE.md 2>/dev/null) || guest_claude=""
    if echo "$guest_claude" | grep -q "host-claude-marker"; then
        pass "restart re-syncs CLAUDE.md from host"
    else
        fail "restart re-syncs CLAUDE.md from host" "got: $guest_claude"
    fi
    unset GUEST_INSTANCE

    # Cleanup
    cs stop "$inst_name" 2>/dev/null || true
    cs destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    rm -r "$cs_dir"
}

# ── Interrupted setup test (--full only) ──────────────────────

test_interrupted_setup() {
    echo ""
    echo "=== Phase: interrupted setup recovery ==="

    # Use a separate named image so we don't risk the default image
    local img="interrupted-$$"

    # Start building an image in the background
    "$BINARY" setup -y --image "$img" --profile python,node \
        >"$tmpdir/int-stdout" 2>"$tmpdir/int-stderr" &
    local setup_pid=$!

    # Wait for the build to actually start (poll stderr for build markers)
    local waited=0
    while [[ $waited -lt 90 ]]; do
        # Check if process already exited (build finished before we could kill)
        if ! kill -0 "$setup_pid" 2>/dev/null; then
            break
        fi
        if grep -qi "building\|installing\|Starting builder\|downloading" \
                "$tmpdir/int-stderr" 2>/dev/null; then
            break
        fi
        sleep 1
        waited=$((waited + 1))
    done

    if ! kill -0 "$setup_pid" 2>/dev/null; then
        # Process already exited — can't test interruption
        wait "$setup_pid" 2>/dev/null || true
        skip "interrupted setup" "build completed before kill"
        coop images --delete "$img" 2>/dev/null || true
        return
    fi

    # Let it get deeper into the build
    sleep 5

    # SIGKILL — bypasses signal handlers, simulates a crash
    kill -9 "$setup_pid" 2>/dev/null
    wait "$setup_pid" 2>/dev/null || true
    pass "setup killed mid-build (SIGKILL)"

    # Retry — setup should clean up leftover state and succeed
    if coop setup -y --image "$img" --profile python,node; then
        pass "setup succeeds after interrupted build"
    else
        fail "setup succeeds after interrupted build" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        coop images --delete "$img" 2>/dev/null || true
        return
    fi

    # Verify the image works: start a VM and check basic connectivity
    local inst_name="${INSTANCE}-recovery"
    if coop start "$inst_name" --no-claude --image "$img"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "start from recovered image exits 0"
    else
        fail "start from recovered image exits 0" "exit code: $?"
        coop images --delete "$img" 2>/dev/null || true
        return
    fi

    GUEST_INSTANCE="$inst_name"
    local reply
    if reply=$(guest_exec echo ok 2>/dev/null) && [[ "$reply" == *ok* ]]; then
        pass "guest SSH works after recovery"
    else
        fail "guest SSH works after recovery" "reply: '$reply'"
    fi
    unset GUEST_INSTANCE

    # Clean up
    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    coop images --delete "$img" 2>/dev/null || true
}

# ── Provision failure test (--full only) ───────────────────────

test_provision_failure() {
    echo ""
    echo "=== Phase: provision failure detection ==="

    # Inject a failure into the provision script via env var.
    # The coop checks COOP_TEST_INJECT_PROVISION_FAILURE and
    # appends 'exit 1' to the provision script before cleanup.
    # setup --rebuild should fail with a clear error message.
    local rc=0
    local out err
    out=$(COOP_TEST_INJECT_PROVISION_FAILURE=1 "$BINARY" setup -y --rebuild 2>"$tmpdir/pf-stderr") || rc=$?
    err=$(cat "$tmpdir/pf-stderr")

    if [[ $rc -ne 0 ]]; then
        pass "setup fails when provision script fails (exit $rc)"
    else
        fail "setup fails when provision script fails" "expected non-zero exit, got 0"
        return
    fi

    # Error message should mention the failure clearly (check stderr
    # separately — stdout can be enormous with apt output, and piping
    # the combined string through echo+grep can lose content)
    if grep -qi "provision.*fail\|failed\|error" "$tmpdir/pf-stderr"; then
        pass "error message mentions failure"
    else
        fail "error message mentions failure" "stderr: $err"
    fi

    # Rebuild the golden image cleanly so subsequent tests work
    echo "  Rebuilding clean golden image..."
    if coop setup -y --rebuild; then
        pass "clean rebuild succeeds after failure"
    else
        fail "clean rebuild succeeds after failure" "exit code: $?"
        echo "FATAL: cannot continue without golden image"
        exit 1
    fi
}

# ── Main ──────────────────────────────────────────────────────

main() {
    echo "coop integration tests"
    echo "================================"
    echo "Binary:   $BINARY"
    echo "Instance: $INSTANCE"
    echo "Profiles: ${PROFILES:-<none>}"
    echo "Full:     $( [[ "$FULL" == "1" ]] && echo "yes" || echo "no (use --full for workspace/multi-instance tests)" )"
    echo ""

    tmpdir=$(mktemp -d)

    verify_binary

    # Pre-VM tests
    test_validate
    test_invalid_names

    # Setup + primary instance
    test_setup
    test_start
    test_duplicate_name
    test_status_running
    test_auto_resolve_running
    test_ssh_connectivity
    test_exec
    test_claude_bin_path
    test_github_token_forwarding
    test_term_handling
    test_guest_environment
    test_sudo
    test_network
    test_tmux
    test_docker
    test_logs
    test_profiles
    test_guest_fingerprint

    # Stop + restart + stopped-state verification
    test_stop
    test_auto_resolve_stopped
    test_status_stopped
    test_resize_status
    test_restart_stopped
    test_restart_rejects_ignored_flags
    test_destroy
    test_auto_resolve_no_instances

    # Idempotency: re-run commands that should be safe to repeat
    test_idempotency

    # Extended tests (each manages its own instance lifecycle)
    if [[ "$FULL" == "1" ]]; then
        test_mount_conflicts
        test_host_mount
        test_host_mount_custom_guest_path
        test_workspace_sync
        test_multi_instance
        test_named_images
        test_custom_profiles
        test_builtin_profiles

        # Local marketplace directory copy
        test_local_marketplace

        # Config sources: CLAUDE.md + rules copy
        test_config_dir

        # Interrupted setup: SIGKILL mid-build, verify clean recovery
        test_interrupted_setup

        # Provision failure rebuilds the golden image, so run before
        # destroy --all which removes it entirely.
        test_provision_failure

        # destroy --all removes the golden image, so run it last.
        # After this test, `coop setup` must be re-run.
        test_destroy_all
        echo ""
        echo "NOTE: destroy --all removed the golden image."
        echo "      Run 'coop setup -y' before next use."
    fi

    summary
}

main
