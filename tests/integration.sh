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
#   TEST_BINARY            — Path to pre-built binary
#   TEST_PROFILES          — Comma-separated profiles to install
#   TEST_INSTANCE          — Instance name prefix
#   TEST_FULL              — Set to 1 for extended tests
#   COOP_TEST_DESTRUCTIVE  — Set to 1 to run the `coop destroy --all` phase.
#                            Off by default because it wipes every coop-managed
#                            instance on the host, including ones not owned by
#                            this test run. Enable only on clean/CI hosts.

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

# Detach from the controlling terminal's stdin. coop gates interactive prompts
# (e.g. the discovered-devcontainer confirmation) on stdin being a TTY, falling
# back to a non-TTY error that the suite asserts on. Under CI stdin is already
# not a TTY, but when the suite runs from an interactive shell (the release
# preflight) those prompts would read real keystrokes and block. Redirecting the
# whole script makes every coop subprocess see a non-TTY stdin regardless of how
# the suite is invoked. The script itself never reads stdin.
exec </dev/null

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
coop_fails() {
    if coop "$@"; then
        return 1
    else
        return 0
    fi
}

# Run a command in the guest VM via `coop shell -- <cmd>`.
# RUST_LOG=off suppresses tracing output that would mix with command output.
# Stderr is captured in $tmpdir/guest_stderr for diagnostics on failure.
guest_exec() {
    local inst="${GUEST_INSTANCE:-$INSTANCE}"
    RUST_LOG=off "$BINARY" shell "$inst" -- "$@" 2>"$tmpdir/guest_stderr"
}

# Run the exec subcommand (captures stdout, propagates exit code).
coop_exec() {
    local inst="${GUEST_INSTANCE:-$INSTANCE}"
    RUST_LOG=off "$BINARY" exec "$inst" -- "$@" 2>"$tmpdir/guest_stderr"
}

# Return captured stderr from the last guest_exec/coop_exec call.
guest_stderr() {
    cat "$tmpdir/guest_stderr" 2>/dev/null
}

# Cross-platform timeout wrapper. Prefer GNU timeout (Linux, or macOS with
# coreutils installed as `gtimeout`); fall back to a perl-based implementation
# since perl ships in the macOS base system.
if command -v timeout >/dev/null 2>&1; then
    _timeout() { timeout "$@"; }
elif command -v gtimeout >/dev/null 2>&1; then
    _timeout() { gtimeout "$@"; }
else
    _timeout() {
        perl -e '
            my $d = shift;
            my $pid = fork() // die "fork: $!";
            if ($pid == 0) { exec @ARGV; die "exec: $!"; }
            local $SIG{ALRM} = sub {
                kill TERM => $pid;
                sleep 1;
                kill KILL => $pid;
                waitpid $pid, 0;
                exit 124;
            };
            alarm $d;
            waitpid $pid, 0;
            alarm 0;
            exit($? & 127 ? 128 + ($? & 127) : $? >> 8);
        ' "$@"
    }
fi

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

    # `[codex] auth = "chatgpt"` uses ChatGPT workspace credentials while the
    # OpenAI proxy injects an API key, so the pair is rejected at parse time.
    local conflict_dir conflict_cfg
    conflict_dir=$(mktemp -d)
    conflict_cfg="$conflict_dir/config.toml"
    cat >"$conflict_cfg" <<'CONFLICTEOF'
[codex]
auth = "chatgpt"

[proxy.openai]
credential = "cmd:echo sk-test"
auth = "bearer"
CONFLICTEOF

    if coop_fails --config "$conflict_cfg" validate; then
        if grep -q "conflicts with \[proxy.openai\]" <<<"$HARNESS_ERR$HARNESS_OUT"; then
            pass "validate rejects codex.auth=chatgpt with [proxy.openai]"
        else
            fail "validate rejects codex.auth=chatgpt with [proxy.openai]" \
                "wrong error: $HARNESS_ERR$HARNESS_OUT"
        fi
    else
        fail "validate rejects codex.auth=chatgpt with [proxy.openai]" \
            "validate unexpectedly succeeded"
    fi

    rm -rf "$conflict_dir"
}

test_completions() {
    echo ""
    echo "=== Phase: shell completions ==="

    # Static script generation — must succeed for each supported shell and
    # the bash script must reference our subcommands.
    for shell in bash zsh fish powershell elvish; do
        if coop completions "$shell" >/dev/null; then
            pass "completions $shell exits 0"
        else
            fail "completions $shell exits 0" "exit code: $?"
        fi
    done

    if coop completions bash; then
        for sub in shell claude destroy completions; do
            # Here-string, not `echo | grep -q`: pipefail + early grep match
            # SIGPIPE's bash's echo on this 48 KB script and turns matches into false misses.
            if grep -q "coop,$sub" <<<"$HARNESS_OUT"; then
                pass "bash completion script references \`$sub\`"
            else
                fail "bash completion script references \`$sub\`" \
                    "output (truncated): $(head -c 400 <<<"$HARNESS_OUT")"
            fi
        done
    else
        fail "bash completion script content check" \
             "completions bash exited non-zero: $?, stderr: $HARNESS_ERR"
    fi

    # Dynamic completion: the engine must return a usable subcommand list
    # when called via the CompleteEnv protocol.
    local dyn_out
    if dyn_out=$(_CLAP_COMPLETE_INDEX=1 _CLAP_IFS=$'\013' \
                 COMPLETE=bash "$BINARY" -- coop "" 2>&1); then
        for sub in shell claude destroy completions; do
            if echo "$dyn_out" | tr $'\013' '\n' | grep -qx "$sub"; then
                pass "dynamic completion offers \`$sub\`"
            else
                fail "dynamic completion offers \`$sub\`" \
                    "got: $(echo "$dyn_out" | tr $'\013' '\n')"
            fi
        done
    else
        fail "dynamic completion request exits 0" "exit code: $?, output: $dyn_out"
    fi
}

test_invalid_names() {
    echo ""
    echo "=== Phase: invalid instance names ==="

    # Path traversal
    if coop_fails start "../../../tmp/evil" --no-agents; then
        pass "rejects path traversal name"
    else
        fail "rejects path traversal name" "should have failed"
    fi

    # Newline injection
    if coop_fails start $'evil\nname' --no-agents; then
        pass "rejects newline in name"
    else
        fail "rejects newline in name" "should have failed"
    fi

    # Empty name is fine (auto-generated), but spaces are not
    if coop_fails start "name with spaces" --no-agents; then
        pass "rejects spaces in name"
    else
        fail "rejects spaces in name" "should have failed"
    fi
}

# ── Guest-memory floor (issue #404) ───────────────────────────
#
# A below-floor `--mem` must be rejected at the CLI boundary
# (`VmMemory::parse_cli`), before any VM is created — so these are fast
# negative checks that never boot. This is the headline #404 regression:
# `coop up --mem 16` previously created an unbootable VM instead of failing.
test_mem_floor() {
    echo ""
    echo "=== Phase: guest-memory floor rejection ==="

    if coop_fails up --mem 16 --no-agents "$tmpdir"; then
        pass "up rejects --mem below the 128 MiB floor"
    else
        fail "up rejects --mem below the 128 MiB floor" "should have failed fast"
    fi

    if coop_fails setup --mem 16 --yes; then
        pass "setup rejects --mem below the 128 MiB floor"
    else
        fail "setup rejects --mem below the 128 MiB floor" "should have failed fast"
    fi

    # config.toml vector: a below-floor mem_size_mib is rejected at load.
    local bad_cfg="$tmpdir/mem-floor-config.toml"
    printf '[vm]\nmem_size_mib = 16\n' >"$bad_cfg"
    if "$BINARY" --config "$bad_cfg" validate 2>"$tmpdir/mem_floor_cfg_stderr"; then
        fail "config.toml rejects mem_size_mib below the floor" "load should have failed"
    elif grep -q "is too low" "$tmpdir/mem_floor_cfg_stderr"; then
        pass "config.toml rejects mem_size_mib below the floor"
    else
        fail "config.toml rejects mem_size_mib below the floor" \
            "failed for the wrong reason: $(cat "$tmpdir/mem_floor_cfg_stderr")"
    fi

    # The floor is a floor, not a ban: the boundary value is accepted by the
    # parser (this still fails later for other reasons, so only assert the
    # error is not the floor message).
    if coop up --mem 128 --dry-run "$tmpdir" 2>"$tmpdir/mem_floor_stderr"; then
        pass "up accepts --mem at the 128 MiB boundary (dry-run)"
    elif ! grep -q "is too low" "$tmpdir/mem_floor_stderr"; then
        pass "up accepts --mem at the 128 MiB boundary (no floor error)"
    else
        fail "up accepts --mem at the 128 MiB boundary" \
            "128 MiB was rejected as too low"
    fi
}

# ── Profiles list/show ────────────────────────────────────────

test_profiles_cli() {
    echo ""
    echo "=== Phase: profiles list/show ==="

    # `profiles list` exits 0 and includes builtin names
    if coop profiles list; then
        pass "profiles list exits 0"
    else
        fail "profiles list exits 0" "exit code: $?"
    fi

    for name in python node c fuzz rust go; do
        if echo "$HARNESS_OUT" | grep -q "$name"; then
            pass "profiles list includes $name"
        else
            fail "profiles list includes $name" "output: $HARNESS_OUT"
        fi
    done

    # Bare `profiles` defaults to list
    if coop profiles; then
        pass "bare profiles exits 0"
    else
        fail "bare profiles exits 0" "exit code: $?"
    fi

    if echo "$HARNESS_OUT" | grep -q "Builtin:"; then
        pass "bare profiles shows Builtin header"
    else
        fail "bare profiles shows Builtin header" "output: $HARNESS_OUT"
    fi

    # `profiles show <name>` exits 0 and prints profile details
    if coop profiles show rust; then
        pass "profiles show rust exits 0"
    else
        fail "profiles show rust exits 0" "exit code: $?"
    fi

    if echo "$HARNESS_OUT" | grep -q "Profile: rust (builtin)"; then
        pass "profiles show rust reports builtin origin"
    else
        fail "profiles show rust reports builtin origin" "output: $HARNESS_OUT"
    fi

    for field in apt_packages pre_install post_install marketplaces plugins; do
        if echo "$HARNESS_OUT" | grep -q "$field"; then
            pass "profiles show rust includes $field"
        else
            fail "profiles show rust includes $field" "output: $HARNESS_OUT"
        fi
    done

    # `profiles show <unknown>` fails
    if coop_fails profiles show nonexistent-profile; then
        pass "profiles show rejects unknown profile"
    else
        fail "profiles show rejects unknown profile" "should have failed"
    fi
}

# ── Setup ─────────────────────────────────────────────────────

test_setup() {
    echo ""
    echo "=== Phase: setup ==="

    local args=(setup -y)
    # The full suite exercises `agent update --codex`; force the initial image
    # build so that test cannot silently reuse a pre-fix local template.
    if [[ "$FULL" == "1" ]]; then
        args+=(--rebuild)
    fi
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

test_up_creates_primary_instance() {
    echo ""
    echo "=== Phase: up creates primary instance ==="

    # `--env` exercises the guest_env CLI -> config -> SendEnv path
    # end-to-end. `test_guest_environment` verifies the value is
    # visible inside the guest via `printenv`.
    mkdir -p "$tmpdir/primary-ws"
    local args=(
        up "$tmpdir/primary-ws" --name "$INSTANCE" --no-agents --no-devcontainer
        --env "COOP_TEST_GUEST_ENV=hello-from-cli"
    )
    if coop "${args[@]}"; then
        STARTED_INSTANCES+=("$INSTANCE")
        pass "up creates primary instance"
    else
        fail "up creates primary instance" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        echo "FATAL: primary instance creation failed, cannot continue"
        exit 1
    fi
}

test_start_rejects_missing_instance() {
    echo ""
    echo "=== Phase: start rejects missing instance ==="

    local missing="${INSTANCE}-missing"
    if coop_fails start "$missing" --no-agents; then
        pass "start rejects missing instance"
    else
        fail "start rejects missing instance" "should have failed"
        coop destroy "$missing" 2>/dev/null || true
    fi
}

test_duplicate_name() {
    echo ""
    echo "=== Phase: duplicate instance name ==="

    local other_ws="$tmpdir/duplicate-ws"
    mkdir -p "$other_ws"
    if coop_fails up "$other_ws" --name "$INSTANCE" --no-agents --no-devcontainer; then
        pass "rejects duplicate instance name"
    else
        fail "rejects duplicate instance name" "should have failed"
        # Clean up the accidental second instance
        coop destroy "$INSTANCE" 2>/dev/null || true
    fi

    if coop_fails up "$tmpdir/primary-ws" --name "${INSTANCE}-other" --no-agents --no-devcontainer; then
        pass "rejects mismatched name for existing project"
    else
        fail "rejects mismatched name for existing project" "should have failed"
        coop destroy "${INSTANCE}-other" 2>/dev/null || true
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

    # The PID must be the VMM, not the `sudo` wrapper: a wrapper PID makes
    # stop()'s SIGKILL fallback orphan the VM. Firecracker only.
    if [[ "$(uname -s)" != "Darwin" ]]; then
        local fc_pid comm=""
        fc_pid=$(echo "$HARNESS_OUT" | sed -n 's/.*PID: \([0-9][0-9]*\).*/\1/p' | head -1)
        if [[ -n "$fc_pid" ]]; then
            comm=$(cat "/proc/$fc_pid/comm" 2>/dev/null || true)
        fi
        if [[ "$comm" == *firecracker* ]]; then
            pass "status PID is the firecracker process (pid $fc_pid)"
        else
            fail "status PID is the firecracker process" \
                "pid=${fc_pid:-<none>} comm=${comm:-<unreadable>} from: $HARNESS_OUT"
        fi
    else
        skip "status PID is the firecracker process" "Firecracker-only"
    fi

    # JSON output: HARNESS_OUT captures stdout only (tracing stays on stderr),
    # so `coop status --json` must be clean, parseable JSON. This exercises the
    # real stdout/stderr split on both backends.
    if coop status "$INSTANCE" --json; then
        if command -v jq >/dev/null 2>&1; then
            if echo "$HARNESS_OUT" | jq -e '.state == "running"' >/dev/null; then
                pass "status --json reports running state"
            else
                fail "status --json reports running state" "got: $HARNESS_OUT"
            fi
            if echo "$HARNESS_OUT" | jq -e '.usage.mem_total_mib > 0' >/dev/null; then
                pass "status --json carries usage"
            else
                fail "status --json carries usage" "got: $HARNESS_OUT"
            fi
        elif echo "$HARNESS_OUT" | grep -q '"state": "running"'; then
            pass "status --json reports running state (no jq)"
        else
            fail "status --json reports running state (no jq)" "got: $HARNESS_OUT"
        fi
    else
        fail "status --json exits 0" "exit code: $?"
    fi

    # Bare `--json` (no name) emits a JSON array of the common shape.
    if coop status --json && command -v jq >/dev/null 2>&1; then
        if echo "$HARNESS_OUT" | jq -e 'type == "array" and (.[0].state | test("running|stopped"))' >/dev/null; then
            pass "status --json (all) emits an array"
        else
            fail "status --json (all) emits an array" "got: $HARNESS_OUT"
        fi
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

test_list_running() {
    echo ""
    echo "=== Phase: list (running) ==="

    if coop list; then
        pass "list exits 0"
    else
        fail "list exits 0" "exit code: $?"
    fi

    if echo "$HARNESS_OUT" | grep -q "^NAME *STATE"; then
        pass "list prints NAME/STATE header"
    else
        fail "list prints NAME/STATE header" "got: $HARNESS_OUT"
    fi

    if echo "$HARNESS_OUT" | grep -qE "^${INSTANCE} +running\$"; then
        pass "list shows instance as running"
    else
        fail "list shows instance as running" "got: $HARNESS_OUT"
    fi

    # `ls` alias resolves to the same command.
    if coop ls; then
        if echo "$HARNESS_OUT" | grep -qE "^${INSTANCE} +running\$"; then
            pass "ls alias prints same output"
        else
            fail "ls alias prints same output" "got: $HARNESS_OUT"
        fi
    else
        fail "ls alias exits 0" "exit code: $?"
    fi

    # JSON output: parseable array with the running instance.
    if coop list --json; then
        if command -v jq >/dev/null 2>&1; then
            if echo "$HARNESS_OUT" | jq -e --arg n "$INSTANCE" \
                'any(.[]; .name == $n and .state == "running")' >/dev/null; then
                pass "list --json shows instance running"
            else
                fail "list --json shows instance running" "got: $HARNESS_OUT"
            fi
        elif echo "$HARNESS_OUT" | grep -q '"state": "running"'; then
            pass "list --json shows running (no jq)"
        else
            fail "list --json shows running (no jq)" "got: $HARNESS_OUT"
        fi
    else
        fail "list --json exits 0" "exit code: $?"
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
        skip "shell auto-resolves single running instance" "$running_count running (need exactly 1)"
        skip "exec auto-resolves single running instance" "skipped (need exactly 1 running)"
        return
    fi

    # shell without name should auto-select the single running instance
    local output
    if output=$(RUST_LOG=off "$BINARY" shell -- echo "auto-resolve-works" 2>/dev/null); then
        pass "shell auto-resolves single running instance"
        if echo "$output" | grep -q "auto-resolve-works"; then
            pass "shell auto-resolve returns correct output"
        else
            fail "shell auto-resolve returns correct output" "got: $output"
        fi
    else
        fail "shell auto-resolves single running instance" "exit code: $?"
    fi

    # exec without an instance name should also auto-select
    if output=$(RUST_LOG=off "$BINARY" exec -- echo "exec-auto" 2>/dev/null); then
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

test_shell_connectivity() {
    echo ""
    echo "=== Phase: shell connectivity ==="

    local output
    if output=$(guest_exec echo "hello-from-guest"); then
        pass "shell connects to guest"
    else
        fail "shell connects to guest" "stderr: $(guest_stderr)"
        return
    fi

    if echo "$output" | grep -q "hello-from-guest"; then
        pass "shell command output correct"
    else
        fail "shell command output correct" "got: $output"
    fi
}

test_ssh_alias() {
    echo ""
    echo "=== Phase: ssh alias (backward compat) ==="

    local output
    if output=$(RUST_LOG=off "$BINARY" ssh "$INSTANCE" -- echo "alias-ok" 2>/dev/null); then
        if echo "$output" | grep -q "alias-ok"; then
            pass "ssh alias works as shell"
        else
            fail "ssh alias works as shell" "got: $output"
        fi
    else
        fail "ssh alias works as shell" "exit code: $?"
    fi
}

# Exercises `coop ssh-config`: it installs a `coop-<name>` alias into
# ~/.ssh/config, the alias works with plain ssh, the block survives `stop`
# and is refreshed on `start` (the Lima port changes per boot), and
# `--clean` removes it. Self-contained: leaves the instance running.
test_ssh_config() {
    echo ""
    echo "=== Phase: ssh-config alias ==="

    local marker="# coop START coop-$INSTANCE"
    local ssh_cfg="$HOME/.ssh/config"

    if coop ssh-config "$INSTANCE"; then
        pass "ssh-config exits 0"
    else
        fail "ssh-config exits 0" "stderr: $HARNESS_ERR"
        return
    fi

    if grep -qF "$marker" "$ssh_cfg" 2>/dev/null; then
        pass "ssh-config writes marker block"
    else
        fail "ssh-config writes marker block" "no marker in $ssh_cfg"
    fi

    if _timeout 30 ssh "coop-$INSTANCE" echo "ssh-config-ok" 2>/dev/null | grep -q "ssh-config-ok"; then
        pass "plain ssh via coop-$INSTANCE alias connects"
    else
        fail "plain ssh via coop-$INSTANCE alias connects" "ssh through alias failed"
    fi

    coop stop "$INSTANCE" || true
    if grep -qF "$marker" "$ssh_cfg" 2>/dev/null; then
        pass "ssh-config block survives stop"
    else
        fail "ssh-config block survives stop" "marker removed by stop"
    fi

    if coop start "$INSTANCE" --no-agents; then
        if _timeout 30 ssh "coop-$INSTANCE" echo "refreshed-ok" 2>/dev/null | grep -q "refreshed-ok"; then
            pass "ssh alias works after restart (block refreshed)"
        else
            fail "ssh alias works after restart (block refreshed)" "alias stale after restart"
        fi
    else
        fail "restart for ssh-config refresh exits 0" "stderr: $HARNESS_ERR"
    fi

    if coop ssh-config "$INSTANCE" --clean; then
        if grep -qF "$marker" "$ssh_cfg" 2>/dev/null; then
            fail "ssh-config --clean removes block" "marker still present"
        else
            pass "ssh-config --clean removes block"
        fi
    else
        fail "ssh-config --clean exits 0" "stderr: $HARNESS_ERR"
    fi
}

# Exercises the host-side half of `coop editor` against a live VM. Stub editor
# CLIs make the phase hermetic (no GUI is opened) while still pinning the argv
# contract, Zed URL encoding, the legacy `vscode` alias, and SSH-config setup.
test_editor() {
    echo ""
    echo "=== Phase: editor launch ==="

    local editor_dir="$tmpdir/editor-bin"
    local editor_log="$tmpdir/editor-argv"
    local marker="# coop START coop-$INSTANCE"
    local ssh_cfg="$HOME/.ssh/config"
    mkdir -p "$editor_dir"

    cat >"$editor_dir/zed" <<'STUB'
#!/bin/sh
printf '%s\n' "$@" > "$COOP_TEST_EDITOR_LOG"
STUB
    cat >"$editor_dir/code" <<'STUB'
#!/bin/sh
printf '%s\n' "$@" > "$COOP_TEST_EDITOR_LOG"
STUB
    chmod +x "$editor_dir/zed" "$editor_dir/code"

    local zed_project='/workspace/editor path#part%done?'
    if COOP_TEST_EDITOR_LOG="$editor_log" PATH="$editor_dir:$PATH" \
        coop editor "$INSTANCE" --editor zed --project "$zed_project"; then
        pass "editor --editor zed exits 0"
    else
        fail "editor --editor zed exits 0" "stderr: $HARNESS_ERR"
        coop editor "$INSTANCE" --clean >/dev/null 2>&1 || true
        return
    fi

    local expected_zed="ssh://coop-$INSTANCE/workspace/editor%20path%23part%25done%3F"
    if [[ "$(cat "$editor_log" 2>/dev/null)" == "$expected_zed" ]]; then
        pass "editor passes Zed an encoded SSH URL"
    else
        fail "editor passes Zed an encoded SSH URL" \
            "expected=$expected_zed got=$(cat "$editor_log" 2>/dev/null)"
    fi

    if grep -qF "$marker" "$ssh_cfg" 2>/dev/null; then
        pass "editor installs the shared SSH alias"
    else
        fail "editor installs the shared SSH alias" "no marker in $ssh_cfg"
    fi

    : >"$editor_log"
    if COOP_TEST_EDITOR_LOG="$editor_log" PATH="$editor_dir:$PATH" \
        coop editor "$INSTANCE" --project /workspace/auto; then
        pass "editor auto-detection exits 0"
    else
        fail "editor auto-detection exits 0" "stderr: $HARNESS_ERR"
    fi

    local editor_argv
    editor_argv=$(cat "$editor_log" 2>/dev/null)
    if [[ "$editor_argv" == $'--remote\nssh-remote+coop-'"$INSTANCE"$'\n/workspace/auto' ]]; then
        pass "editor auto-detection tries VS Code first"
    else
        fail "editor auto-detection tries VS Code first" "got: $editor_argv"
    fi

    # `vscode` remains a public compatibility alias for `editor`. Pin the real
    # dispatch path, not only clap parsing, and check the VS Code Remote-SSH
    # argv shape expected by the extension.
    : >"$editor_log"
    if COOP_TEST_EDITOR_LOG="$editor_log" PATH="$editor_dir:$PATH" \
        coop vscode "$INSTANCE" --editor code --project /workspace/legacy; then
        pass "vscode compatibility alias exits 0"
    else
        fail "vscode compatibility alias exits 0" "stderr: $HARNESS_ERR"
    fi

    editor_argv=$(cat "$editor_log" 2>/dev/null)
    if [[ "$editor_argv" == $'--remote\nssh-remote+coop-'"$INSTANCE"$'\n/workspace/legacy' ]]; then
        pass "vscode alias passes the VS Code Remote-SSH arguments"
    else
        fail "vscode alias passes the VS Code Remote-SSH arguments" "got: $editor_argv"
    fi

    # Leave the SSH config in the same clean state this phase inherited from
    # test_ssh_config, so later restart tests do not accidentally cover alias
    # refresh merely because this test ran.
    if coop editor "$INSTANCE" --clean; then
        if grep -qF "$marker" "$ssh_cfg" 2>/dev/null; then
            fail "editor --clean removes its SSH block" "marker still present"
        else
            pass "editor --clean removes its SSH block"
        fi
    else
        fail "editor --clean exits 0" "stderr: $HARNESS_ERR"
    fi
}

test_exec() {
    echo ""
    echo "=== Phase: exec ==="

    local output
    if output=$(coop_exec echo "exec-works"); then
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
    coop_exec false || rc=$?
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
    # Verify the full path is reachable via coop exec.
    if guest_exec test -x /home/ubuntu/.local/bin/claude; then
        pass "claude binary exists at CLAUDE_BIN path"
    else
        # Image was built with --no-agents or without profiles — skip
        skip "claude binary at CLAUDE_BIN path" "not installed in this image"
        return
    fi

    # Verify coop exec can invoke it by full path
    if coop_exec /home/ubuntu/.local/bin/claude --version >/dev/null; then
        pass "claude binary invocable via full path"
    else
        # --version may fail without auth, but any output means the binary ran
        skip "claude --version" "binary exists but --version returned non-zero (may need auth)"
    fi

    # Verify /usr/local/bin/claude symlink exists and points to the real binary
    local link_target
    if link_target=$(guest_exec readlink /usr/local/bin/claude); then
        if [[ "$link_target" == "/home/ubuntu/.local/bin/claude" ]]; then
            pass "claude symlink in /usr/local/bin"
        else
            fail "claude symlink in /usr/local/bin" "points to: $link_target"
        fi
    else
        fail "claude symlink in /usr/local/bin" "not found"
    fi

    # Verify ~/.local/bin is on PATH in a non-interactive SSH session — the
    # exact case `coop claude` and claude's Bash-tool subshells hit (issue
    # #248). `guest_exec` runs `coop shell -- cmd`, which sshs a bare remote
    # command (no login shell, no .profile), so this pins the fix in
    # /etc/environment rather than the old .profile/.bashrc appends.
    local guest_path
    if guest_path=$(guest_exec printenv PATH); then
        if [[ ":$guest_path:" == *":/home/ubuntu/.local/bin:"* ]]; then
            pass "~/.local/bin on PATH in non-interactive session"
        else
            fail "~/.local/bin on PATH in non-interactive session" "PATH=$guest_path"
        fi
    else
        fail "~/.local/bin on PATH in non-interactive session" "printenv PATH failed; stderr: $(guest_stderr)"
    fi

    # Verify claude-yolo shortcut exists and is executable
    if guest_exec test -x /usr/local/bin/claude-yolo; then
        pass "claude-yolo shortcut exists"
    else
        fail "claude-yolo shortcut exists" "stderr: $(guest_stderr)"
    fi

    # Verify claude-yolo passes --dangerously-skip-permissions
    local yolo_content
    if yolo_content=$(guest_exec cat /usr/local/bin/claude-yolo); then
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

test_claude_settings_merge() {
    echo ""
    echo "=== Phase: claude settings merge across restart ==="

    # Seed settings.json with a sentinel enabledPlugins key plus a non-managed
    # permissions value. The bug this guards: write_managed_claude_settings used
    # to overwrite the whole file on every boot, wiping the plugin/marketplace
    # state Claude Code keeps here (it only installs on first boot, so nothing
    # re-adds it on restart). The fix merges instead, so arbitrary keys must
    # survive a real stop/start while managed permissions are still reapplied.
    # ~ is inside the single-quoted sh -c so the guest expands it, not the host.
    local seed='mkdir -p ~/.claude && printf "%s" '
    seed+='"{\"enabledPlugins\":{\"sentinel@m\":true},\"permissions\":{\"defaultMode\":\"default\"}}" '
    seed+='> ~/.claude/settings.json'
    if coop_exec sh -c "$seed"; then
        pass "seed settings.json with sentinel plugin"
    else
        fail "seed settings.json with sentinel plugin" "stderr: $(guest_stderr)"
        return
    fi

    # No --no-agents: the merge runs inside bootstrap_agents, which --no-agents
    # skips entirely. A restart runs it in BootMode::Restart, which re-merges
    # settings but does not reinstall plugins/marketplaces (those are gated to
    # FirstBoot), so this stays cheap and offline while exercising the real
    # guest round-trip the bug is about.
    coop stop "$INSTANCE" || true
    if coop start "$INSTANCE"; then
        pass "restart for settings merge exits 0"
    else
        fail "restart for settings merge exits 0" "stderr: $HARNESS_ERR"
        return
    fi

    local merged
    if ! merged=$(coop_exec sh -c 'cat ~/.claude/settings.json'); then
        fail "read merged settings.json after restart" "stderr: $(guest_stderr)"
        return
    fi

    if echo "$merged" | grep -q 'sentinel@m'; then
        pass "enabledPlugins survives restart"
    else
        fail "enabledPlugins survives restart" "$merged"
    fi

    if echo "$merged" | grep -q 'bypassPermissions'; then
        pass "managed permissions reapplied after restart"
    else
        fail "managed permissions reapplied after restart" "$merged"
    fi
}

test_claude_onboarding_seed() {
    echo ""
    echo "=== Phase: claude onboarding seed (issue #365) ==="

    # Claude Code gates its onboarding wizard on hasCompletedOnboarding in
    # ~/.claude.json and ignores CLAUDE_CODE_OAUTH_TOKEN for that check, so coop
    # seeds the flag in seed_claude_onboarding (src/backend.rs). The seed is
    # gated on CLAUDE_CODE_OAUTH_TOKEN reaching the guest env and runs inside
    # bootstrap_agents on every boot (FirstBoot and Restart). This phase pins
    # the env gate and the restart path with the same real stop/start round-trip
    # the settings-merge phase above uses. No --no-agents: that skips
    # bootstrap_agents entirely, so the seed never runs.
    #
    # Order matters: --env persists into guest_env_state.json across restarts,
    # so the negative (no-token) case must run before the positive case adds the
    # token. The dummy token makes the throwaway `claude -p` call in
    # seed_claude_json fail/time out by design — the file is written from the
    # merge step regardless, so this phase asserts only on ~/.claude.json, never
    # on that command's exit status.

    # Negative env gate: restart without the token. The early return in
    # seed_claude_onboarding must make this a complete no-op — ~/.claude.json
    # must NOT be created, or credential-less guests would lose the interactive
    # login flow. Remove the file first: an earlier phase ran `claude --version`,
    # which can leave a ~/.claude.json behind, so the assertion must test "the
    # seed did not (re)create it" rather than "the file never existed."
    if ! coop_exec sh -c 'rm -f ~/.claude.json'; then
        fail "remove ~/.claude.json before negative case" "stderr: $(guest_stderr)"
        return
    fi

    coop stop "$INSTANCE" || true
    if coop start "$INSTANCE"; then
        pass "restart without token exits 0"
    else
        fail "restart without token exits 0" "stderr: $HARNESS_ERR"
        return
    fi

    if coop_exec sh -c 'test ! -e ~/.claude.json'; then
        pass "~/.claude.json absent without CLAUDE_CODE_OAUTH_TOKEN"
    else
        fail "~/.claude.json absent without CLAUDE_CODE_OAUTH_TOKEN" \
            "file was seeded despite no token; stderr: $(guest_stderr)"
    fi

    # Positive env gate on BootMode::Restart: stop/start with the token
    # forwarded. bootstrap_agents runs seed_claude_onboarding, which seeds
    # ~/.claude.json with hasCompletedOnboarding: true.
    coop stop "$INSTANCE" || true
    if coop start "$INSTANCE" --env "CLAUDE_CODE_OAUTH_TOKEN=dummy-token-issue-365"; then
        pass "restart with token exits 0"
    else
        fail "restart with token exits 0" "stderr: $HARNESS_ERR"
        return
    fi

    local seeded
    if ! seeded=$(coop_exec sh -c 'cat ~/.claude.json'); then
        fail "read ~/.claude.json after seeding" \
            "file missing after restart with token; stderr: $(guest_stderr)"
        return
    fi

    if echo "$seeded" | grep -q '"hasCompletedOnboarding":[[:space:]]*true'; then
        pass "~/.claude.json seeded with hasCompletedOnboarding: true"
    else
        fail "~/.claude.json seeded with hasCompletedOnboarding: true" "$seeded"
    fi
}

validate_codex_package() {
    local context="$1"

    if guest_exec test -x /usr/local/bin/codex; then
        pass "codex binary exists at /usr/local/bin/codex ($context)"
    else
        fail "codex binary exists at /usr/local/bin/codex ($context)" \
            "stderr: $(guest_stderr)"
        return
    fi

    if coop_exec /usr/local/bin/codex --version >/dev/null; then
        pass "codex binary invocable via full path ($context)"
    else
        fail "codex binary invocable via full path ($context)" \
            "stderr: $(guest_stderr)"
    fi

    if guest_exec test -x /usr/local/bin/codex-code-mode-host; then
        pass "codex code-mode host exists and is executable ($context)"
    else
        fail "codex code-mode host exists and is executable ($context)" \
            "stderr: $(guest_stderr)"
        return
    fi

    # `--help` exits before the host initializes its transport. Stdio with EOF
    # exercises the real entrypoint without starting a persistent service; the
    # timeout makes a regression fail instead of hanging the integration run.
    if coop_exec sh -c \
        'timeout 10 /usr/local/bin/codex-code-mode-host --listen stdio </dev/null >/dev/null'; then
        pass "codex code-mode host accepts stdio transport ($context)"
    else
        fail "codex code-mode host accepts stdio transport ($context)" \
            "stderr: $(guest_stderr)"
    fi

    local codex_path code_mode_path codex_release
    codex_path=$(guest_exec readlink -f /usr/local/bin/codex)
    code_mode_path=$(guest_exec readlink -f /usr/local/bin/codex-code-mode-host)
    if [[ "$codex_path" =~ ^(/usr/local/lib/codex/releases/[0-9a-f]{64})/bin/codex$ ]] \
        && [[ "$code_mode_path" == "${BASH_REMATCH[1]}/bin/codex-code-mode-host" ]]; then
        codex_release="${BASH_REMATCH[1]}"
        pass "codex and code-mode host come from the same package ($context)"
    else
        fail "codex and code-mode host come from the same package ($context)" \
            "codex=$codex_path host=$code_mode_path"
        return
    fi

    if guest_exec test -x "$codex_release/codex-path/rg" \
        -a -x "$codex_release/codex-resources/bwrap" \
        -a -x "$codex_release/codex-resources/zsh/bin/zsh" \
        -a -f "$codex_release/codex-package.json"; then
        pass "codex package runtime resources are installed ($context)"
    else
        fail "codex package runtime resources are installed ($context)" \
            "release=$codex_release stderr: $(guest_stderr)"
    fi

    if guest_exec test ! -w "$codex_release/bin/codex" \
        -a ! -w "$codex_release/bin/codex-code-mode-host"; then
        pass "codex package executables are not guest-writable ($context)"
    else
        fail "codex package executables are not guest-writable ($context)" \
            "release=$codex_release stderr: $(guest_stderr)"
    fi
}

test_codex_bin_path() {
    echo ""
    echo "=== Phase: codex binary path ==="

    validate_codex_package "after provisioning"

    if guest_exec test -x /usr/local/bin/codex-yolo; then
        pass "codex-yolo shortcut exists"
    else
        fail "codex-yolo shortcut exists" "stderr: $(guest_stderr)"
    fi

    local yolo_content
    if yolo_content=$(guest_exec cat /usr/local/bin/codex-yolo); then
        if echo "$yolo_content" | grep -q "dangerously-bypass-approvals-and-sandbox"; then
            pass "codex-yolo includes dangerous full-access flag"
        else
            fail "codex-yolo includes dangerous full-access flag" \
                "content: $yolo_content"
        fi
        if echo "$yolo_content" | grep -q "codex-account"; then
            pass "codex-yolo routes through the account wrapper"
        else
            fail "codex-yolo routes through the account wrapper" \
                "content: $yolo_content"
        fi
    else
        fail "codex-yolo includes dangerous full-access flag" "cat failed"
    fi
}

test_codex_account_auth_support() {
    echo ""
    echo "=== Phase: codex account-auth (Secret Service) support ==="

    if guest_exec test -x /usr/local/bin/codex-account; then
        pass "codex-account wrapper exists"
    else
        fail "codex-account wrapper exists" "stderr: $(guest_stderr)"
        return
    fi

    # The wrapper is written unconditionally by the provision script, so the
    # packages behind it are what actually need asserting.
    local tool
    for tool in dbus-run-session gnome-keyring-daemon secret-tool; do
        if guest_exec command -v "$tool"; then
            pass "guest Secret Service tool present: $tool"
        else
            fail "guest Secret Service tool present: $tool" "stderr: $(guest_stderr)"
        fi
    done

    # Give the probe an explicit managed config with no credential-store
    # setting. The wrapper checks $HOME/.codex/config.toml before consulting an
    # explicit $CODEX_HOME, so both locations must be isolated for this test.
    local account_probe_root="/tmp/coop-codex-account-probe"
    local account_probe_home="$account_probe_root/home"
    local account_probe_codex_home="$account_probe_root/codex-home"
    if ! guest_exec sh -c '
        set -eu
        rm -rf "$1"
        mkdir -p "$1/home/.codex" "$1/codex-home"
        : > "$1/home/.codex/config.toml"
        : > "$1/codex-home/config.toml"
    ' sh "$account_probe_root"; then
        fail "prepare codex-account probe config" "stderr: $(guest_stderr)"
        guest_exec rm -rf "$account_probe_root" || true
        return
    fi

    # Without keyring mode the wrapper must be a transparent passthrough: no
    # D-Bus session, keyring, or password prompt. A hang here would mean it
    # tried to unlock a keyring it should have skipped.
    local version
    if version=$(coop_exec env HOME="$account_probe_home" \
        CODEX_HOME="$account_probe_codex_home" \
        /usr/local/bin/codex-account --version); then
        pass "codex-account passes through to codex without keyring mode ($version)"
    else
        fail "codex-account passes through to codex without keyring mode" \
            "stderr: $(guest_stderr)"
    fi

    # Select keyring mode explicitly in the scratch CODEX_HOME while retaining
    # the isolated managed config. This is the only place the
    # `cli_auth_credentials_store` check, the D-Bus re-exec, the tool guards and
    # the TTY guard actually execute — the assertions above all run on the
    # passthrough branch.
    #
    # `coop exec` is not a TTY, so the wrapper must refuse rather than block on
    # a password prompt. A hang here is the failure this asserts against.
    if ! guest_exec sh -c 'printf "cli_auth_credentials_store = \"keyring\"\n" \
        > "$1/config.toml"' sh "$account_probe_codex_home"; then
        fail "prepare codex-account keyring probe config" \
            "stderr: $(guest_stderr)"
        guest_exec rm -rf "$account_probe_root" || true
        return
    fi

    # </dev/null so the TTY guard is deterministic: run interactively, stdin
    # could otherwise be a terminal and the wrapper would prompt and block.
    if coop_exec env HOME="$account_probe_home" \
        CODEX_HOME="$account_probe_codex_home" \
        /usr/local/bin/codex-account --version </dev/null; then
        fail "codex-account enters keyring mode from the guest config" \
            "expected a non-TTY refusal, but the wrapper passed through to codex"
    elif guest_stderr | grep -q "interactive TTY"; then
        pass "codex-account enters keyring mode and refuses a non-TTY session"
    else
        fail "codex-account enters keyring mode and refuses a non-TTY session" \
            "stderr: $(guest_stderr)"
    fi

    if ! guest_exec rm -rf "$account_probe_root"; then
        fail "clean up codex-account probe config" "stderr: $(guest_stderr)"
    fi
}

# The wrapper/package checks above use a scratch CODEX_HOME, but they do not
# prove that `[codex] auth = "chatgpt"` drives the real lifecycle wiring. This
# phase restarts the primary VM in account mode, checks the guest boundary, and
# restores the default configuration for all later phases.
test_codex_chatgpt_mode() {
    echo ""
    echo "=== Phase: configured Codex ChatGPT auth ==="

    local cfg_dir="$tmpdir/codex-chatgpt"
    local cfg_file="$cfg_dir/config.toml"
    local host_openai="sk-host-openai-chatgpt-sentinel-LEAK"
    local overlay_openai="sk-runtime-openai-chatgpt-sentinel-LEAK"
    mkdir -p "$cfg_dir"
    cat >"$cfg_file" <<'CFGEOF'
[claude]
github = "off"

[codex]
auth = "chatgpt"
CFGEOF

    chatgpt() {
        local rc=0
        HARNESS_OUT=$(OPENAI_API_KEY="$host_openai" \
            "$BINARY" --config "$cfg_file" "$@" 2>"$tmpdir/stderr") || rc=$?
        HARNESS_ERR=$(cat "$tmpdir/stderr")
        return $rc
    }
    chatgpt_exec() {
        RUST_LOG=off OPENAI_API_KEY="$host_openai" \
            "$BINARY" --config "$cfg_file" exec "$INSTANCE" -- "$@" \
            2>"$tmpdir/guest_stderr"
    }

    # Seed the credential file that account mode promises never to stage. A
    # passing removal assertion therefore proves active cleanup, even on a host
    # that has no ~/.codex/auth.json of its own.
    if ! guest_exec sh -c 'mkdir -p ~/.codex && printf stale > ~/.codex/auth.json'; then
        fail "seed stale Codex auth.json" "stderr: $(guest_stderr)"
        return
    fi

    coop stop "$INSTANCE" || true
    if chatgpt start "$INSTANCE" --env "OPENAI_API_KEY=$overlay_openai" \
        --env "COOP_TEST_CHATGPT_PASSTHROUGH=kept"; then
        pass "start with codex auth=chatgpt exits 0"
    else
        fail "start with codex auth=chatgpt exits 0" "stderr: $HARNESS_ERR"
        coop start "$INSTANCE" || true
        return
    fi

    local codex_cfg
    if codex_cfg=$(chatgpt_exec cat ./.codex/config.toml) \
        && echo "$codex_cfg" | grep -q 'cli_auth_credentials_store = "keyring"'; then
        pass "chatgpt mode selects the guest keyring credential store"
    else
        fail "chatgpt mode selects the guest keyring credential store" \
            "config=$codex_cfg stderr: $(guest_stderr)"
    fi

    # coop manages only ~/.codex. A session-level CODEX_HOME must not bypass the
    # managed setting and let Codex write a plaintext refresh token into the
    # alternate directory (which could be inside /workspace and copied back to
    # the host). Give the alternate config the same first-line keyring marker
    # so its contents cannot bypass the guard.
    local alternate_codex_home="/tmp/coop-codex-home-bypass-probe"
    chatgpt_exec sh -c 'mkdir -p "$1" && printf "%s\n" \
        "cli_auth_credentials_store = \"keyring\"" > "$1/config.toml"' \
        sh "$alternate_codex_home"
    if chatgpt_exec env CODEX_HOME="$alternate_codex_home" \
        /usr/local/bin/codex-account --version; then
        fail "chatgpt mode rejects an unmanaged CODEX_HOME" \
            "wrapper honored CODEX_HOME instead of coop's managed config"
    elif guest_stderr | grep -q "unset CODEX_HOME"; then
        pass "chatgpt mode rejects an unmanaged CODEX_HOME"
    else
        fail "chatgpt mode rejects an unmanaged CODEX_HOME" \
            "stderr: $(guest_stderr)"
    fi
    chatgpt_exec rm -rf "$alternate_codex_home"

    # Positive witness for the supported path: the CODEX_HOME guard must not
    # reject a normal ChatGPT launch. This non-TTY call should get past that
    # guard and reach the existing keyring prompt check.
    if chatgpt_exec env -u CODEX_HOME \
        /usr/local/bin/codex-account --version </dev/null; then
        fail "chatgpt mode accepts an unset CODEX_HOME" \
            "expected the later non-TTY keyring refusal, but Codex ran"
    elif guest_stderr | grep -q "interactive TTY"; then
        pass "chatgpt mode accepts an unset CODEX_HOME"
    else
        fail "chatgpt mode accepts an unset CODEX_HOME" \
            "stderr: $(guest_stderr)"
    fi

    local guest_env
    guest_env=$(chatgpt_exec env || true)
    if echo "$guest_env" | grep -q '^OPENAI_API_KEY=' \
        || echo "$guest_env" | grep -qF "$host_openai" \
        || echo "$guest_env" | grep -qF "$overlay_openai"; then
        fail "chatgpt mode suppresses OPENAI_API_KEY" "raw host key reached the guest"
    else
        pass "chatgpt mode suppresses OPENAI_API_KEY"
    fi
    if echo "$guest_env" | grep -q '^COOP_TEST_CHATGPT_PASSTHROUGH=kept$'; then
        pass "chatgpt mode preserves non-secret runtime env"
    else
        fail "chatgpt mode preserves non-secret runtime env" \
            "positive-control env entry is missing"
    fi

    if chatgpt_exec test -e ./.codex/auth.json; then
        fail "chatgpt mode removes Codex auth.json" "stale auth.json remains in the guest"
    else
        pass "chatgpt mode removes Codex auth.json"
    fi

    chatgpt stop "$INSTANCE" || true
    if coop start "$INSTANCE"; then
        pass "restart after chatgpt-mode test restores default config"
    else
        fail "restart after chatgpt-mode test restores default config" \
            "stderr: $HARNESS_ERR"
    fi
}

test_codex_sandbox_bypass() {
    echo ""
    echo "=== Phase: codex sandbox bypass (issue #353) ==="

    # `coop codex` runs Codex unrestricted by prepending the global flag
    # `--dangerously-bypass-approvals-and-sandbox` (as `codex <flag> <args>`),
    # for parity with `coop claude`'s bypassPermissions. The VM is the isolation
    # boundary and Codex's own Linux sandbox does not work in the guest, so every
    # shell command Codex would otherwise run fails on sandbox setup.
    #
    # A full agentic run that actually spawns a sandboxed shell command needs model
    # credentials the CI guest doesn't have (Codex only reaches sandbox setup after
    # a model turn), so we can't drive a live tool call here. Instead we pin the
    # coop<->Codex contract: the installed Codex must accept that exact flag in the
    # global position coop uses. If a Codex release renames or moves it, this fails
    # loudly rather than silently re-enabling the broken sandbox.
    if coop_exec /usr/local/bin/codex --dangerously-bypass-approvals-and-sandbox \
        exec --help >/dev/null; then
        pass "codex accepts global --dangerously-bypass-approvals-and-sandbox flag"
    else
        fail "codex accepts global --dangerously-bypass-approvals-and-sandbox flag" \
            "stderr: $(guest_stderr)"
    fi
}

# `coop agent update` refreshes the in-guest agent binaries (issue #402).
# `--check` is cheap and network-tolerant (the Codex latest-version lookup
# degrades to "unknown" on failure, so the command still exits 0). The actual
# Codex reinstall downloads the release inside the guest, so it is gated behind
# --full.
test_agent_update() {
    echo ""
    echo "=== Phase: agent update ==="

    if coop agent update "$INSTANCE" --check; then
        if echo "$HARNESS_OUT" | grep -q "Claude Code" \
            && echo "$HARNESS_OUT" | grep -q "Codex"; then
            pass "agent update --check reports both agents"
        else
            fail "agent update --check reports both agents" "out: $HARNESS_OUT"
        fi
    else
        fail "agent update --check exits 0" "exit: $? stderr: $HARNESS_ERR"
    fi

    if [[ "$FULL" == "1" ]]; then
        # Model the broken legacy/corrupt-package state from #442. The forced
        # updater must rebuild an incomplete same-SHA release, not merely keep
        # an already healthy host executable in place.
        local installed_host installed_codex_version
        local inactive_sha active_sha inactive_release active_release active_pid active_exe
        inactive_sha="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        active_sha="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        inactive_release="/usr/local/lib/codex/releases/$inactive_sha"
        active_release="/usr/local/lib/codex/releases/$active_sha"
        active_pid=""

        # Seed one collectable release and one whose executable is live. The
        # updater should bound disk growth without breaking an in-flight Codex
        # session that still needs files from its package directory.
        # shellcheck disable=SC2016 # Variables expand in the guest's sh, not here.
        if guest_exec sudo sh -c '
            set -eu
            inactive_release=$1
            active_release=$2
            install -d -m 755 "$inactive_release" "$active_release/bin"
            cp /bin/sleep "$active_release/bin/hold"
            chmod 755 "$active_release/bin/hold"
            nohup "$active_release/bin/hold" 300 </dev/null >/dev/null 2>&1 &
            active_pid=$!
            echo "$active_pid" >/tmp/coop-codex-active-release.pid
            attempt=0
            while [ "$attempt" -lt 50 ]; do
                executable=$(readlink -f "/proc/$active_pid/exe" 2>/dev/null || true)
                [ "$executable" = "$active_release/bin/hold" ] && exit 0
                attempt=$((attempt + 1))
                sleep 0.1
            done
            exit 1
        ' sh "$inactive_release" "$active_release"; then
            active_pid=$(guest_exec cat /tmp/coop-codex-active-release.pid) \
                || active_pid=""
            active_exe=$(guest_exec sudo readlink -f "/proc/$active_pid/exe") \
                || active_exe=""
            if [[ "$active_exe" == "$active_release/bin/hold" ]]; then
                pass "seeded inactive and live Codex releases for garbage collection"
            else
                fail "seeded inactive and live Codex releases for garbage collection" \
                    "pid=${active_pid:-unknown} exe=${active_exe:-unknown} stderr: $(guest_stderr)"
            fi
        else
            active_pid=$(guest_exec cat /tmp/coop-codex-active-release.pid) \
                || active_pid=""
            fail "seeded inactive and live Codex releases for garbage collection" \
                "stderr: $(guest_stderr)"
        fi

        installed_host=$(guest_exec readlink -f /usr/local/bin/codex-code-mode-host)
        installed_codex_version=$(guest_exec codex --version)
        if guest_exec sudo rm -f "$installed_host" \
            && guest_exec test ! -e /usr/local/bin/codex-code-mode-host; then
            pass "removed code-mode host before updater repair test"
        else
            fail "removed code-mode host before updater repair test" \
                "host=$installed_host stderr: $(guest_stderr)"
        fi

        if coop agent update "$INSTANCE" --codex -y; then
            pass "agent update --codex exits 0"
        else
            fail "agent update --codex exits 0" "exit: $? stderr: $HARNESS_ERR"
        fi

        # The reinstall must leave an executable, correctly versioned binary.
        local ver
        if ver=$(guest_exec /usr/local/bin/codex --version) \
            && [[ "$ver" =~ [0-9]+\.[0-9]+\.[0-9]+ ]]; then
            pass "codex is executable and versioned after update ($ver)"
        else
            fail "codex is executable and versioned after update" \
                "got: $ver stderr: $(guest_stderr)"
        fi

        local repaired_host repaired_codex_version
        repaired_host=$(guest_exec readlink -f /usr/local/bin/codex-code-mode-host)
        repaired_codex_version=$(guest_exec codex --version)
        if guest_exec test -x /usr/local/bin/codex-code-mode-host \
            && { [[ "$repaired_codex_version" != "$installed_codex_version" ]] \
                || [[ "$repaired_host" == "$installed_host" ]]; }; then
            pass "codex update repairs missing code-mode host"
        else
            fail "codex update repairs missing code-mode host" \
                "before=$installed_host ($installed_codex_version) after=$repaired_host ($repaired_codex_version) stderr: $(guest_stderr)"
        fi

        validate_codex_package "after updater repair"

        if guest_exec test ! -e "$inactive_release"; then
            pass "codex update prunes an inactive release"
        else
            fail "codex update prunes an inactive release" \
                "release still exists: $inactive_release"
        fi
        active_exe=""
        if [[ -n "$active_pid" ]]; then
            active_exe=$(guest_exec sudo readlink -f "/proc/$active_pid/exe") \
                || active_exe=""
        fi
        if [[ "$active_exe" == "$active_release/bin/hold" ]] \
            && guest_exec test -d "$active_release" \
            && guest_exec sudo kill -0 "$active_pid"; then
            pass "codex update preserves a release with a live executable"
        else
            fail "codex update preserves a release with a live executable" \
                "release=$active_release pid=${active_pid:-unknown} exe=${active_exe:-unknown} stderr: $(guest_stderr)"
        fi

        if [[ -n "$active_pid" ]]; then
            active_exe=$(guest_exec sudo readlink -f "/proc/$active_pid/exe") \
                || active_exe=""
            if [[ "$active_exe" == "$active_release/bin/hold" ]]; then
                guest_exec sudo kill "$active_pid" 2>/dev/null || true
            fi
        fi
        guest_exec sudo rm -rf -- "$active_release" "$inactive_release" \
            /tmp/coop-codex-active-release.pid || true
    else
        skip "agent update --codex" "use --full; downloads the release in-guest"
    fi
}

# `coop agent update` requires a running VM; against a stopped instance it must
# fail with the shared "not running" guidance and a non-zero exit (issue #402).
test_agent_update_stopped() {
    echo ""
    echo "=== Phase: agent update (stopped instance) ==="

    if coop_fails agent update "$INSTANCE" --check; then
        if echo "$HARNESS_ERR" | grep -qi "not running"; then
            pass "agent update on a stopped instance fails with 'not running'"
        else
            fail "agent update on a stopped instance fails with 'not running'" \
                "stderr: $HARNESS_ERR"
        fi
    else
        fail "agent update on a stopped instance exits non-zero" "out: $HARNESS_OUT"
    fi
}

test_github_token_forwarding() {
    echo ""
    echo "=== Phase: github token forwarding ==="

    # Check the config's github auth strategy to determine expected behavior.
    # `github` may be a plain string ("auto" / "env" / "off" / "pat") or a
    # table — collapse the table form to its `mode` field.
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
    raw = cfg.get('github', 'off')
    if isinstance(raw, dict):
        raw = raw.get('mode', 'off')
    print(raw)
except Exception:
    print('off')
" 2>/dev/null || echo "off")

    local token_out
    token_out=$(env GITHUB_TOKEN=test-leak-token RUST_LOG=off \
        "$BINARY" exec "$INSTANCE" -- printenv GITHUB_TOKEN 2>/dev/null) || true

    if [[ "$github_setting" == "auto" || "$github_setting" == "env" ]]; then
        # Token should be forwarded
        if [[ "$token_out" == *"test-leak-token"* ]]; then
            pass "GITHUB_TOKEN forwarded to guest (github: $github_setting)"
        else
            fail "GITHUB_TOKEN forwarded to guest (github: $github_setting)" "got: ${token_out:-empty}"
        fi
    else
        # Token should NOT be forwarded (off / pat / unset)
        if [[ -z "$token_out" || "$token_out" != *"test-leak-token"* ]]; then
            pass "GITHUB_TOKEN not forwarded to guest (github: $github_setting)"
        else
            fail "GITHUB_TOKEN not forwarded to guest (github: $github_setting)" "got: $token_out"
        fi
    fi
}

# Verify that github = "pat" + a matching [github.pat] entry forwards the
# configured token (a server-side dummy literal, never a real PAT) into
# the guest for an instance whose workspace `origin` matches the entry.
# Uses `--workspace` with a forged origin rather than `--git-repo` so the
# test avoids a real network clone (post-#119 the clone path actively
# uses the configured PAT, which a dummy literal can't satisfy). Runs in
# the --full bucket because it boots a fresh VM.
test_github_pat_forwarding() {
    echo ""
    echo "=== Phase: github pat-mode token forwarding ==="

    local pat_instance="${INSTANCE}-pat"
    local token_file="$tmpdir/pat-token.txt"
    local cfg_file="$tmpdir/coop-pat.toml"
    local repo_url="https://github.com/trailofbits/coop.git"
    local repo_slug="trailofbits/coop"
    local sentinel="github_pat_test_FORWARDED_OK"

    # File-backend secret store: a 0600 file the wizard would have written.
    printf '%s' "$sentinel" > "$token_file"
    chmod 0600 "$token_file"

    # Config inherits the default data_dir + golden image. github =/= pat
    # mode is the only override.
    cat > "$cfg_file" <<CFGEOF
[github]
mode = "pat"

[github.pat."$repo_slug"]
token = "cmd:cat $token_file"
CFGEOF

    # Step 1: `coop validate` must parse pat-mode and report the entry.
    if env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" validate \
            >"$tmpdir/pat-validate.out" 2>&1; then
        if grep -q "github.pat.\"$repo_slug\": ok" "$tmpdir/pat-validate.out"; then
            pass "pat: validate reports the configured entry"
        else
            fail "pat: validate reports the configured entry" \
                "validate output: $(cat "$tmpdir/pat-validate.out")"
        fi
    else
        fail "pat: validate succeeds with a [github.pat] entry" \
            "$(cat "$tmpdir/pat-validate.out")"
    fi

    # Step 2: `coop github status` must list the entry without resolving it
    # (no --probe).
    if env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" github status \
            >"$tmpdir/pat-status.out" 2>&1; then
        if grep -q "$repo_slug" "$tmpdir/pat-status.out"; then
            pass "pat: github status lists the entry"
        else
            fail "pat: github status lists the entry" \
                "status output: $(cat "$tmpdir/pat-status.out")"
        fi
    else
        fail "pat: github status runs cleanly" "$(cat "$tmpdir/pat-status.out")"
    fi

    # Step 3: boot a VM with --workspace pointing at a local repo whose
    # `origin` is forged to the configured slug. `detect_instance_repo`
    # runs `git remote get-url origin` against the host workspace path on
    # follow-up commands, so `exec` resolves the slug, looks up the PAT
    # entry, and forwards the sentinel as GITHUB_TOKEN — exercising the
    # post-#119 PAT resolution without a real network clone or a real PAT.
    local ws_dir="$tmpdir/pat-workspace"
    mkdir -p "$ws_dir"
    (
        cd "$ws_dir" && \
        git init --quiet --initial-branch=main && \
        git -c user.email=ci@test -c user.name=CI commit --allow-empty --quiet -m "init" && \
        git remote add origin "$repo_url"
    ) || {
        fail "pat: set up forged-origin workspace" "git init/remote failed"
        return
    }

    if env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" up \
            "$ws_dir" \
            --name "$pat_instance" \
            --no-agents \
            --no-devcontainer \
            --no-prompt \
            >"$tmpdir/pat-start.out" 2>&1; then
        STARTED_INSTANCES+=("$pat_instance")
        pass "pat: up with workspace whose origin matches the configured slug"
    else
        fail "pat: up with workspace whose origin matches the configured slug" \
            "$(cat "$tmpdir/pat-start.out")"
        env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" destroy "$pat_instance" 2>/dev/null || true
        return
    fi

    local pat_token_out
    pat_token_out=$(env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY RUST_LOG=off \
        "$BINARY" --config "$cfg_file" exec "$pat_instance" -- printenv GITHUB_TOKEN 2>/dev/null) || true
    if [[ "$pat_token_out" == *"$sentinel"* ]]; then
        pass "pat: configured token forwarded to guest"
    else
        fail "pat: configured token forwarded to guest" "got: ${pat_token_out:-empty}"
    fi

    # Cleanup.
    env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" destroy "$pat_instance" 2>/dev/null || true
}

test_term_handling() {
    echo ""
    echo "=== Phase: TERM handling ==="

    # Modern terminals (Ghostty, Kitty) set TERM values that don't exist
    # in a stock Ubuntu install. The guest_term() function in ssh.rs remaps
    # unknown TERM values to xterm-256color for interactive sessions
    # (coop shell, coop claude). Non-interactive SSH (-- cmd) doesn't
    # allocate a PTY, so TERM doesn't matter there.
    #
    # This test verifies:
    # 1. Non-interactive SSH works when host has an exotic TERM
    # 2. The coop binary doesn't crash or reject exotic TERM values
    local output
    if output=$(env TERM=xterm-ghostty RUST_LOG=off \
        "$BINARY" shell "$INSTANCE" -- echo term-ok 2>/dev/null); then
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
    if whoami_out=$(guest_exec whoami); then
        if [[ "$whoami_out" == "ubuntu" ]]; then
            pass "guest user is 'ubuntu'"
        else
            fail "guest user is 'ubuntu'" "got: $whoami_out"
        fi
    else
        fail "guest user is 'ubuntu'" "ssh failed; stderr: $(guest_stderr)"
    fi

    # Check /workspace exists
    if guest_exec test -d /workspace; then
        pass "/workspace directory exists"
    else
        fail "/workspace directory exists" "stderr: $(guest_stderr)"
    fi

    # Check /workspace is owned by ubuntu
    local ws_owner
    if ws_owner=$(guest_exec stat -c '%U' /workspace); then
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
            fail "$tool is installed" "stderr: $(guest_stderr)"
        fi
    done

    # Check home directory
    local home
    if home=$(guest_exec printenv HOME); then
        if [[ "$home" == "/home/ubuntu" ]]; then
            pass "HOME is /home/ubuntu"
        else
            fail "HOME is /home/ubuntu" "got: $home"
        fi
    else
        fail "HOME is /home/ubuntu" "printenv failed"
    fi

    # Verify `--env` from test_start landed in the guest process env.
    local guest_env_val
    if guest_env_val=$(guest_exec printenv COOP_TEST_GUEST_ENV); then
        if [[ "$guest_env_val" == "hello-from-cli" ]]; then
            pass "guest_env from --env reaches the guest"
        else
            fail "guest_env from --env reaches the guest" "got: $guest_env_val"
        fi
    else
        fail "guest_env from --env reaches the guest" "printenv failed; stderr: $(guest_stderr)"
    fi
}

test_sudo() {
    echo ""
    echo "=== Phase: sudo ==="

    # Sudo should work without a password
    local sudo_out
    if sudo_out=$(guest_exec sudo whoami); then
        if [[ "$sudo_out" == "root" ]]; then
            pass "sudo works without password"
        else
            fail "sudo works without password" "got: $sudo_out"
        fi
    else
        fail "sudo works without password" "sudo command failed; stderr: $(guest_stderr)"
    fi

    # Sudo should be able to write to root-owned locations
    if guest_exec sudo touch /root/test-sudo-write; then
        pass "sudo can write to /root"
        guest_exec sudo rm -f /root/test-sudo-write || true
    else
        fail "sudo can write to /root" "stderr: $(guest_stderr)"
    fi
}

test_network() {
    echo ""
    echo "=== Phase: network connectivity ==="

    # DNS resolution
    if guest_exec nslookup github.com >/dev/null || \
       guest_exec host github.com >/dev/null || \
       guest_exec getent hosts github.com >/dev/null; then
        pass "DNS resolution works"
    else
        fail "DNS resolution works" "all resolution methods failed; stderr: $(guest_stderr)"
    fi

    # HTTP connectivity (use a reliable endpoint)
    local http_code
    if http_code=$(guest_exec curl -s -o /dev/null -w '%{http_code}' --max-time 10 https://api.github.com); then
        if [[ "$http_code" =~ ^[23] ]]; then
            pass "HTTPS connectivity works (HTTP $http_code)"
        else
            fail "HTTPS connectivity works" "HTTP $http_code"
        fi
    else
        fail "HTTPS connectivity works" "curl failed"
    fi
}

test_docker() {
    echo ""
    echo "=== Phase: docker ==="

    if guest_exec docker info >/dev/null; then
        pass "docker daemon is running"
    else
        fail "docker daemon is running" "stderr: $(guest_stderr)"
        return
    fi

    # User should be in docker group (no sudo needed)
    local groups_out
    if groups_out=$(guest_exec groups); then
        if echo "$groups_out" | grep -q "docker"; then
            pass "ubuntu user in docker group"
        else
            fail "ubuntu user in docker group" "groups: $groups_out"
        fi
    else
        fail "ubuntu user in docker group" "groups command failed; stderr: $(guest_stderr)"
    fi

    local docker_out
    if docker_out=$(guest_exec docker run --rm hello-world); then
        if echo "$docker_out" | grep -q "Hello from Docker"; then
            pass "docker run hello-world works"
        else
            fail "docker run hello-world works" "unexpected stdout: $docker_out"
        fi
    else
        fail "docker run hello-world works" "docker run failed; stderr: $(guest_stderr)"
    fi

    # Docker port mapping (verifies bridge networking + iptables)
    local port_test_ok=false
    if guest_exec docker run -d --name port-test -p 8080:80 nginx:alpine >/dev/null; then
        # Give nginx a moment to start
        sleep 2
        local port_out
        if port_out=$(guest_exec curl -s --max-time 5 http://localhost:8080); then
            if echo "$port_out" | grep -qi "nginx\|welcome"; then
                pass "docker port mapping works"
                port_test_ok=true
            fi
        fi
        if ! $port_test_ok; then
            fail "docker port mapping works" "curl to mapped port failed; stderr: $(guest_stderr)"
        fi
        guest_exec docker rm -f port-test >/dev/null || true
    else
        fail "docker port mapping works" "failed to start nginx container; stderr: $(guest_stderr)"
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
                if guest_exec python3 --version >/dev/null; then
                    pass "python3 installed (profile: python)"
                else
                    fail "python3 installed (profile: python)" "stderr: $(guest_stderr)"
                fi
                ;;
            node)
                if guest_exec node --version >/dev/null; then
                    pass "node installed (profile: node)"
                else
                    fail "node installed (profile: node)" "stderr: $(guest_stderr)"
                fi
                ;;
            rust)
                # Rust is installed for the ubuntu user via rustup
                if guest_exec rustc --version >/dev/null; then
                    pass "rustc installed (profile: rust)"
                else
                    fail "rustc installed (profile: rust)" "stderr: $(guest_stderr)"
                fi
                ;;
            go)
                if guest_exec go version >/dev/null; then
                    pass "go installed (profile: go)"
                else
                    fail "go installed (profile: go)" "stderr: $(guest_stderr)"
                fi
                ;;
            c)
                if guest_exec clang --version >/dev/null; then
                    pass "clang installed (profile: c)"
                else
                    fail "clang installed (profile: c)" "stderr: $(guest_stderr)"
                fi
                ;;
            fuzz)
                if guest_exec which afl-fuzz >/dev/null; then
                    pass "afl-fuzz installed (profile: fuzz)"
                else
                    fail "afl-fuzz installed (profile: fuzz)" "stderr: $(guest_stderr)"
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
    val=$(guest_exec lsb_release -ds) || val="unknown"
    echo "os=$val" >> "$fp"

    # Architecture
    val=$(guest_exec uname -m) || val="unknown"
    echo "arch=$val" >> "$fp"

    # User and groups
    val=$(guest_exec id) || val="unknown"
    echo "id=$val" >> "$fp"

    # Base tool versions
    local tool
    for tool in git curl docker node python3 rustc go; do
        val=$(guest_exec "$tool" --version | head -1) || val="not installed"
        echo "${tool}=$val" >> "$fp"
    done

    # Docker info: storage driver and iptables mode
    val=$(guest_exec docker info --format '{{.Driver}}') || val="unknown"
    echo "docker_storage=$val" >> "$fp"

    # DNS resolver
    if guest_exec test -f /etc/resolv.conf; then
        val=$(guest_exec grep -m1 '^nameserver' /etc/resolv.conf) || val="none"
    else
        val="no resolv.conf"
    fi
    echo "dns=$val" >> "$fp"

    # Directory layout
    for dir in /workspace /home/ubuntu /home/ubuntu/.ssh; do
        if guest_exec test -d "$dir"; then
            local owner
            owner=$(guest_exec stat -c '%U:%G' "$dir") || owner="unknown"
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

test_stop_idempotency() {
    echo ""
    echo "=== Phase: stop idempotency ==="

    # The primary instance was just stopped by test_stop. Reuse that state
    # instead of booting a throwaway VM solely to stop it twice.
    local rc=0
    "$BINARY" stop "$INSTANCE" >/dev/null 2>&1 || rc=$?
    if [[ $rc -eq 0 ]]; then
        pass "second stop succeeds (idempotent)"
    else
        fail "second stop is idempotent" "exit code: $rc"
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
        skip "shell rejects when only stopped instances exist" "$running_count still running"
        return
    fi

    if coop_fails shell -- echo "should-not-work"; then
        pass "shell rejects when only stopped instances exist"
        if echo "$HARNESS_ERR" | grep -qi "stopped"; then
            pass "shell error mentions instance is stopped"
        else
            fail "shell error mentions instance is stopped" "stderr: $HARNESS_ERR"
        fi
    else
        fail "shell rejects when only stopped instances exist" "should have failed"
    fi
}

test_list_stopped() {
    echo ""
    echo "=== Phase: list (stopped) ==="

    if coop list; then
        pass "list exits 0 after stop"
    else
        fail "list exits 0 after stop" "exit code: $?"
    fi

    if echo "$HARNESS_OUT" | grep -qE "^${INSTANCE} +stopped\$"; then
        pass "list shows instance as stopped"
    else
        fail "list shows instance as stopped" "got: $HARNESS_OUT"
    fi
}

test_list_empty() {
    echo ""
    echo "=== Phase: list (no instances) ==="

    if ! coop list; then
        fail "list exits 0 with no instances" "exit code: $?"
        return
    fi

    # Dev/CI hosts may carry long-lived instances unrelated to this run.
    # Assert what this phase actually owns: the just-destroyed `$INSTANCE`
    # is gone. The empty-state message is only asserted on a clean host.
    local instance_count
    instance_count=$(RUST_LOG=off "$BINARY" status 2>/dev/null | grep -cE "running|stopped" || true)

    if [[ "$instance_count" -gt 0 ]]; then
        if echo "$HARNESS_OUT" | grep -qE "^${INSTANCE} "; then
            fail "destroyed instance no longer in list" "still present: $HARNESS_OUT"
        else
            pass "destroyed instance no longer in list ($instance_count other(s) present)"
        fi
        return
    fi

    if echo "$HARNESS_OUT" | grep -q "No instances found"; then
        pass "list shows empty-state message"
    else
        fail "list shows empty-state message" "got: $HARNESS_OUT"
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

test_reconfigure() {
    echo ""
    echo "=== Phase: resize changes memory and vCPUs ==="

    # The instance is stopped here. Apply new mem/vcpu and boot in one
    # step (--start) so the result can be read back via `coop status`,
    # which needs a running VM on Firecracker. Values are small and safe
    # to boot on any host, and differ from the defaults so a no-op would
    # be caught.
    local want_vcpus=1
    local want_mem=2048

    if coop resize "$INSTANCE" --vcpus "$want_vcpus" --mem "$want_mem" --start; then
        pass "resize --vcpus/--mem --start exits 0"
    else
        fail "resize --vcpus/--mem --start exits 0" "exit code: $?"
        return
    fi

    # On Firecracker, `--start` routes through configure(), which must
    # preserve the freshly-written mem/vcpu instead of resetting them to
    # the global config — so this single check also exercises the
    # preserve-on-restart behavior. On Lima the values come from the
    # re-read lima.yaml via limactl.
    if ! coop status "$INSTANCE" 2>/dev/null; then
        fail "status after reconfigure" "status unavailable for running instance"
        return
    fi

    local got_vcpus got_mem
    got_vcpus=$(echo "$HARNESS_OUT" | sed -n 's/.*vCPUs: \([0-9][0-9]*\).*/\1/p' | head -1)
    got_mem=$(echo "$HARNESS_OUT" | sed -n 's/.*Memory: \([0-9][0-9]*\) MiB.*/\1/p' | head -1)

    if [[ "$got_vcpus" == "$want_vcpus" ]]; then
        pass "status reflects reconfigured vCPUs (${got_vcpus})"
    else
        fail "status reflects reconfigured vCPUs" "expected ${want_vcpus}, got ${got_vcpus}"
    fi

    if [[ "$got_mem" == "$want_mem" ]]; then
        pass "status reflects reconfigured memory (${got_mem} MiB)"
    else
        fail "status reflects reconfigured memory" "expected ${want_mem} MiB, got ${got_mem}"
    fi

    # Return the instance to a stopped state for the phases that follow
    # (commit/restore/restart all expect it stopped).
    if coop stop "$INSTANCE" >/dev/null 2>&1; then
        pass "stop after reconfigure exits 0"
    else
        fail "stop after reconfigure exits 0" "exit code: $?"
    fi
}

test_commit_restore() {
    echo ""
    echo "=== Phase: commit + restore ==="

    # The instance is stopped from earlier phases. commit/restore both
    # require a stopped instance for filesystem consistency.
    local snap="snap-$$"

    # Seed a sentinel into the guest home directory and capture the
    # instance's Guest IP before any commit/restore. The home dir lives on
    # the rootfs that commit/restore swaps, so the sentinel round-trips with
    # the disk: baking "committed" into the image now lets a later read prove
    # restore brought the committed bytes back (not a default or wrong disk).
    # The IP is captured so we can later assert it survived the disk swap.
    local ip_before=""
    if coop start "$INSTANCE" --no-agents >/dev/null 2>&1; then
        if guest_exec sh -c 'echo committed > ~/sentinel'; then
            pass "seed sentinel into guest before commit"
        else
            fail "seed sentinel into guest before commit" "stderr: $(guest_stderr)"
        fi
        if coop status "$INSTANCE" 2>/dev/null; then
            ip_before=$(echo "$HARNESS_OUT" | sed -n 's/.*Guest IP: \([0-9.][0-9.]*\).*/\1/p' | head -1)
        fi
        coop stop "$INSTANCE" >/dev/null 2>&1 || true
    else
        fail "start instance to seed sentinel" "could not start instance"
    fi

    # Commit the stopped instance's filesystem as a new image.
    if coop commit "$INSTANCE" --image "$snap"; then
        pass "commit exits 0"
    else
        fail "commit exits 0" "exit code: $?; stderr: $HARNESS_ERR"
        return
    fi

    # The committed image is an ordinary image — `coop images` lists it.
    if coop images && echo "$HARNESS_OUT" | grep -q "$snap"; then
        pass "images lists committed image '$snap'"
    else
        fail "images lists committed image '$snap'" "output: $HARNESS_OUT"
    fi

    # Committing onto an existing name without --force is refused.
    if coop_fails commit "$INSTANCE" --image "$snap"; then
        pass "commit without --force refuses to overwrite"
        if echo "$HARNESS_ERR" | grep -qi "already exists\|--force"; then
            pass "overwrite error suggests --force"
        else
            fail "overwrite error suggests --force" "stderr: $HARNESS_ERR"
        fi
    else
        fail "commit without --force refuses to overwrite" "should have failed"
    fi

    # --force overwrites the existing image.
    if coop commit "$INSTANCE" --image "$snap" --force; then
        pass "commit --force overwrites existing image"
    else
        fail "commit --force overwrites existing image" "exit code: $?; stderr: $HARNESS_ERR"
    fi

    # A committed image is an ordinary launchable base image. `coop up
    # --image <snap>` boots a brand-new instance from it, exercising the
    # TemplateConfig carry-over (guest_user/profiles/hashes) that cmd_commit
    # writes and `up` reads back — a path `restore` (same-instance disk swap)
    # never takes. A commit that wrote a broken template config would still
    # pass every restore assertion but fail here.
    local commit_inst="${INSTANCE}-from-commit"
    local commit_ws="$tmpdir/${commit_inst}-ws"
    mkdir -p "$commit_ws"
    if coop up "$commit_ws" --name "$commit_inst" --no-agents --no-devcontainer --image "$snap"; then
        STARTED_INSTANCES+=("$commit_inst")
        pass "up --image (committed snapshot) exits 0"

        if coop status "$commit_inst" && echo "$HARNESS_OUT" | grep -qi "running"; then
            pass "instance from committed image is running"
        else
            fail "instance from committed image is running" "status: $HARNESS_OUT"
        fi

        GUEST_INSTANCE="$commit_inst"
        local up_hostname up_sentinel
        up_hostname=$(guest_exec hostname) || up_hostname=""
        up_sentinel=$(coop_exec sh -c 'cat ~/sentinel') || up_sentinel=""
        unset GUEST_INSTANCE

        if [[ -n "$up_hostname" ]]; then
            pass "guest from committed image reachable via SSH"
        else
            fail "guest from committed image reachable via SSH" "no response"
        fi
        # The committed bytes carried through the up path, not just a boot.
        if [[ "$up_sentinel" == "committed" ]]; then
            pass "committed contents present in up --image instance"
        else
            fail "committed contents present in up --image instance" \
                "sentinel='$up_sentinel' (expected 'committed')"
        fi

        coop destroy "$commit_inst" >/dev/null 2>&1 || true
        untrack_instance "$commit_inst"
    else
        fail "up --image (committed snapshot) exits 0" "exit code: $?; stderr: $HARNESS_ERR"
        coop destroy "$commit_inst" 2>/dev/null || true
        untrack_instance "$commit_inst"
    fi

    # Mutate the live disk before restoring, so the restore has something to
    # roll back. Without this the sentinel would still read "committed" even
    # if restore were a no-op; overwriting it first makes the post-restore
    # read a real discriminator.
    if coop start "$INSTANCE" --no-agents >/dev/null 2>&1; then
        if guest_exec sh -c 'echo modified > ~/sentinel'; then
            pass "mutate sentinel on live disk before restore"
        else
            fail "mutate sentinel on live disk before restore" "stderr: $(guest_stderr)"
        fi
        coop stop "$INSTANCE" >/dev/null 2>&1 || true
    else
        fail "start instance to mutate sentinel" "could not start instance"
    fi

    # Restore rolls the instance back to the committed image in place.
    if coop restore "$INSTANCE" --image "$snap"; then
        pass "restore exits 0"
    else
        fail "restore exits 0" "exit code: $?; stderr: $HARNESS_ERR"
    fi

    # Status lineage now tracks the restored image.
    if coop status "$INSTANCE" 2>/dev/null && echo "$HARNESS_OUT" | grep -q "$snap"; then
        pass "status shows restored image '$snap'"
    else
        fail "status shows restored image '$snap'" "output: $HARNESS_OUT"
    fi

    # Restoring a non-existent image fails with a clear message.
    if coop_fails restore "$INSTANCE" --image "no-such-image-$$"; then
        pass "restore rejects unknown image"
    else
        fail "restore rejects unknown image" "should have failed"
    fi

    # The central claim: a committed image is a usable image. Boot the
    # instance from the just-restored committed disk and confirm the guest
    # is reachable — this exercises the re-patched guest network and proves
    # the committed rootfs actually boots, not merely that it's listed.
    if coop start "$INSTANCE" --no-agents >/dev/null 2>&1; then
        if coop status "$INSTANCE" && echo "$HARNESS_OUT" | grep -qi "running"; then
            pass "instance boots from restored committed image"
        else
            fail "instance boots from restored committed image" "status: $HARNESS_OUT"
        fi

        # Identity: the Guest IP must survive the disk swap. The IP is
        # derived from the instance index, so this guards against a future
        # restore that reassigns the index/IP instead of swapping only the
        # disk (restore's contract is name/index/IP/workspace unchanged).
        # Firecracker status surfaces the IP; Lima status does not, so the
        # compare degrades to a skip there rather than a spurious failure.
        local ip_after=""
        if coop status "$INSTANCE" 2>/dev/null; then
            ip_after=$(echo "$HARNESS_OUT" | sed -n 's/.*Guest IP: \([0-9.][0-9.]*\).*/\1/p' | head -1)
        fi
        if [[ -z "$ip_before" || -z "$ip_after" ]]; then
            skip "guest IP preserved across restore" "status does not surface Guest IP"
        elif [[ "$ip_before" == "$ip_after" ]]; then
            pass "guest IP preserved across restore ($ip_after)"
        else
            fail "guest IP preserved across restore" "before=$ip_before after=$ip_after"
        fi

        GUEST_INSTANCE="$INSTANCE"
        local snap_hostname restored_sentinel
        snap_hostname=$(guest_exec hostname) || snap_hostname=""
        restored_sentinel=$(coop_exec sh -c 'cat ~/sentinel') || restored_sentinel=""
        unset GUEST_INSTANCE
        if [[ -n "$snap_hostname" ]]; then
            pass "guest reachable after restore from committed image"
        else
            fail "guest reachable after restore from committed image" "no response"
        fi

        # Content fidelity: the restored disk carries the *committed* bytes,
        # not the "modified" overwrite from before the restore. This is the
        # assertion that distinguishes a working disk swap from a no-op or a
        # wrong-disk swap that still boots and answers hostname.
        if [[ "$restored_sentinel" == "committed" ]]; then
            pass "restore brings back committed contents"
        else
            fail "restore brings back committed contents" \
                "sentinel='$restored_sentinel' (expected 'committed')"
        fi

        # Association: the workspace mapping survived the disk swap, so
        # /workspace is present and reachable after restore.
        if coop exec "$INSTANCE" -- test -d /workspace; then
            pass "workspace association preserved across restore"
        else
            fail "workspace association preserved across restore" "/workspace missing after restore"
        fi

        # commit/restore on a running instance must be refused.
        if coop_fails commit "$INSTANCE" --image "$snap-live"; then
            pass "commit refuses a running instance"
            if echo "$HARNESS_ERR" | grep -qi "running\|stop"; then
                pass "commit error tells the user to stop first"
            else
                fail "commit error tells the user to stop first" "stderr: $HARNESS_ERR"
            fi
        else
            fail "commit refuses a running instance" "should have failed"
            coop images --delete "$snap-live" 2>/dev/null || true
        fi

        coop stop "$INSTANCE" >/dev/null 2>&1 || true
    else
        skip "instance boots from restored committed image" "could not start instance"
    fi

    # Roll lineage back to the default image before deleting the snapshot,
    # so the instance isn't left pointing at a deleted image for the
    # restart/destroy phases that follow.
    if coop restore "$INSTANCE" --image default; then
        pass "restore back to default image exits 0"
    else
        fail "restore back to default image exits 0" "exit code: $?; stderr: $HARNESS_ERR"
    fi

    # Clean up the committed image.
    if coop images --delete "$snap"; then
        pass "delete committed image exits 0"
    else
        fail "delete committed image exits 0" "exit code: $?"
    fi
}

test_restart_stopped() {
    echo ""
    echo "=== Phase: restart stopped instance ==="

    # Restart the stopped instance (was stopped in previous phase)
    if coop start "$INSTANCE" --no-agents; then
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
    if coop exec "$INSTANCE" -- test -d /workspace; then
        pass "workspace persists across restart"
    else
        fail "workspace persists across restart" "exit code: $?"
    fi

    # Verify duplicate start of running instance is rejected
    if coop_fails start "$INSTANCE" --no-agents; then
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

    # Instance is stopped from previous phase. Restarting with a --workspace
    # that does not match the named instance's restart key should fail with a
    # clear error instead of silently ignoring it.

    local mount_dir
    mount_dir=$(mktemp -d)

    if coop_fails start "$INSTANCE" --no-agents --workspace "$mount_dir"; then
        pass "restart with --workspace rejected"
    else
        fail "restart with --workspace rejected" "should have failed"
        coop stop "$INSTANCE" 2>/dev/null || true
    fi

    if echo "$HARNESS_ERR" | grep -qi "already exists\|ignored on restart\|destroy"; then
        pass "error message suggests destroy first"
    else
        fail "error message suggests destroy first" "stderr: $HARNESS_ERR"
    fi

    # Creation-time flags no longer exist on `coop start`; clap rejects them as
    # unknown arguments. (`--mount`, `--git-repo`, `--vcpus`, `--mem`, `--disk`,
    # `--image`, `--exclude-git`, `--profile` all live on `coop up`.)
    local flag
    for flag in "--mount $mount_dir" "--disk 20" "--vcpus 4" --exclude-git; do
        # shellcheck disable=SC2086
        if coop_fails start "$INSTANCE" --no-agents $flag; then
            pass "start ${flag%% *} rejected by clap"
        else
            fail "start ${flag%% *} rejected by clap" "should have failed"
            coop stop "$INSTANCE" 2>/dev/null || true
        fi
    done

    # Plain restart (no conflicting flags) should still work
    if coop start "$INSTANCE" --no-agents; then
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
        skip "shell rejects when no instances exist" "$instance_count instances still present"
        return
    fi

    if coop_fails shell -- echo "should-not-work"; then
        pass "shell rejects when no instances exist"
        if echo "$HARNESS_ERR" | grep -qi "no instances"; then
            pass "shell error mentions no instances"
        else
            fail "shell error mentions no instances" "stderr: $HARNESS_ERR"
        fi
    else
        fail "shell rejects when no instances exist" "should have failed"
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

}

# ── Quickstart (--full only) ──────────────────────────────────

# `coop quickstart` chains ensure-image, ensure-instance, launch-claude. The
# first two steps share code with `coop setup` / `coop up` (covered above);
# this phase exercises the reconnect/restart logic specific to quickstart and
# the workspace-affinity lookup that drives it.
#
# Claude is interactive — with stdin redirected from /dev/null `ssh` doesn't
# allocate a PTY and the claude binary fails fast. `run_interactive` only
# logs (does not error) on non-zero remote status, so quickstart still
# returns 0 to the host. Each invocation is bounded with `_timeout` defensively
# so a future claude that hangs on EOF can't wedge the suite.
test_quickstart() {
    echo ""
    echo "=== Phase: quickstart ==="

    if ! "$BINARY" exec --help >/dev/null 2>&1; then
        skip "quickstart" "binary missing exec subcommand"
        return
    fi

    local qs_ws qs_inst_name
    qs_ws=$(mktemp -d "$tmpdir/qs-ws-XXXXXX")
    # The instance name is the sanitised basename of the workspace path.
    # `basename | tr` would rewrite the trailing newline to a dash; use bash
    # pattern expansion to mirror `sanitize_basename` in src/config.rs.
    qs_inst_name=${qs_ws##*/}
    qs_inst_name=${qs_inst_name//[!a-zA-Z0-9_-]/-}

    # First invocation: image is already built (from test_setup) and no
    # instance for this workspace exists, so quickstart must allocate fresh.
    local rc=0
    ( cd "$qs_ws" && _timeout 180 "$BINARY" quickstart --no-devcontainer \
        </dev/null >"$tmpdir/qs1_out" 2>"$tmpdir/qs1_err" ) || rc=$?

    if [[ $rc -eq 0 ]]; then
        pass "first quickstart exits 0"
    else
        fail "first quickstart exits 0" "exit: $rc; stderr: $(cat "$tmpdir/qs1_err")"
        rm -rf "$qs_ws"
        return
    fi

    # Track for cleanup even if status assertions below fail.
    STARTED_INSTANCES+=("$qs_inst_name")

    # `ssh::run_interactive` swallows non-zero remote exit, so checking only
    # `rc=0` would let a quickstart that bailed before the ssh leg slip
    # through. Grep the tracing output for the connect line to confirm the
    # claude exec was actually reached.
    if grep -q "Connecting via SSH" "$tmpdir/qs1_err"; then
        pass "first quickstart reached SSH/claude exec"
    else
        fail "first quickstart reached SSH/claude exec" \
            "stderr: $(cat "$tmpdir/qs1_err")"
    fi

    if "$BINARY" status "$qs_inst_name" >/dev/null 2>&1; then
        pass "quickstart created and started instance '$qs_inst_name'"
    else
        fail "quickstart created and started instance" "no instance '$qs_inst_name'"
        rm -rf "$qs_ws"
        return
    fi

    # Second invocation in the same cwd: must reconnect — same name, no new
    # instance allocated.
    local pre_list post_list
    pre_list=$("$BINARY" list 2>/dev/null | sort)

    rc=0
    ( cd "$qs_ws" && _timeout 180 "$BINARY" quickstart --no-devcontainer \
        </dev/null >"$tmpdir/qs2_out" 2>"$tmpdir/qs2_err" ) || rc=$?

    if [[ $rc -eq 0 ]]; then
        pass "second quickstart exits 0 (reconnect path)"
    else
        fail "second quickstart exits 0" "exit: $rc; stderr: $(cat "$tmpdir/qs2_err")"
    fi

    if grep -q "Connecting via SSH" "$tmpdir/qs2_err"; then
        pass "second quickstart reached SSH/claude exec"
    else
        fail "second quickstart reached SSH/claude exec" \
            "stderr: $(cat "$tmpdir/qs2_err")"
    fi

    post_list=$("$BINARY" list 2>/dev/null | sort)
    if [[ "$pre_list" == "$post_list" ]]; then
        pass "second quickstart reuses existing instance for cwd"
    else
        fail "second quickstart reuses existing instance for cwd" \
            "list changed (diff): $(diff <(echo "$pre_list") <(echo "$post_list"))"
    fi

    # Stop and re-invoke: must restart the same stopped instance.
    if ! coop stop "$qs_inst_name"; then
        fail "stop quickstart instance before restart test" "exit code: $?"
        return
    fi

    rc=0
    ( cd "$qs_ws" && _timeout 180 "$BINARY" quickstart --no-devcontainer \
        </dev/null >"$tmpdir/qs3_out" 2>"$tmpdir/qs3_err" ) || rc=$?

    if [[ $rc -eq 0 ]]; then
        pass "third quickstart exits 0 (restart path)"
    else
        fail "third quickstart exits 0" "exit: $rc; stderr: $(cat "$tmpdir/qs3_err")"
    fi

    if grep -q "Connecting via SSH" "$tmpdir/qs3_err"; then
        pass "third quickstart reached SSH/claude exec"
    else
        fail "third quickstart reached SSH/claude exec" \
            "stderr: $(cat "$tmpdir/qs3_err")"
    fi

    if "$BINARY" status "$qs_inst_name" 2>/dev/null | grep -q -i running; then
        pass "quickstart restarted stopped instance '$qs_inst_name'"
    else
        fail "quickstart restarted stopped instance" \
            "status: $("$BINARY" status "$qs_inst_name" 2>&1)"
    fi

    # Cleanup
    coop destroy "$qs_inst_name" 2>/dev/null || true
    untrack_instance "$qs_inst_name"
    rm -rf "$qs_ws"
}

# ── Project workflow tests (`coop up`, --full only) ───────────

test_up_project_workflow() {
    echo ""
    echo "=== Phase: project up ==="

    local up_ws up_inst_name
    up_ws=$(mktemp -d "$tmpdir/up-ws-XXXXXX")
    echo "up-copy-content" > "$up_ws/hello.txt"
    up_inst_name=${up_ws##*/}
    up_inst_name=${up_inst_name//[!a-zA-Z0-9_-]/-}

    if coop up "$up_ws" --no-agents --no-devcontainer; then
        STARTED_INSTANCES+=("$up_inst_name")
        pass "up copy creates project instance"
    else
        fail "up copy creates project instance" "exit code: $? stderr: $HARNESS_ERR"
        rm -rf "$up_ws"
        return
    fi

    GUEST_INSTANCE="$up_inst_name"
    local content
    if content=$(guest_exec cat /workspace/hello.txt); then
        if [[ "$content" == "up-copy-content" ]]; then
            pass "up copy syncs project into /workspace"
        else
            fail "up copy syncs project into /workspace" "got: $content"
        fi
    else
        fail "up copy syncs project into /workspace" "file not found"
    fi
    unset GUEST_INSTANCE

    local pre_list post_list
    pre_list=$("$BINARY" list 2>/dev/null | sort)
    if coop up "$up_ws" --no-devcontainer; then
        pass "up copy re-run exits 0 while running"
    else
        fail "up copy re-run exits 0 while running" "exit code: $? stderr: $HARNESS_ERR"
    fi
    post_list=$("$BINARY" list 2>/dev/null | sort)
    if [[ "$pre_list" == "$post_list" ]]; then
        pass "up copy reuses running project instance"
    else
        fail "up copy reuses running project instance" \
            "list changed (diff): $(diff <(echo "$pre_list") <(echo "$post_list"))"
    fi

    if coop stop "$up_inst_name" && coop up "$up_ws" --no-agents --no-devcontainer; then
        pass "up copy restarts stopped project instance"
    else
        fail "up copy restarts stopped project instance" "stderr: $HARNESS_ERR"
    fi

    local reject_ws data_dir
    reject_ws=$(mktemp -d "$tmpdir/up-reject-XXXXXX")
    data_dir=$(mktemp -d "$tmpdir/up-data-XXXXXX")
    if coop_fails up "$reject_ws" --extra-mount "$data_dir" --no-devcontainer; then
        if echo "$HARNESS_ERR" | grep -q "/workspace"; then
            pass "up copy rejects host-only extra mount at /workspace"
        else
            fail "up copy rejects host-only extra mount at /workspace" "stderr: $HARNESS_ERR"
        fi
    else
        fail "up copy rejects host-only extra mount at /workspace" \
            "command unexpectedly succeeded"
    fi
    rm -rf "$reject_ws" "$data_dir"

    coop destroy "$up_inst_name" 2>/dev/null || true
    untrack_instance "$up_inst_name"
    rm -rf "$up_ws"

    local mount_ws mount_inst_name
    mount_ws=$(mktemp -d "$tmpdir/up-mount-XXXXXX")
    echo "up-mount-content" > "$mount_ws/marker.txt"
    mount_inst_name=${mount_ws##*/}
    mount_inst_name=${mount_inst_name//[!a-zA-Z0-9_-]/-}

    if coop up "$mount_ws" --mount --no-agents --no-devcontainer; then
        STARTED_INSTANCES+=("$mount_inst_name")
        pass "up mount creates project instance"
    else
        fail "up mount creates project instance" "exit code: $? stderr: $HARNESS_ERR"
        rm -rf "$mount_ws"
        return
    fi

    GUEST_INSTANCE="$mount_inst_name"
    if content=$(guest_exec cat /workspace/marker.txt); then
        if [[ "$content" == "up-mount-content" ]]; then
            pass "up mount exposes project at /workspace"
        else
            fail "up mount exposes project at /workspace" "got: $content"
        fi
    else
        fail "up mount exposes project at /workspace" "file not found"
    fi

    if [[ "$(uname -s)" == "Darwin" ]]; then
        echo "live-up-update" > "$mount_ws/live.txt"
        if content=$(guest_exec cat /workspace/live.txt); then
            if [[ "$content" == "live-up-update" ]]; then
                pass "up mount is live on Lima"
            else
                fail "up mount is live on Lima" "got: $content"
            fi
        else
            fail "up mount is live on Lima" "file not found"
        fi
    else
        skip "up mount live sync" "Firecracker uses one-time rsync sync"
    fi
    unset GUEST_INSTANCE

    pre_list=$("$BINARY" list 2>/dev/null | sort)
    if coop up "$mount_ws" --mount --no-devcontainer; then
        pass "up mount re-run exits 0 while running"
    else
        fail "up mount re-run exits 0 while running" "exit code: $? stderr: $HARNESS_ERR"
    fi
    post_list=$("$BINARY" list 2>/dev/null | sort)
    if [[ "$pre_list" == "$post_list" ]]; then
        pass "up mount reuses running project instance"
    else
        fail "up mount reuses running project instance" \
            "list changed (diff): $(diff <(echo "$pre_list") <(echo "$post_list"))"
    fi

    coop destroy "$mount_inst_name" 2>/dev/null || true
    untrack_instance "$mount_inst_name"
    rm -rf "$mount_ws"
}

# ── git-repo source (--full only) ─────────────────────────────

# Exercise `coop up --git-repo`: the clone runs inside the guest, so this
# needs guest network access (already required by test_network). Uses
# octocat/Hello-World, GitHub's canonical tiny public repo, cloned
# anonymously (no [github] config). Instance-name derivation is covered
# by unit tests; here we pass --name for deterministic cleanup.
test_git_repo() {
    echo ""
    echo "=== Phase: up --git-repo (clone) ==="

    local repo_url="https://github.com/octocat/Hello-World.git"
    local gr_instance="${INSTANCE}-gitrepo"
    local data_dir
    data_dir=$(mktemp -d "$tmpdir/gitrepo-data-XXXXXX")
    echo "extra-mount-marker" > "$data_dir/marker.txt"

    # --git-repo conflicts with the local-directory sources (arg parsing,
    # no VM boot).
    local some_dir
    some_dir=$(mktemp -d "$tmpdir/gitrepo-dir-XXXXXX")
    if coop_fails up "$some_dir" --git-repo "$repo_url" --no-devcontainer; then
        pass "up --git-repo conflicts with a positional directory"
    else
        fail "up --git-repo conflicts with a positional directory" "should have failed"
    fi
    if coop_fails up --git-repo "$repo_url" --mount --no-devcontainer; then
        pass "up --git-repo conflicts with --mount"
    else
        fail "up --git-repo conflicts with --mount" "should have failed"
    fi
    rm -rf "$some_dir"

    # --extra-mount targeting /workspace collides with the clone; rejected
    # before boot (--no-devcontainer avoids remote devcontainer discovery).
    if coop_fails up --git-repo "$repo_url" --name "$gr_instance" \
            --extra-mount "$data_dir:/workspace" --no-agents --no-devcontainer; then
        if echo "$HARNESS_ERR" | grep -q "/workspace"; then
            pass "up --git-repo rejects --extra-mount at /workspace"
        else
            fail "up --git-repo rejects --extra-mount at /workspace" "stderr: $HARNESS_ERR"
        fi
    else
        fail "up --git-repo rejects --extra-mount at /workspace" \
            "command unexpectedly succeeded"
    fi

    # Clone happy path plus an extra data mount in a single boot. The
    # git-repo + extra-mount combination is the workspace-sync path that
    # regressed on Firecracker before this change.
    if coop up --git-repo "$repo_url" --name "$gr_instance" \
            --extra-mount "$data_dir:/data" --no-agents --no-devcontainer; then
        STARTED_INSTANCES+=("$gr_instance")
        pass "up --git-repo creates an instance"
    else
        fail "up --git-repo creates an instance" "exit code: $? stderr: $HARNESS_ERR"
        rm -rf "$data_dir"
        return
    fi

    GUEST_INSTANCE="$gr_instance"

    if guest_exec test -d /workspace/.git; then
        pass "up --git-repo clones the repository into /workspace"
    else
        fail "up --git-repo clones the repository into /workspace" \
            "no .git at /workspace; stderr: $(guest_stderr)"
    fi

    local head
    if head=$(guest_exec git -C /workspace rev-parse HEAD); then
        if [[ -n "$head" ]]; then
            pass "cloned repository has a valid HEAD"
        else
            fail "cloned repository has a valid HEAD" "empty rev-parse output"
        fi
    else
        fail "cloned repository has a valid HEAD" "git rev-parse failed: $(guest_stderr)"
    fi

    local marker
    if marker=$(guest_exec cat /data/marker.txt); then
        if [[ "$marker" == "extra-mount-marker" ]]; then
            pass "up --git-repo also syncs --extra-mount data"
        else
            fail "up --git-repo also syncs --extra-mount data" "got: $marker"
        fi
    else
        fail "up --git-repo also syncs --extra-mount data" "file not found at /data"
    fi

    unset GUEST_INSTANCE

    # Re-running while the instance is up reuses it: exit 0, no new instance.
    local pre_list post_list
    pre_list=$("$BINARY" list 2>/dev/null | sort)
    if coop up --git-repo "$repo_url" --name "$gr_instance" --no-devcontainer; then
        pass "up --git-repo re-run exits 0 while running"
    else
        fail "up --git-repo re-run exits 0 while running" "exit code: $? stderr: $HARNESS_ERR"
    fi
    post_list=$("$BINARY" list 2>/dev/null | sort)
    if [[ "$pre_list" == "$post_list" ]]; then
        pass "up --git-repo reuses the running instance"
    else
        fail "up --git-repo reuses the running instance" \
            "list changed: $(diff <(echo "$pre_list") <(echo "$post_list"))"
    fi

    coop destroy "$gr_instance" 2>/dev/null || true
    untrack_instance "$gr_instance"
    rm -rf "$data_dir"
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

    # Initialise a git repo so the .git/ inclusion path (issue #91) is
    # exercised end-to-end. Use a local-only commit; no remote is set up.
    (
        cd "$ws_tmpdir" && \
        git init --quiet --initial-branch=main && \
        git -c user.email=ci@test -c user.name=CI commit --allow-empty --quiet -m "init"
    ) || fail "set up git repo in workspace tmpdir" "git init/commit failed"

    # Create instance for the workspace.
    local args=(up "$ws_tmpdir" --name "$ws_instance" --no-agents --no-devcontainer)
    if coop "${args[@]}"; then
        STARTED_INSTANCES+=("$ws_instance")
        pass "up with workspace exits 0"
    else
        fail "up with workspace exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        return
    fi

    # Verify files were synced to guest
    local file_content
    GUEST_INSTANCE="$ws_instance"

    if file_content=$(guest_exec cat /workspace/hello.txt); then
        if [[ "$file_content" == "workspace-test-content" ]]; then
            pass "workspace file synced to guest"
        else
            fail "workspace file synced to guest" "got: $file_content"
        fi
    else
        fail "workspace file synced to guest" "file not found in guest"
    fi

    # Verify nested directory was synced
    if file_content=$(guest_exec cat /workspace/subdir/nested.txt); then
        if echo "$file_content" | grep -q "nested"; then
            pass "nested workspace files synced"
        else
            fail "nested workspace files synced" "got: $file_content"
        fi
    else
        fail "nested workspace files synced" "nested file not found"
    fi

    # Issue #91: `.git/` is now included by default. Verify the guest has
    # a usable git workspace, not just the HEAD file.
    if guest_exec test -f /workspace/.git/HEAD; then
        pass ".git/ directory transferred to guest"
    else
        fail ".git/ directory transferred to guest" "/workspace/.git/HEAD missing"
    fi

    if guest_exec git -C /workspace rev-parse HEAD >/dev/null; then
        pass "guest git repo is functional after sync"
    else
        fail "guest git repo is functional after sync" \
            "git rev-parse HEAD failed: $(guest_stderr)"
    fi

    # Modify file in guest, then pull
    coop_exec sh -c 'echo modified-in-guest > /workspace/hello.txt' || true

    local pull_dir
    pull_dir=$(mktemp -d)
    if coop pull "$ws_instance" --force --dir "$pull_dir"; then
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
    if coop push "$ws_instance" --force --dir "$ws_tmpdir"; then
        pass "push exits 0"

        local pushed_content
        pushed_content=$(guest_exec cat /workspace/hello.txt)
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

# macOS hosts: `coop up --copy` streams the workspace through host tar, whose
# default copyfile(3) metadata path emits AppleDouble `._*` sidecar entries
# for xattrs/resource forks. Those land in the Linux guest as ordinary files
# (they can break Git's pack/ref discovery inside .git/) and get pulled back
# to the host. coop sets COPYFILE_DISABLE=1 on host-side tar creation to
# suppress them; this phase reproduces the round-trip and pins the fix.
# Skipped off macOS, where tar has no copyfile path.
test_appledouble_sidecars() {
    echo ""
    echo "=== Phase: appledouble sidecars (macOS --copy round-trip) ==="

    if [[ "$(uname -s)" != "Darwin" ]]; then
        skip "copy/pull round-trip creates no AppleDouble sidecars" "macOS-only"
        return
    fi

    local ad_instance="${INSTANCE}-appledouble"
    local repo_dir="$tmpdir/appledouble-repo"
    local pull_dir="$tmpdir/appledouble-pull"

    mkdir -p "$repo_dir"
    (
        cd "$repo_dir" && \
        git init --quiet --initial-branch=main && \
        echo "hi" > AGENTS.md && \
        git add . && \
        git -c user.email=ci@test -c user.name=CI commit --quiet -m "init" && \
        xattr -w com.example.coop-test value AGENTS.md
    ) || {
        fail "set up xattr-bearing git repo" "git init/commit/xattr failed"
        return
    }

    local sidecars
    sidecars=$(find "$repo_dir" -name '._*')
    if [[ -n "$sidecars" ]]; then
        fail "source fixture is sidecar-free" "found: $sidecars"
        return
    fi

    if coop up "$repo_dir" --name "$ad_instance" --copy \
        --no-agents --no-prompt --no-devcontainer; then
        STARTED_INSTANCES+=("$ad_instance")
        pass "up --copy with xattr-bearing repo exits 0"
    else
        fail "up --copy with xattr-bearing repo exits 0" "stderr: $HARNESS_ERR"
        return
    fi

    # The push side is where host tar creation runs, so check the guest
    # first: sidecars appearing here mean COPYFILE_DISABLE didn't apply.
    GUEST_INSTANCE="$ad_instance"
    if sidecars=$(coop_exec sh -c 'find /workspace -name "._*"'); then
        if [[ -z "$sidecars" ]]; then
            pass "guest workspace has no AppleDouble sidecars"
        else
            fail "guest workspace has no AppleDouble sidecars" "found: $sidecars"
        fi
    else
        fail "guest workspace has no AppleDouble sidecars" \
            "find failed: $(guest_stderr)"
    fi
    unset GUEST_INSTANCE

    mkdir -p "$pull_dir"
    if coop pull "$ad_instance" --force --dir "$pull_dir"; then
        sidecars=$(find "$pull_dir" -name '._*')
        if [[ -z "$sidecars" ]]; then
            pass "pulled checkout has no AppleDouble sidecars"
        else
            fail "pulled checkout has no AppleDouble sidecars" "found: $sidecars"
        fi
    else
        fail "pull for sidecar check exits 0" "stderr: $HARNESS_ERR"
    fi

    coop destroy "$ad_instance" 2>/dev/null || true
    untrack_instance "$ad_instance"
}

# ── Multi-instance tests (--full only) ────────────────────────

test_multi_instance() {
    echo ""
    echo "=== Phase: multi-instance ==="

    local inst_a="${INSTANCE}-a"
    local inst_b="${INSTANCE}-b"
    local ws_a="$tmpdir/${inst_a}-ws"
    local ws_b="$tmpdir/${inst_b}-ws"
    mkdir -p "$ws_a" "$ws_b"

    # Create two project instances
    if coop up "$ws_a" --name "$inst_a" --no-agents --no-devcontainer; then
        STARTED_INSTANCES+=("$inst_a")
        pass "up creates instance A ($inst_a)"
    else
        fail "up creates instance A ($inst_a)" "exit code: $?"
        return
    fi

    if coop up "$ws_b" --name "$inst_b" --no-agents --no-devcontainer; then
        STARTED_INSTANCES+=("$inst_b")
        pass "up creates instance B ($inst_b)"
    else
        fail "up creates instance B ($inst_b)" "exit code: $?"
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
    if coop_fails shell -- echo "should-not-work"; then
        pass "shell rejects auto-resolve with multiple running"
        if echo "$HARNESS_ERR" | grep -qi "multiple"; then
            pass "error mentions multiple running instances"
        else
            fail "error mentions multiple running instances" "stderr: $HARNESS_ERR"
        fi
    else
        fail "shell rejects auto-resolve with multiple running" "should have failed"
    fi

    # Explicit name should still work with multiple instances
    if RUST_LOG=off "$BINARY" shell "$inst_a" -- echo "explicit-ok" 2>/dev/null | grep -q "explicit-ok"; then
        pass "shell with explicit name works among multiple"
    else
        fail "shell with explicit name works among multiple"
    fi

    # Both should be independently accessible via SSH
    GUEST_INSTANCE="$inst_a"
    local hostname_a hostname_b
    hostname_a=$(guest_exec hostname) || hostname_a=""
    GUEST_INSTANCE="$inst_b"
    hostname_b=$(guest_exec hostname) || hostname_b=""
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

    # Create an instance from the named image
    local inst_name="${INSTANCE}-img"
    local img_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$img_ws"
    if coop up "$img_ws" --name "$inst_name" --no-agents --no-devcontainer --image "$img_name"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up --image $img_name exits 0"
    else
        fail "up --image $img_name exits 0" "exit code: $?"
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
    hostname=$(guest_exec hostname) || hostname=""
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

    # Create instance from custom image
    local inst_name="${INSTANCE}-custom"
    local custom_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$custom_ws"
    if "$BINARY" --config "$cfg_file" up "$custom_ws" --name "$inst_name" --no-agents --no-devcontainer --image "$custom_img" 2>"$tmpdir/stderr"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up with custom profile image exits 0"
    else
        fail "up with custom profile image exits 0" "exit code: $?"
        rm -rf "$cfg_dir"
        return
    fi

    # Verify custom profile effects in guest
    GUEST_INSTANCE="$inst_name"
    local marker
    marker=$(guest_exec cat /etc/custom-profile-installed) || marker=""
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

    # Build-on-demand via up. The sorted profile list derives the
    # node-python image name. These cover apt-only (python) and
    # pre-install script (node/NodeSource).
    local img_name="node-python"
    coop images --delete "$img_name" 2>/dev/null || true

    local inst_name="${INSTANCE}-prof"
    local prof_ws
    prof_ws=$(mktemp -d)
    echo "profile workspace" > "$prof_ws/README.md"
    if coop up "$prof_ws" --name "$inst_name" --profile python,node --no-agents; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up --profile python,node exits 0"
    else
        fail "up --profile python,node exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        rm -rf "$prof_ws"
        return
    fi

    if coop images; then
        if echo "$HARNESS_OUT" | grep -q "^${img_name} "; then
            pass "derived profile image is listed"
        else
            fail "derived profile image is listed" "output: $HARNESS_OUT"
        fi
    else
        fail "images exits 0 after up --profile" "exit code: $?"
    fi

    GUEST_INSTANCE="$inst_name"

    # Verify python profile
    local py_ver
    if py_ver=$(guest_exec python3 --version); then
        pass "python3 installed ($py_ver)"
    else
        fail "python3 installed (profile: python)"
    fi

    if guest_exec python3 -c 'import venv' >/dev/null; then
        pass "python3-venv available"
    else
        fail "python3-venv available"
    fi

    # Verify node profile
    local node_ver
    if node_ver=$(guest_exec node --version); then
        pass "node installed ($node_ver)"
    else
        fail "node installed (profile: node)"
    fi

    if guest_exec npm --version >/dev/null; then
        pass "npm installed"
    else
        fail "npm installed"
    fi

    unset GUEST_INSTANCE

    # Clean up
    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    coop images --delete "$img_name" 2>/dev/null || true
    rm -rf "$prof_ws"
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

    # Create instance with project mount at /workspace
    if coop up "$mount_dir" --name "$mount_instance" --no-agents --no-devcontainer --mount; then
        STARTED_INSTANCES+=("$mount_instance")
        pass "up --mount exits 0"
    else
        fail "up --mount exits 0" "exit code: $?"
        echo "stderr: $HARNESS_ERR"
        rm -rf "$mount_dir"
        return
    fi

    GUEST_INSTANCE="$mount_instance"

    # Verify host files are visible in guest at /workspace
    local content
    if content=$(guest_exec cat /workspace/sentinel.txt); then
        if [[ "$content" == "mount-test-content" ]]; then
            pass "mounted file readable in guest"
        else
            fail "mounted file readable in guest" "got: $content"
        fi
    else
        fail "mounted file readable in guest" "file not found"
    fi

    # Verify nested directory
    if content=$(guest_exec cat /workspace/subdir/deep.txt); then
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
        if guest_exec sh -c 'echo "written-by-guest" > /workspace/from-guest.txt'; then
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
        if content=$(guest_exec cat /workspace/live.txt); then
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

    local project_dir="$tmpdir/${mount_instance}-ws"
    mkdir -p "$project_dir"
    # Mount with explicit guest path
    if coop up "$project_dir" --name "$mount_instance" --no-agents --no-devcontainer --extra-mount "$mount_dir:/data/project"; then
        STARTED_INSTANCES+=("$mount_instance")
        pass "up with --extra-mount host:guest exits 0"
    else
        fail "up with --extra-mount host:guest exits 0" "exit code: $?"
        rm -rf "$mount_dir"
        return
    fi

    GUEST_INSTANCE="$mount_instance"

    local content
    if content=$(guest_exec cat /data/project/marker.txt); then
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

# ── Port forward tests (--full only) ─────────────────────────

test_port_forwards() {
    echo ""
    echo "=== Phase: port forwards ==="

    local fwd_instance="${INSTANCE}-fwd"
    # Pick a likely-free host port high in the ephemeral range to avoid
    # clashes with anything the developer is already running.
    local host_port=14573
    local guest_port=8765
    local content_file
    content_file=$(mktemp)
    echo "forward-test-payload" > "$content_file"
    local fwd_ws="$tmpdir/${fwd_instance}-ws"
    mkdir -p "$fwd_ws"

    if coop up "$fwd_ws" --name "$fwd_instance" --no-agents --no-devcontainer --forward-port "${guest_port}:${host_port}"; then
        STARTED_INSTANCES+=("$fwd_instance")
        pass "up with --forward-port exits 0"
    else
        fail "up with --forward-port exits 0" "exit code: $?"
        rm -f "$content_file"
        return
    fi

    GUEST_INSTANCE="$fwd_instance"

    # Run a one-shot HTTP listener in the guest. The Firecracker CI rootfs
    # ships no netcat variant (only socat among net listeners), so we use
    # python3, which is present on both Firecracker (pulled in via
    # python3-boto3) and Lima (Ubuntu cloud image default). The listener
    # script is embedded as a heredoc; coop shell single-quotes each arg,
    # so newlines pass through to the remote sh -c untouched.
    local payload
    payload=$(cat "$content_file")
    guest_exec sh -c "cat > /tmp/fwd.py <<'PYEOF'
import socket, sys
port = int(sys.argv[1])
body = sys.argv[2].encode()
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', port))
s.listen(1)
c, _ = s.accept()
c.recv(65536)
c.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n%s' % (len(body), body))
c.close()
PYEOF
nohup python3 /tmp/fwd.py ${guest_port} ${payload} > /tmp/fwd.log 2>&1 &" || true
    sleep 1

    if curl -fsS --max-time 3 "http://127.0.0.1:${host_port}/" > "$content_file.got"; then
        local got
        got=$(cat "$content_file.got")
        if [[ "$got" == "$payload" ]]; then
            pass "host curl reaches guest via forwarded port"
        else
            fail "host curl reaches guest via forwarded port" "got: $got"
        fi
    else
        fail "host curl reaches guest via forwarded port" "curl failed"
    fi

    # Collision: starting another forward to the same host port should error.
    # The coop() wrapper captures the binary's stderr into $HARNESS_ERR — an
    # outer `2>` redirect here would be shadowed by the wrapper's internal
    # redirect and silently capture nothing.
    local fwd_instance2="${INSTANCE}-fwd2"
    local fwd_ws2="$tmpdir/${fwd_instance2}-ws"
    mkdir -p "$fwd_ws2"
    if coop up "$fwd_ws2" --name "$fwd_instance2" --no-agents --no-devcontainer --forward-port "9999:${host_port}"; then
        STARTED_INSTANCES+=("$fwd_instance2")
        fail "collision detection rejects in-use host port" "start unexpectedly succeeded"
        coop destroy "$fwd_instance2" 2>/dev/null || true
        untrack_instance "$fwd_instance2"
    else
        if grep -q "already in use" <<<"$HARNESS_ERR"; then
            pass "collision detection rejects in-use host port"
        else
            fail "collision detection rejects in-use host port" "stderr: $HARNESS_ERR"
        fi
    fi

    unset GUEST_INSTANCE

    coop stop "$fwd_instance" 2>/dev/null || true

    # After stop, the host port must be free again so the next test can rebind.
    if (exec 3<>/dev/tcp/127.0.0.1/"$host_port") 2>/dev/null; then
        exec 3<&-
        exec 3>&-
        fail "host port released after stop" "still listening on $host_port"
    else
        pass "host port released after stop"
    fi

    # Restart WITHOUT re-passing --forward-port. The forward set persisted to
    # forwards.json at `up` time must be reloaded and respawned, proving the
    # tunnel survives a stop/start cycle without the caller restating it.
    # Already tracked from the initial `up`; `coop stop` does not untrack, so
    # the instance stays in STARTED_INSTANCES across the restart.
    if coop start "$fwd_instance" --no-agents; then
        pass "restart without --forward-port exits 0"
    else
        fail "restart without --forward-port exits 0" "exit code: $? stderr: $HARNESS_ERR"
        coop destroy "$fwd_instance" 2>/dev/null || true
        untrack_instance "$fwd_instance"
        rm -f "$content_file" "$content_file.got"
        return
    fi

    GUEST_INSTANCE="$fwd_instance"

    # The previous listener died with the guest on stop; re-launch it so there
    # is something to reach through the respawned tunnel.
    guest_exec sh -c "cat > /tmp/fwd.py <<'PYEOF'
import socket, sys
port = int(sys.argv[1])
body = sys.argv[2].encode()
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('127.0.0.1', port))
s.listen(1)
c, _ = s.accept()
c.recv(65536)
c.sendall(b'HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n%s' % (len(body), body))
c.close()
PYEOF
nohup python3 /tmp/fwd.py ${guest_port} ${payload} > /tmp/fwd.log 2>&1 &" || true
    sleep 1

    if curl -fsS --max-time 3 "http://127.0.0.1:${host_port}/" > "$content_file.got"; then
        local got_restart
        got_restart=$(cat "$content_file.got")
        if [[ "$got_restart" == "$payload" ]]; then
            pass "forwarded port reachable again after restart"
        else
            fail "forwarded port reachable again after restart" "got: $got_restart"
        fi
    else
        fail "forwarded port reachable again after restart" "curl failed"
    fi

    unset GUEST_INSTANCE

    coop destroy "$fwd_instance" 2>/dev/null || true
    untrack_instance "$fwd_instance"
    rm -f "$content_file" "$content_file.got"
}

# ── Destroy --all test (--full only) ──────────────────────────

test_destroy_all() {
    echo ""
    echo "=== Phase: destroy --all ==="

    # Start two throwaway instances
    local inst_x="${INSTANCE}-x"
    local inst_y="${INSTANCE}-y"
    local ws_x="$tmpdir/${inst_x}-ws"
    local ws_y="$tmpdir/${inst_y}-ws"
    mkdir -p "$ws_x" "$ws_y"

    if ! coop up "$ws_x" --name "$inst_x" --no-agents --no-devcontainer; then
        fail "up instance for destroy --all" "exit code: $?"
        return
    fi
    STARTED_INSTANCES+=("$inst_x")

    if ! coop up "$ws_y" --name "$inst_y" --no-agents --no-devcontainer; then
        fail "up second instance for destroy --all" "exit code: $?"
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

    local mp_ws="$tmpdir/${mp_instance}-ws"
    mkdir -p "$mp_ws"

    # Create the instance WITH bootstrap. The stub claude handles
    # `marketplace add` calls. Unset tokens to avoid auth steps.
    if env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" up "$mp_ws" --name "$mp_instance" --image "$mp_img" --no-devcontainer 2>"$tmpdir/stderr"; then
        STARTED_INSTANCES+=("$mp_instance")
        pass "up with local marketplace exits 0"
    else
        local boot_err
        boot_err=$(cat "$tmpdir/stderr")
        fail "up with local marketplace exits 0" "stderr: $boot_err"
        env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY "$BINARY" --config "$cfg_file" images --delete "$mp_img" 2>/dev/null || true
        rm -rf "$mp_dir" "$cfg_dir"
        return
    fi

    GUEST_INSTANCE="$mp_instance"

    # Verify the marketplace directory was copied to the guest. Local
    # marketplaces are copied into a per-tool subdir (`claude/` here, since this
    # is a `[claude]` marketplace) so the two agents' same-named marketplaces
    # cannot collide. Use $HOME explicitly — tilde would expand on the test
    # host, not the guest.
    local mp_base="\$HOME/.coop/marketplaces/claude/$mp_name"

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
    claude_log=$(guest_exec cat /tmp/claude-calls.log) || claude_log=""

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

        if echo "$claude_log" | grep -q "coop/marketplaces/claude/$mp_name"; then
            pass "marketplace add uses guest path (not host path)"
        else
            fail "marketplace add uses guest path (not host path)" "log: '$claude_log'"
        fi
    fi

    # Verify `coop claude --help` invokes the binary at CLAUDE_BIN path.
    # The stub claude will log the args, so we can verify it was called.
    guest_exec truncate -s 0 /tmp/claude-calls.log || true
    # coop claude uses run_interactive which needs a PTY — use exec instead
    # to verify the binary path is correct by invoking it directly.
    env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY RUST_LOG=off \
        "$BINARY" --config "$cfg_file" exec "$mp_instance" -- \
        /home/ubuntu/.local/bin/claude --help >/dev/null 2>/dev/null || true

    local post_log
    post_log=$(guest_exec cat /tmp/claude-calls.log) || post_log=""
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

    # ── 1. First up: allowlisted files copied to guest ──

    local cd_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$cd_ws"
    if cs up "$cd_ws" --name "$inst_name" --no-devcontainer; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up with config_dir exits 0"
    else
        fail "up with config_dir exits 0" "exit code: $? stderr: $HARNESS_ERR"
        rm -r "$cs_dir"
        return
    fi

    GUEST_INSTANCE="$inst_name"

    local guest_claude
    guest_claude=$(guest_exec cat /home/ubuntu/.claude/CLAUDE.md) || guest_claude=""
    if echo "$guest_claude" | grep -q "host-claude-marker"; then
        pass "CLAUDE.md copied to guest ~/.claude/CLAUDE.md"
    else
        fail "CLAUDE.md copied to guest ~/.claude/CLAUDE.md" "got: $guest_claude"
    fi

    local guest_rule
    guest_rule=$(guest_exec cat /home/ubuntu/.claude/rules/safety.md) || guest_rule=""
    if echo "$guest_rule" | grep -q "host-rule-marker"; then
        pass "rules file copied to guest ~/.claude/rules/"
    else
        fail "rules file copied to guest ~/.claude/rules/" "got: $guest_rule"
    fi

    local guest_cmd
    guest_cmd=$(guest_exec cat /home/ubuntu/.claude/commands/deploy.md) || guest_cmd=""
    if echo "$guest_cmd" | grep -q "host-cmd-marker"; then
        pass "commands file copied to guest ~/.claude/commands/"
    else
        fail "commands file copied to guest ~/.claude/commands/" "got: $guest_cmd"
    fi

    # Verify non-allowlisted files are NOT copied.
    # The guest may have its own settings.json from Claude Code init,
    # so check for the specific marker content from our test file.
    local guest_settings
    guest_settings=$(guest_exec cat /home/ubuntu/.claude/settings.json) || guest_settings=""
    if echo "$guest_settings" | grep -q "should-not-copy"; then
        fail "settings.json NOT copied (allowlist)" "host content leaked to guest"
    else
        pass "settings.json NOT copied (allowlist)"
    fi

    # ── 2. Modify guest CLAUDE.md, restart → re-synced from host ──

    guest_exec sh -c "'echo guest-modified > /home/ubuntu/.claude/CLAUDE.md'" || true
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
    guest_claude=$(guest_exec cat /home/ubuntu/.claude/CLAUDE.md) || guest_claude=""
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

# ── [guest_env] config block end-to-end (--full only) ─────────

# `test_guest_environment` covers the `--env` CLI path against the shared
# primary instance. This phase covers the complementary `[guest_env]`
# config-file path, plus the literal-over-forwarded precedence (and its
# WARN) from `prepare_env_forwarding` in src/backend.rs. It owns a dedicated
# config file and instance so it doesn't perturb the primary instance.
test_guest_env_config() {
    echo ""
    echo "=== Phase: [guest_env] config block ==="

    local inst_name="${INSTANCE}-genv"

    local ge_dir
    ge_dir=$(mktemp -d)
    local cfg_file="$ge_dir/config.toml"

    # `COOP_TEST_GUEST_ENV_CONFIG` is a pure config literal.
    # `COOP_TEST_GUEST_ENV_PRECEDENCE` is also listed in `claude.env_forward`
    # and exported on the host below, so the literal must override the
    # forwarded host value (and a WARN must be emitted on the collision).
    # `post_start` forces the up-time SSH session (and thus
    # `prepare_env_forwarding`, which emits the precedence WARN) to run even
    # under `--no-agents`, which otherwise skips all session work. `true` is
    # a no-op. Without it the WARN is never produced during `up`.
    cat > "$cfg_file" <<CFGEOF
post_start = "true"

[claude]
github = "off"
env_forward = ["COOP_TEST_GUEST_ENV_PRECEDENCE"]

[guest_env]
COOP_TEST_GUEST_ENV_CONFIG = "from-config-file"
COOP_TEST_GUEST_ENV_PRECEDENCE = "literal-wins"
CFGEOF

    ge() {
        local rc=0
        HARNESS_OUT=$(env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY \
            COOP_TEST_GUEST_ENV_PRECEDENCE="forwarded-loses" \
            "$BINARY" --config "$cfg_file" "$@" 2>"$tmpdir/stderr") || rc=$?
        HARNESS_ERR=$(cat "$tmpdir/stderr")
        return $rc
    }

    # `coop shell` must run with this config file: config-block `guest_env`
    # literals are re-derived from `config.toml` on each command (only CLI
    # `--env` and devcontainer entries are persisted to `guest_env.json`).
    # A bare `coop shell` would load the default config and never see them.
    # `RUST_LOG=off` keeps tracing out of captured stdout, mirroring
    # `guest_exec`; stderr lands in the shared guest_stderr file.
    ge_exec() {
        RUST_LOG=off env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY \
            COOP_TEST_GUEST_ENV_PRECEDENCE="forwarded-loses" \
            "$BINARY" --config "$cfg_file" shell "$inst_name" -- "$@" \
            2>"$tmpdir/guest_stderr"
    }

    local ge_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$ge_ws"
    if ge up "$ge_ws" --name "$inst_name" --no-agents --no-devcontainer; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up with [guest_env] config exits 0"
    else
        fail "up with [guest_env] config exits 0" "exit code: $? stderr: $HARNESS_ERR"
        rm -r "$ge_dir"
        return
    fi

    # The override WARN is emitted during `up` (stderr, INFO default level).
    if echo "$HARNESS_ERR" | grep -q "COOP_TEST_GUEST_ENV_PRECEDENCE.*overrides"; then
        pass "guest_env literal override logs a WARN"
    else
        fail "guest_env literal override logs a WARN" "stderr: $HARNESS_ERR"
    fi

    # The pure config literal must reach the guest process environment.
    local cfg_val
    if cfg_val=$(ge_exec printenv COOP_TEST_GUEST_ENV_CONFIG); then
        if [[ "$cfg_val" == "from-config-file" ]]; then
            pass "guest_env from config block reaches the guest"
        else
            fail "guest_env from config block reaches the guest" "got: $cfg_val"
        fi
    else
        fail "guest_env from config block reaches the guest" \
            "printenv failed; stderr: $(guest_stderr)"
    fi

    # The literal must win over the forwarded host value of the same name.
    local prec_val
    if prec_val=$(ge_exec printenv COOP_TEST_GUEST_ENV_PRECEDENCE); then
        if [[ "$prec_val" == "literal-wins" ]]; then
            pass "guest_env literal overrides forwarded value"
        else
            fail "guest_env literal overrides forwarded value" "got: $prec_val"
        fi
    else
        fail "guest_env literal overrides forwarded value" \
            "printenv failed; stderr: $(guest_stderr)"
    fi

    ge stop "$inst_name" 2>/dev/null || true
    ge destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    rm -r "$ge_dir"
}

# ── coop model local/remote end-to-end (--full only) ──────────

# Exercises `coop model <vm> local|remote`: the local-model wiring must
# land in the guest's Claude settings.json (env block) and Codex
# config.toml (provider block) with the host URL rewritten to the
# backend's guest-visible address, and `remote` must drop both. No real
# model server is needed — only the materialized config is asserted.
test_local_model() {
    echo ""
    echo "=== Phase: coop model local/remote (--full) ==="

    local inst_name="${INSTANCE}-model"
    local lm_dir
    lm_dir=$(mktemp -d)
    local cfg_file="$lm_dir/config.toml"
    local claude_model="coop-test-claude-model"
    local codex_model="coop-test-codex-model"

    cat > "$cfg_file" <<CFGEOF
[claude]
github = "off"

[claude.local_model]
host_url = "http://localhost:11434"
model = "$claude_model"

[codex.local_model]
host_url = "http://localhost:11434/v1/"
model = "$codex_model"
CFGEOF

    lm() {
        local rc=0
        HARNESS_OUT=$(env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY -u OPENAI_API_KEY \
            "$BINARY" --config "$cfg_file" "$@" 2>"$tmpdir/stderr") || rc=$?
        HARNESS_ERR=$(cat "$tmpdir/stderr")
        return $rc
    }
    lm_exec() {
        RUST_LOG=off env -u GITHUB_TOKEN -u ANTHROPIC_API_KEY -u OPENAI_API_KEY \
            "$BINARY" --config "$cfg_file" exec "$inst_name" -- "$@" \
            2>"$tmpdir/guest_stderr"
    }

    local lm_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$lm_ws"
    if lm up "$lm_ws" --name "$inst_name" --no-agents --no-devcontainer; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up for model test exits 0"
    else
        fail "up for model test exits 0" "exit: $? stderr: $HARNESS_ERR"
        rm -r "$lm_dir"
        return
    fi

    # Default (no model.json) must report remote.
    if lm model "$inst_name" && echo "$HARNESS_OUT" | grep -qi "Mode:.*remote"; then
        pass "model status defaults to remote"
    else
        fail "model status defaults to remote" "out: $HARNESS_OUT stderr: $HARNESS_ERR"
    fi

    # Switch to local — materializes guest config against the running VM.
    if lm model "$inst_name" local; then
        pass "model local exits 0"
    else
        fail "model local exits 0" "exit: $? stderr: $HARNESS_ERR"
    fi

    # The VM is running, so the change applies live: assert the no-restart
    # fast-path report line rather than the "applies on next start" message.
    if echo "$HARNESS_OUT" | grep -qi "no restart needed"; then
        pass "model local reports no-restart fast path"
    else
        fail "model local reports no-restart fast path" "out: $HARNESS_OUT"
    fi

    # Claude settings.json carries the env block with a rewritten host. The
    # two cache-buster keys (CLAUDE_CODE_ATTRIBUTION_HEADER,
    # CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS) disable the per-request prompt
    # mutators that would otherwise re-prefill a local backend's KV cache on
    # every tool call; they exist only in the local env block, so assert they
    # reach the guest — the unit test pins only the in-process map shape.
    local settings
    if settings=$(lm_exec cat ./.claude/settings.json); then
        if echo "$settings" | grep -q "$claude_model" \
            && echo "$settings" | grep -q "ANTHROPIC_BASE_URL" \
            && echo "$settings" | grep -q "CLAUDE_CODE_ATTRIBUTION_HEADER" \
            && echo "$settings" | grep -q "CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS" \
            && ! echo "$settings" | grep -q "localhost"; then
            pass "claude settings.json has rewritten local env block"
        else
            fail "claude settings.json has rewritten local env block" "got: $settings"
        fi
    else
        fail "claude settings.json has rewritten local env block" \
            "cat failed; stderr: $(guest_stderr)"
    fi

    # Codex config.toml carries the provider block with a rewritten host.
    local codex_cfg
    if codex_cfg=$(lm_exec cat ./.codex/config.toml); then
        if echo "$codex_cfg" | grep -q "coop_local" \
            && echo "$codex_cfg" | grep -q "responses" \
            && echo "$codex_cfg" | grep -q "$codex_model" \
            && ! echo "$codex_cfg" | grep -q "localhost"; then
            pass "codex config.toml has rewritten local provider block"
        else
            fail "codex config.toml has rewritten local provider block" "got: $codex_cfg"
        fi
    else
        fail "codex config.toml has rewritten local provider block" \
            "cat failed; stderr: $(guest_stderr)"
    fi

    # Status now reports local with the model.
    if lm model "$inst_name" && echo "$HARNESS_OUT" | grep -qi "Mode:.*local" \
        && echo "$HARNESS_OUT" | grep -q "$claude_model"; then
        pass "model status reports local with endpoint"
    else
        fail "model status reports local with endpoint" "out: $HARNESS_OUT"
    fi

    # The selection must survive a stop/start: boot-time bootstrap re-applies
    # the persisted local mode.
    lm stop "$inst_name" >/dev/null 2>&1 || true
    if lm start "$inst_name" >/dev/null 2>&1; then
        if settings=$(lm_exec cat ./.claude/settings.json) \
            && echo "$settings" | grep -q "ANTHROPIC_BASE_URL"; then
            pass "local mode persists across stop/start"
        else
            fail "local mode persists across stop/start" "got: $settings"
        fi
    else
        fail "local mode persists across stop/start" "restart failed: $HARNESS_ERR"
    fi

    # Switch back to remote — both materializations must be dropped.
    if lm model "$inst_name" remote; then
        pass "model remote exits 0"
    else
        fail "model remote exits 0" "exit: $? stderr: $HARNESS_ERR"
    fi

    if settings=$(lm_exec cat ./.claude/settings.json) \
        && ! echo "$settings" | grep -q "ANTHROPIC_BASE_URL"; then
        pass "remote drops claude env block"
    else
        fail "remote drops claude env block" "got: $settings"
    fi

    if codex_cfg=$(lm_exec cat ./.codex/config.toml 2>/dev/null) \
        && echo "$codex_cfg" | grep -q "coop_local"; then
        fail "remote drops codex provider block" "stale provider: $codex_cfg"
    else
        pass "remote drops codex provider block"
    fi

    lm stop "$inst_name" 2>/dev/null || true
    lm destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    rm -r "$lm_dir"
}

# ── Credential-injecting proxy test (issue #411, --full only) ─────────
#
# Brings a VM up in proxy mode (remote model mode + `[proxy.*]` configured) and
# asserts the non-exposure property end-to-end on both backends. There is no
# mock upstream: the proxy pins TLS to the real hosts (a security property), so
# a local mock cannot present a trusted cert. Instead the checks are hermetic
# and need no upstream — the injection-into-the-header logic is covered by the
# `coop-proxy` unit tests + `gate.rs`. Here we assert what only a live VM can:
#   - the guest is wired at the proxy (Codex `config.toml`, Claude
#     `settings.json`) and holds only the capability token;
#   - the raw keys are suppressed even when set on the host (crown jewel), and
#     Codex's `auth.json` is not staged;
#   - the proxy is reachable on host loopback and enforces the capability gate;
#   - `stop` tears the proxy down (the host port stops answering).
test_proxy() {
    echo ""
    echo "=== Phase: credential-injecting proxy (--full) ==="

    if ! command -v curl >/dev/null 2>&1; then
        skip "credential-proxy test (curl not available on host)"
        return
    fi
    # The proxy needs the `coop-proxy` binary next to `coop`. It is not a
    # default workspace member (it needs cmake for aws-lc-rs), so a plain
    # `cargo build` / older deploy may not have it; skip rather than fail the
    # fail-closed `up` when it's absent.
    if [[ ! -x "$(dirname "$BINARY")/coop-proxy" ]]; then
        skip "credential-proxy test (coop-proxy not built alongside coop)"
        return
    fi

    local inst_name="${INSTANCE}-proxy"
    local px_dir
    px_dir=$(mktemp -d)
    local cfg_file="$px_dir/config.toml"
    # Literal fake credentials: `resolve_cmd_value` returns a non-`cmd:` value
    # verbatim, so the proxy injects these. The test asserts they never reach
    # the guest — bootstrap never runs the agents, so a fake key is harmless.
    local openai_secret="sk-coop-it-openai-FAKE-do-not-use"
    local anthropic_secret="sk-ant-coop-it-FAKE-do-not-use"

    cat >"$cfg_file" <<CFGEOF
[claude]
github = "off"

[proxy.anthropic]
credential = "$anthropic_secret"
auth = "bearer"

[proxy.openai]
credential = "$openai_secret"
auth = "bearer"
CFGEOF

    # Unlike the model-test helpers, DO set host OPENAI_API_KEY/ANTHROPIC_API_KEY
    # (sentinels) so the test proves proxy mode SUPPRESSES them rather than
    # merely that they were absent.
    local host_openai="sk-host-openai-sentinel-LEAK"
    local host_anthropic="sk-ant-host-sentinel-LEAK"
    px() {
        local rc=0
        HARNESS_OUT=$(env -u GITHUB_TOKEN \
            OPENAI_API_KEY="$host_openai" ANTHROPIC_API_KEY="$host_anthropic" \
            "$BINARY" --config "$cfg_file" "$@" 2>"$tmpdir/stderr") || rc=$?
        HARNESS_ERR=$(cat "$tmpdir/stderr")
        return $rc
    }
    px_exec() {
        RUST_LOG=off env -u GITHUB_TOKEN \
            OPENAI_API_KEY="$host_openai" ANTHROPIC_API_KEY="$host_anthropic" \
            "$BINARY" --config "$cfg_file" exec "$inst_name" -- "$@" \
            2>"$tmpdir/guest_stderr"
    }

    local px_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$px_ws"
    # Proxy mode is wired during AGENT bootstrap, so this must NOT be --no-agents.
    if px up "$px_ws" --name "$inst_name" --no-devcontainer; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up in proxy mode exits 0"
    else
        fail "up in proxy mode exits 0" "exit: $? stderr: $HARNESS_ERR"
        rm -r "$px_dir"
        return
    fi

    # Pin the user-facing status contract against the same running instance.
    # Literal credentials are deliberately used above, so this also proves the
    # command redacts them rather than leaking either value to stdout.
    if px proxy status --vm "$inst_name"; then
        if echo "$HARNESS_OUT" | grep -q 'anthropic.*default.*bearer.*redacted' \
            && echo "$HARNESS_OUT" | grep -q 'openai.*default.*bearer.*redacted' \
            && ! echo "$HARNESS_OUT" | grep -qF "$anthropic_secret" \
            && ! echo "$HARNESS_OUT" | grep -qF "$openai_secret"; then
            pass "proxy status shows redacted provider defaults"
        else
            fail "proxy status shows redacted provider defaults" "out: $HARNESS_OUT"
        fi
    else
        fail "proxy status --vm exits 0" "stderr: $HARNESS_ERR"
    fi

    # Exercise the persisted per-VM representation used by `proxy setup --vm`.
    # The wizard itself is intentionally interactive; seeding proxy.json keeps
    # the suite noninteractive while testing load, precedence, status, and
    # redaction through the public command.
    local override_secret="sk-ant-coop-it-override-FAKE-do-not-use"
    local proxy_state="$HOME/.coop/instances/$inst_name/proxy.json"
    cat >"$proxy_state" <<STATEEOF
{
  "anthropic": {
    "credential": "$override_secret",
    "auth": "api_key"
  }
}
STATEEOF
    chmod 600 "$proxy_state"
    if px proxy status --vm "$inst_name"; then
        if echo "$HARNESS_OUT" | grep -q 'anthropic.*override.*api_key.*redacted' \
            && echo "$HARNESS_OUT" | grep -q 'openai.*default.*bearer.*redacted' \
            && ! echo "$HARNESS_OUT" | grep -qF "$override_secret"; then
            pass "proxy status resolves and redacts a per-VM override"
        else
            fail "proxy status resolves and redacts a per-VM override" "out: $HARNESS_OUT"
        fi
    else
        fail "proxy status reads a per-VM override" "stderr: $HARNESS_ERR"
    fi

    # ── Codex (OpenAI) guest wiring ──
    local codex_cfg
    if codex_cfg=$(px_exec cat ./.codex/config.toml); then
        if echo "$codex_cfg" | grep -q "coop_local" \
            && echo "$codex_cfg" | grep -q "responses" \
            && echo "$codex_cfg" | grep -Eq "127\.0\.0\.1:[0-9]+/v1" \
            && echo "$codex_cfg" | grep -q "COOP_LOCAL_API_KEY"; then
            pass "codex config.toml points at the proxy (coop_local, responses, /v1)"
        else
            fail "codex config.toml points at the proxy" "got: $codex_cfg"
        fi
        if echo "$codex_cfg" | grep -q "$openai_secret"; then
            fail "openai credential absent from codex config" "leaked into config.toml"
        else
            pass "openai credential absent from codex config"
        fi
    else
        fail "codex config.toml readable" "cat failed; stderr: $(guest_stderr)"
    fi

    # auth.json must not be staged in proxy mode (issue #411 §7).
    if px_exec test -f ./.codex/auth.json; then
        fail "codex auth.json not staged in proxy mode" "auth.json present in guest"
    else
        pass "codex auth.json not staged in proxy mode"
    fi

    # ── Crown jewel: raw keys never enter the guest env ──
    local guest_env
    guest_env=$(px_exec env || true)
    if echo "$guest_env" | grep -q "^COOP_LOCAL_API_KEY="; then
        pass "guest holds the capability token (COOP_LOCAL_API_KEY)"
    else
        fail "guest holds the capability token" "COOP_LOCAL_API_KEY not in guest env"
    fi
    if echo "$guest_env" | grep -q "^OPENAI_API_KEY="; then
        fail "OPENAI_API_KEY suppressed in guest (proxy mode)" "it was forwarded"
    elif echo "$guest_env" | grep -qF "$openai_secret" \
        || echo "$guest_env" | grep -qF "$host_openai"; then
        fail "raw OpenAI key absent from guest env" "a raw key value leaked"
    else
        pass "raw OpenAI key suppressed and absent from guest env"
    fi
    if echo "$guest_env" | grep -q "^ANTHROPIC_API_KEY="; then
        fail "ANTHROPIC_API_KEY suppressed in guest (proxy mode)" "it was forwarded"
    else
        pass "ANTHROPIC_API_KEY suppressed in guest"
    fi

    # ── Claude (Anthropic) guest wiring ──
    local settings
    if settings=$(px_exec cat ./.claude/settings.json); then
        if echo "$settings" | grep -q "ANTHROPIC_BASE_URL" \
            && echo "$settings" | grep -q "ANTHROPIC_AUTH_TOKEN" \
            && ! echo "$settings" | grep -q "$anthropic_secret"; then
            pass "claude settings.json points at the proxy with the capability token"
        else
            fail "claude settings.json points at the proxy" "got: $settings"
        fi
    else
        fail "claude settings.json readable" "cat failed; stderr: $(guest_stderr)"
    fi

    # ── The proxy is reachable on host loopback and enforces the gate ──
    # The proxy binds 127.0.0.1:<port> on the host and is reverse-tunnelled into
    # the guest, so the host exercises the same listener. A request without the
    # token is refused locally with 401. With the token, an allowlisted
    # operation reaches the real host, yielding 502 with no egress or an
    # upstream auth error. Status alone is ambiguous because upstream auth can
    # also return 401/403, so the fixed local bodies identify gate refusals.
    local oai_port cap_token
    oai_port=$(echo "$codex_cfg" | grep -oE '127\.0\.0\.1:[0-9]+' | head -1 | cut -d: -f2 || true)
    cap_token=$(echo "$guest_env" | grep '^COOP_LOCAL_API_KEY=' | cut -d= -f2- || true)
    if [[ -n "$oai_port" ]]; then
        local no_tok tok no_tok_status tok_status response_body
        local no_tok_body_file="$tmpdir/proxy-no-token-body"
        local tok_body_file="$tmpdir/proxy-token-body"
        response_body='{"model":"gpt-4.1","input":"ping"}'
        no_tok_status=$(curl -s --max-time 10 -X POST \
            "http://127.0.0.1:$oai_port/v1/responses" \
            -H "Content-Type: application/json" --data "$response_body" \
            --output "$no_tok_body_file" --write-out '%{http_code}' || true)
        no_tok=$(cat "$no_tok_body_file" 2>/dev/null || true)
        tok_status=$(curl -s --max-time 15 -X POST \
            "http://127.0.0.1:$oai_port/v1/responses" \
            -H "Authorization: Bearer $cap_token" -H "Content-Type: application/json" \
            --data "$response_body" --output "$tok_body_file" \
            --write-out '%{http_code}' || true)
        tok=$(cat "$tok_body_file" 2>/dev/null || true)
        if [[ "$no_tok_status" == "401" ]] \
            && echo "$no_tok" | grep -Fqi "missing or invalid coop-proxy capability token"; then
            pass "openai proxy refuses a request missing the capability token"
        else
            fail "openai proxy refuses a request missing the capability token" \
                "status: $no_tok_status body: $no_tok"
        fi
        if echo "$tok" | grep -Fqi "missing or invalid coop-proxy capability token"; then
            fail "valid capability token forwards an allowed operation" \
                "local capability refusal (status $tok_status): $tok"
        elif echo "$tok" | grep -Fqi "operation is not allowed by coop-proxy"; then
            fail "valid capability token forwards an allowed operation" \
                "local operation-policy refusal (status $tok_status): $tok"
        elif [[ "$tok_status" =~ ^[1-5][0-9][0-9]$ ]]; then
            pass "valid capability token forwards an allowed operation"
        else
            fail "valid capability token forwards an allowed operation" \
                "no HTTP response (status: $tok_status body: $tok)"
        fi
    else
        fail "locate openai proxy port" "no 127.0.0.1:<port> in codex config"
    fi

    # ── The jail confines the host-side proxy (issue #411, slice 3) ──
    # Prove the confinement coop applies to `coop-proxy` actually restricts on
    # this host: a filesystem write, a program exec, and egress to a
    # non-upstream port are all blocked, while the upstream port (:443) stays
    # reachable. `coop-proxy --jail-selftest` runs the identical probe under
    # whichever mechanism confined it — Linux self-applies Landlock; macOS
    # applies the same Seatbelt profile coop uses, via `sandbox-exec` — and
    # exits 0 with "=> PASS" only if all four hold. This is the host-side
    # analog of the guest-side non-exposure checks above.
    local proxy_bin selftest_out selftest_rc=0
    proxy_bin="$(dirname "$BINARY")/coop-proxy"
    if [[ "$(uname -s)" == "Darwin" ]]; then
        local sb_profile
        sb_profile="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/src/seatbelt-proxy.sb"
        if [[ -f "$sb_profile" ]]; then
            # Pass the profile inline with -p (as coop's launcher does), not -f,
            # and bind PROXY_BIN to the proxy's absolute path (as the launcher
            # does), so the smoke test exercises the same code path production
            # uses. `pwd -P` resolves the dir the way the kernel does at exec.
            local proxy_bin_abs
            proxy_bin_abs="$(cd "$(dirname "$proxy_bin")" && pwd -P)/$(basename "$proxy_bin")"
            selftest_out=$(sandbox-exec -D "PROXY_BIN=$proxy_bin_abs" \
                -p "$(cat "$sb_profile")" "$proxy_bin_abs" --jail-selftest 2>&1) \
                || selftest_rc=$?
            if [[ $selftest_rc -eq 0 ]] && echo "$selftest_out" | grep -q "=> PASS"; then
                pass "proxy jail confines coop-proxy (Seatbelt: no fs-write/exec/off-list egress)"
            else
                fail "proxy jail confines coop-proxy (Seatbelt)" "rc=$selftest_rc out: $selftest_out"
            fi
        else
            skip "proxy jail self-test (Seatbelt profile not found at $sb_profile)"
        fi
    else
        selftest_out=$("$proxy_bin" --jail-selftest 2>&1) || selftest_rc=$?
        if [[ $selftest_rc -eq 0 ]] && echo "$selftest_out" | grep -q "=> PASS"; then
            pass "proxy jail confines coop-proxy (Landlock: no fs-write/exec/off-list egress)"
        else
            fail "proxy jail confines coop-proxy (Landlock)" "rc=$selftest_rc out: $selftest_out"
        fi
    fi

    # ── Teardown tears the proxy down ──
    px stop "$inst_name" >/dev/null 2>&1 || true
    if [[ -n "$oai_port" ]]; then
        if curl -s --max-time 3 -o /dev/null "http://127.0.0.1:$oai_port/v1/models"; then
            fail "stop tears the proxy down" "proxy still answering on 127.0.0.1:$oai_port"
        else
            pass "stop tears the proxy down (host port no longer answers)"
        fi
    fi

    px destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    rm -r "$px_dir"
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
    local recovery_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$recovery_ws"
    if coop up "$recovery_ws" --name "$inst_name" --no-agents --no-devcontainer --image "$img"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up from recovered image exits 0"
    else
        fail "up from recovered image exits 0" "exit code: $?"
        coop images --delete "$img" 2>/dev/null || true
        return
    fi

    GUEST_INSTANCE="$inst_name"
    local reply
    if reply=$(guest_exec echo ok) && [[ "$reply" == *ok* ]]; then
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

# ── devcontainer.json translator (pre-VM + --full) ────────────

# Write a sample devcontainer.json into $1/.devcontainer/devcontainer.json.
# Exercises JSONC features (comments + trailing commas) so the integration
# run also catches parser regressions, not just the unit tests.
_write_devcontainer() {
    local dir="$1"
    mkdir -p "$dir/.devcontainer"
    cat > "$dir/.devcontainer/devcontainer.json" <<'EOF'
{
    // sample devcontainer.json — coop reads a subset.
    "name": "coop-it-demo",
    "image": "ubuntu:22.04",
    "hostRequirements": {
        "cpus": 2,
        "memory": "1GiB",
    },
    "containerEnv": {
        "COOP_TEST_DEVCONTAINER": "applied",
    },
    "forwardPorts": [3000],
    "postStartCommand": "echo dc-hooked > /tmp/coop-dc-marker",
    "remoteUser": "root",
}
EOF
}

test_devcontainer_translator() {
    echo ""
    echo "=== Phase: devcontainer.json translator (dry-run) ==="

    local dcdir="$tmpdir/devcontainer-ws"
    _write_devcontainer "$dcdir"
    local dcfile="$dcdir/.devcontainer/devcontainer.json"

    # --dry-run with auto-discovery: report is printed to stderr, no VM work.
    if coop up "$dcdir" --name "${INSTANCE}-dc-dry" --dry-run --no-agents; then
        pass "up ... --dry-run exits 0"
    else
        fail "up ... --dry-run exits 0" "exit code: $? stderr: $HARNESS_ERR"
    fi

    # Report content lands on stderr (per the CLAUDE.md "tracing → stderr" rule).
    if grep -q "hostRequirements.cpus" <<< "$HARNESS_ERR" \
        && grep -q "applied" <<< "$HARNESS_ERR"; then
        pass "dry-run report covers hostRequirements"
    else
        fail "dry-run report covers hostRequirements" "stderr: $HARNESS_ERR"
    fi

    # JSONC: the file uses //-comments and trailing commas; parser must accept.
    if grep -q "containerEnv" <<< "$HARNESS_ERR"; then
        pass "JSONC (comments + trailing commas) parses"
    else
        fail "JSONC (comments + trailing commas) parses" "stderr: $HARNESS_ERR"
    fi

    # `remoteUser: "root"` is rejected by the GuestUser validator
    # (coop requires an unprivileged uid-1000 account), so the report
    # row for `remoteUser` must specifically read "invalid" — not
    # "unsupported" (which other rows like `image` carry, so a fuzzy
    # whole-buffer grep would pass by coincidence).
    if grep "remoteUser" <<< "$HARNESS_ERR" | grep -q "invalid"; then
        pass "remoteUser=root is reported invalid"
    else
        fail "remoteUser=root is reported invalid" "stderr: $HARNESS_ERR"
    fi

    # Dedicated check command: validates the same file without discovery,
    # config loading, setup, or VM work.
    if coop devcontainer check "$dcfile" --stage both; then
        pass "devcontainer check exits 0"
    else
        fail "devcontainer check exits 0" "exit code: $? stderr: $HARNESS_ERR"
    fi
    if grep -q "setup-stage translation:" <<< "$HARNESS_ERR" \
        && grep -q "start-stage translation:" <<< "$HARNESS_ERR" \
        && grep -q "remoteUser" <<< "$HARNESS_ERR"; then
        pass "devcontainer check reports setup and start stages"
    else
        fail "devcontainer check reports setup and start stages" "stderr: $HARNESS_ERR"
    fi

    local oci_bad="$tmpdir/devcontainer-oci-bad.json"
    cat > "$oci_bad" <<'EOF'
{
  "features": {
    "ghcr.io/devcontainers/features/github-cli:1": {
      "version": { "nested": true }
    }
  }
}
EOF
    if coop devcontainer check "$oci_bad" --stage setup; then
        pass "devcontainer check handles invalid OCI feature options"
    else
        fail "devcontainer check handles invalid OCI feature options" "exit code: $? stderr: $HARNESS_ERR"
    fi
    if grep -q "features.ghcr.io/devcontainers/features/github-cli:1" <<< "$HARNESS_ERR" \
        && grep -q "invalid" <<< "$HARNESS_ERR" \
        && grep -q "must be a string" <<< "$HARNESS_ERR"; then
        pass "invalid OCI feature options are reported loudly"
    else
        fail "invalid OCI feature options are reported loudly" "stderr: $HARNESS_ERR"
    fi

    # --no-devcontainer silently skips the file: the report header must NOT appear.
    # `--dry-run` lets us exercise the discovery path without any VM work.
    if coop up "$dcdir" --name "${INSTANCE}-dc-skip" \
        --no-devcontainer --dry-run --no-agents; then
        if grep -q "devcontainer.json:" <<< "$HARNESS_ERR"; then
            fail "--no-devcontainer suppresses discovery" "report header still appeared: $HARNESS_ERR"
        else
            pass "--no-devcontainer suppresses discovery"
        fi
    else
        fail "--no-devcontainer suppresses discovery" "exit code: $? stderr: $HARNESS_ERR"
    fi

    local pref_cfg="$tmpdir/devcontainer-pref-config.toml"
    local pref_data="$tmpdir/devcontainer-pref-data"
    mkdir -p "$pref_data"
    cat > "$pref_cfg" <<EOF
data_dir = "$pref_data"
EOF

    if coop --config "$pref_cfg" devcontainer ignore "$dcdir"; then
        pass "devcontainer ignore records persistent opt-out"
    else
        fail "devcontainer ignore records persistent opt-out" "exit code: $? stderr: $HARNESS_ERR"
    fi

    if coop --config "$pref_cfg" devcontainer status "$dcdir" \
        && grep -q "disabled" <<< "$HARNESS_OUT" \
        && grep -q "$dcdir" <<< "$HARNESS_OUT"; then
        pass "devcontainer status reports project opt-out"
    else
        fail "devcontainer status reports project opt-out" "stdout: $HARNESS_OUT stderr: $HARNESS_ERR"
    fi

    if coop --config "$pref_cfg" up "$dcdir" --name "${INSTANCE}-dc-pref" \
        --dry-run --no-agents; then
        if grep -q "stored devcontainer opt-out" <<< "$HARNESS_ERR" \
            && ! grep -q "devcontainer.json:" <<< "$HARNESS_ERR"; then
            pass "stored devcontainer opt-out skips discovery"
        else
            fail "stored devcontainer opt-out skips discovery" "stderr: $HARNESS_ERR"
        fi
    else
        fail "stored devcontainer opt-out skips discovery" "exit code: $? stderr: $HARNESS_ERR"
    fi

    if coop --config "$pref_cfg" setup --workspace "$dcdir" --dry-run; then
        if grep -q "stored devcontainer opt-out" <<< "$HARNESS_ERR" \
            && ! grep -q "setup-stage translation" <<< "$HARNESS_ERR"; then
            pass "stored devcontainer opt-out skips setup --workspace dry-run discovery"
        else
            fail "stored devcontainer opt-out skips setup --workspace dry-run discovery" "stderr: $HARNESS_ERR"
        fi
    else
        fail "stored devcontainer opt-out skips setup --workspace dry-run discovery" "exit code: $? stderr: $HARNESS_ERR"
    fi

    if coop --config "$pref_cfg" start --workspace "$dcdir" --dry-run; then
        if grep -q "stored devcontainer opt-out" <<< "$HARNESS_ERR" \
            && ! grep -q "start-stage translation" <<< "$HARNESS_ERR"; then
            pass "stored devcontainer opt-out skips start --workspace dry-run discovery"
        else
            fail "stored devcontainer opt-out skips start --workspace dry-run discovery" "stderr: $HARNESS_ERR"
        fi
    else
        fail "stored devcontainer opt-out skips start --workspace dry-run discovery" "exit code: $? stderr: $HARNESS_ERR"
    fi

    if coop --config "$pref_cfg" up "$dcdir" --name "${INSTANCE}-dc-pref-explicit" \
        --devcontainer "$dcfile" --dry-run --no-agents; then
        if grep -q "devcontainer.json:" <<< "$HARNESS_ERR"; then
            pass "explicit --devcontainer bypasses stored opt-out"
        else
            fail "explicit --devcontainer bypasses stored opt-out" "stderr: $HARNESS_ERR"
        fi
    else
        fail "explicit --devcontainer bypasses stored opt-out" "exit code: $? stderr: $HARNESS_ERR"
    fi

    if coop --config "$pref_cfg" devcontainer clear "$dcdir"; then
        pass "devcontainer clear removes persistent opt-out"
    else
        fail "devcontainer clear removes persistent opt-out" "exit code: $? stderr: $HARNESS_ERR"
    fi

    local stale_ws
    stale_ws=$(mktemp -d "$tmpdir/devcontainer-stale-XXXXXX")
    _write_devcontainer "$stale_ws"
    if coop --config "$pref_cfg" devcontainer ignore "$stale_ws"; then
        rm -rf "$stale_ws"
        if coop --config "$pref_cfg" devcontainer clear "$stale_ws" \
            && grep -q "Cleared devcontainer opt-out" <<< "$HARNESS_OUT"; then
            pass "devcontainer clear removes stale deleted-project opt-out"
        else
            fail "devcontainer clear removes stale deleted-project opt-out" "stdout: $HARNESS_OUT stderr: $HARNESS_ERR"
        fi
    else
        fail "devcontainer clear removes stale deleted-project opt-out" "ignore failed: $HARNESS_ERR"
    fi

    if coop_fails --config "$pref_cfg" up "$dcdir" --name "${INSTANCE}-dc-pref-cleared" --no-agents; then
        if grep -qi "devcontainer" <<< "$HARNESS_ERR" \
            && grep -q -- "--no-devcontainer" <<< "$HARNESS_ERR"; then
            pass "cleared devcontainer opt-out restores non-TTY prompt error"
        else
            fail "cleared devcontainer opt-out restores non-TTY prompt error" "stderr: $HARNESS_ERR"
        fi
    else
        fail "cleared devcontainer opt-out restores non-TTY prompt error" "expected non-zero exit"
    fi

    # Non-interactive + discovered file + no escape hatch must error with the
    # hint pointing at --devcontainer / --no-devcontainer.
    if coop_fails up "$dcdir" --name "${INSTANCE}-dc-noopt" --no-agents; then
        if grep -qi "devcontainer" <<< "$HARNESS_ERR" \
            && grep -q -- "--no-devcontainer" <<< "$HARNESS_ERR"; then
            pass "non-TTY without escape hatch errors with hint"
        else
            fail "non-TTY without escape hatch errors with hint" "stderr: $HARNESS_ERR"
        fi
    else
        fail "non-TTY without escape hatch errors with hint" "expected non-zero exit"
    fi

    # CLI overrides devcontainer.json values — report should mark cpus as
    # "overridden" with source = CLI.
    if coop up "$dcdir" --name "${INSTANCE}-dc-override" \
        --vcpus 8 --dry-run --no-agents; then
        pass "up --dry-run with overriding CLI flag exits 0"
    else
        fail "up --dry-run with overriding CLI flag exits 0" "stderr: $HARNESS_ERR"
        return
    fi
    if grep "hostRequirements.cpus" <<< "$HARNESS_ERR" | grep -q "overridden"; then
        pass "CLI --vcpus is reported as overriding devcontainer.json"
    else
        fail "CLI --vcpus is reported as overriding devcontainer.json" "stderr: $HARNESS_ERR"
    fi
}

# ── devcontainer.json apply (--full only) ─────────────────────

test_devcontainer_apply() {
    echo ""
    echo "=== Phase: devcontainer.json apply (--full) ==="

    local dcdir="$tmpdir/devcontainer-apply-ws"
    _write_devcontainer "$dcdir"
    local dcfile="$dcdir/.devcontainer/devcontainer.json"
    local inst_name="${INSTANCE}-dc-apply"

    # Use explicit --devcontainer to skip the prompt in CI.
    if coop up "$dcdir" --name "$inst_name" \
        --devcontainer "$dcfile" --no-agents; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up with --devcontainer exits 0"
    else
        fail "up with --devcontainer exits 0" "exit code: $? stderr: $HARNESS_ERR"
        return
    fi

    GUEST_INSTANCE="$inst_name"

    # containerEnv must reach the guest as a literal env var.
    local seen_env
    seen_env=$(guest_exec printenv COOP_TEST_DEVCONTAINER 2>/dev/null) || seen_env=""
    if [[ "$seen_env" == "applied" ]]; then
        pass "containerEnv reached the guest"
    else
        fail "containerEnv reached the guest" "got: '$seen_env'"
    fi

    # postStartCommand must have written the marker.
    local seen_marker
    seen_marker=$(guest_exec cat /tmp/coop-dc-marker 2>/dev/null) || seen_marker=""
    if [[ "$seen_marker" == *dc-hooked* ]]; then
        pass "postStartCommand ran in the guest"
    else
        fail "postStartCommand ran in the guest" "marker contents: '$seen_marker'"
    fi

    cat > "$dcfile" <<'EOF'
{
    "name": "coop-it-demo-changed",
    "hostRequirements": {
        "cpus": 4,
        "memory": "2GiB"
    },
    "containerEnv": {
        "COOP_TEST_DEVCONTAINER": "changed"
    },
    "forwardPorts": [3001],
    "postStartCommand": "echo changed > /tmp/coop-dc-marker",
    "remoteUser": "root"
}
EOF

    if coop stop "$inst_name"; then
        pass "stop devcontainer apply instance exits 0"
    else
        fail "stop devcontainer apply instance exits 0" "exit code: $? stderr: $HARNESS_ERR"
    fi

    if coop start --workspace "$dcdir" --no-agents; then
        pass "start --workspace after devcontainer change exits 0"
    else
        fail "start --workspace after devcontainer change exits 0" "exit code: $? stderr: $HARNESS_ERR"
    fi
    if grep -q "devcontainer.json changed" <<< "$HARNESS_ERR" \
        && grep -q "Destroy and recreate" <<< "$HARNESS_ERR" \
        && grep -q "features, hostRequirements, mounts" <<< "$HARNESS_ERR" \
        && grep -q "not re-applied automatically" <<< "$HARNESS_ERR"; then
        pass "changed devcontainer warning is informational"
    else
        fail "changed devcontainer warning is informational" "stderr: $HARNESS_ERR"
    fi

    seen_env=$(guest_exec printenv COOP_TEST_DEVCONTAINER 2>/dev/null) || seen_env=""
    if [[ "$seen_env" == "applied" ]]; then
        pass "changed containerEnv is not re-applied on restart"
    else
        fail "changed containerEnv is not re-applied on restart" "got: '$seen_env'"
    fi

    seen_marker=$(guest_exec cat /tmp/coop-dc-marker 2>/dev/null) || seen_marker=""
    if [[ "$seen_marker" != *changed* ]]; then
        pass "changed postStartCommand is not re-applied on restart"
    else
        fail "changed postStartCommand is not re-applied on restart" "marker contents: '$seen_marker'"
    fi

    unset GUEST_INSTANCE

    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
}

# ── OCI devcontainer feature install (--full only) ────────────

# Resolve a real public GHCR devcontainer Feature, bake it into the image,
# and assert the tool it installs is present and runnable in the guest. This
# is the end-to-end counterpart to the invalid-options error path exercised
# in test_devcontainer_translator (which only runs `devcontainer check`).
test_devcontainer_oci_feature() {
    echo ""
    echo "=== Phase: OCI devcontainer feature install (--full) ==="

    local dcdir="$tmpdir/devcontainer-oci-ws"
    mkdir -p "$dcdir/.devcontainer"
    local dcfile="$dcdir/.devcontainer/devcontainer.json"
    local inst_name="${INSTANCE}-dc-oci"

    # github-cli is a small public Feature on ghcr.io that installs `gh` to
    # /usr/local/bin. Pinning the major tag keeps the resolved digest stable.
    cat > "$dcfile" <<'EOF'
{
    "name": "coop-it-oci-feature",
    "features": {
        "ghcr.io/devcontainers/features/github-cli:1": {}
    }
}
EOF

    # Use explicit --devcontainer to skip the prompt in CI. Feature resolution
    # and bake happen during `up`; a network failure reaching ghcr.io would
    # surface here.
    if coop up "$dcdir" --name "$inst_name" \
        --devcontainer "$dcfile" --no-agents; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up with OCI feature exits 0"
    else
        fail "up with OCI feature exits 0" "exit code: $? stderr: $HARNESS_ERR"
        return
    fi

    GUEST_INSTANCE="$inst_name"

    # The feature's install.sh must have placed `gh` on PATH in the guest.
    if guest_exec command -v gh >/dev/null 2>&1; then
        pass "gh is on PATH in the guest"
    else
        fail "gh is on PATH in the guest" "stderr: $(guest_stderr)"
    fi

    # The installed tool must be runnable, not just present.
    local gh_version
    if gh_version=$(guest_exec gh --version 2>/dev/null) \
        && [[ "$gh_version" == *"gh version"* ]]; then
        pass "gh runs in the guest"
    else
        fail "gh runs in the guest" "got: '$gh_version' stderr: $(guest_stderr)"
    fi

    unset GUEST_INSTANCE

    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
}

# ── post_start hook (--full only) ──────────────────────────────

test_post_start() {
    echo ""
    echo "=== Phase: post_start hook ==="

    local inst_name="${INSTANCE}-poststart"
    local marker="/tmp/coop-post-start-$$.marker"

    # --post-start runs the command in the guest after SSH is ready.
    # The marker file written by the hook is the assertion.
    local post_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$post_ws"
    if coop up "$post_ws" --name "$inst_name" --no-agents --no-devcontainer \
        --post-start "echo hooked > $marker"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up --post-start exits 0"
    else
        fail "up --post-start exits 0" "exit code: $?"
        return
    fi

    GUEST_INSTANCE="$inst_name"
    local seen
    seen=$(guest_exec cat "$marker" 2>/dev/null) || seen=""
    unset GUEST_INSTANCE

    if [[ "$seen" == *hooked* ]]; then
        pass "--post-start hook ran in the guest"
    else
        fail "--post-start hook ran in the guest" "marker contents: '$seen'"
    fi

    # Verify a failing hook does not fail `coop up` (warn-and-continue).
    local fail_inst="${INSTANCE}-poststart-fail"
    local fail_ws="$tmpdir/${fail_inst}-ws"
    mkdir -p "$fail_ws"
    if coop up "$fail_ws" --name "$fail_inst" --no-agents --no-devcontainer \
        --post-start "false; exit 1"; then
        STARTED_INSTANCES+=("$fail_inst")
        pass "up succeeds when --post-start fails (warn-and-continue)"
    else
        fail "up succeeds when --post-start fails" "exit code: $?"
    fi

    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"
    coop destroy "$fail_inst" 2>/dev/null || true
    untrack_instance "$fail_inst"
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
    local err
    COOP_TEST_INJECT_PROVISION_FAILURE=1 "$BINARY" setup -y --rebuild >/dev/null 2>"$tmpdir/pf-stderr" || rc=$?
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

# ── Guest user CLI validation (pre-VM) ────────────────────────

test_guest_user_validation() {
    echo ""
    echo "=== Phase: --guest-user CLI validation ==="

    # `root` is rejected by the GuestUser validator: coop requires an
    # unprivileged uid-1000 account.
    if coop_fails setup -y --guest-user root --dry-run; then
        pass "setup --guest-user root is rejected"
    else
        fail "setup --guest-user root is rejected" "should have failed"
    fi

    # Uppercase and other non-POSIX-portable characters are rejected.
    if coop_fails setup -y --guest-user Vscode --dry-run; then
        pass "setup --guest-user with uppercase rejected"
    else
        fail "setup --guest-user with uppercase rejected" "should have failed"
    fi

    # `--guest-user` is only on `setup` — the rest of the lifecycle
    # reads the persisted value. clap should reject the flag on `start`.
    if coop_fails start "${INSTANCE}-gu-flag" --guest-user vscode --no-agents; then
        if grep -qi "unexpected\|unknown\|unrecognized\|--guest-user" <<< "$HARNESS_ERR"; then
            pass "start --guest-user is rejected as unknown flag"
        else
            fail "start --guest-user is rejected as unknown flag" "unexpected error: $HARNESS_ERR"
        fi
    else
        fail "start --guest-user is rejected as unknown flag" "should have failed"
    fi
}

# ── Alternate guest user lifecycle (--full only) ──────────────

test_guest_user_alt() {
    echo ""
    echo "=== Phase: alternate guest user ==="

    local img_name="test-altuser-$$"
    local inst_name="${INSTANCE}-altuser"
    local alt_user="vscode"

    # Setup a separate image whose `template_config.json` should record
    # guest_user=vscode and whose first boot should create the vscode
    # account at uid 1000.
    if coop setup -y --image "$img_name" --guest-user "$alt_user"; then
        pass "setup --guest-user $alt_user exits 0"
    else
        fail "setup --guest-user $alt_user exits 0" "exit code: $? stderr: $HARNESS_ERR"
        return
    fi

    # template_config.json must serialize the configured user — this is
    # what start/shell/exec read on subsequent invocations.
    local tc_path="$HOME/.coop/images/$img_name/template-config.json"
    if [[ -f "$tc_path" ]]; then
        if grep -q "\"guest_user\": *\"$alt_user\"" "$tc_path"; then
            pass "template_config.json records guest_user=$alt_user"
        else
            fail "template_config.json records guest_user=$alt_user" "contents: $(cat "$tc_path")"
        fi
    else
        skip "template_config.json records guest_user=$alt_user" "config at $tc_path not found"
    fi

    # Create an instance from the alt-user image — no `--guest-user` flag
    # here on purpose; coop must read the persisted value.
    local alt_ws="$tmpdir/${inst_name}-ws"
    mkdir -p "$alt_ws"
    if coop up "$alt_ws" --name "$inst_name" --no-agents --no-devcontainer --image "$img_name"; then
        STARTED_INSTANCES+=("$inst_name")
        pass "up (alt-user image) exits 0"
    else
        fail "up (alt-user image) exits 0" "exit code: $? stderr: $HARNESS_ERR"
        coop images --delete "$img_name" 2>/dev/null || true
        return
    fi

    GUEST_INSTANCE="$inst_name"

    # whoami via SSH — this is the highest-signal assertion. If any
    # SshTarget construction site still hardcoded `ubuntu`, the session
    # would either fail to authenticate or land on the wrong account.
    local whoami_out
    if whoami_out=$(guest_exec whoami); then
        if [[ "$whoami_out" == "$alt_user" ]]; then
            pass "guest user is '$alt_user'"
        else
            fail "guest user is '$alt_user'" "got: $whoami_out"
        fi
    else
        fail "guest user is '$alt_user'" "ssh failed; stderr: $(guest_stderr)"
    fi

    # HOME should resolve to the alt user's directory.
    local home
    if home=$(guest_exec printenv HOME); then
        if [[ "$home" == "/home/$alt_user" ]]; then
            pass "HOME is /home/$alt_user"
        else
            fail "HOME is /home/$alt_user" "got: $home"
        fi
    else
        fail "HOME is /home/$alt_user" "printenv failed"
    fi

    # /workspace ownership tracks the configured user.
    local ws_owner
    if ws_owner=$(guest_exec stat -c '%U' /workspace); then
        if [[ "$ws_owner" == "$alt_user" ]]; then
            pass "/workspace owned by $alt_user"
        else
            fail "/workspace owned by $alt_user" "owner: $ws_owner"
        fi
    else
        fail "/workspace owned by $alt_user" "stat failed"
    fi

    # The Claude installer writes to ~/.local/bin under the configured
    # user's home, not /home/ubuntu. This is exactly the bake-time path
    # that `GuestUser::claude_bin` resolves at runtime.
    if guest_exec test -x "/home/$alt_user/.local/bin/claude"; then
        pass "claude binary at /home/$alt_user/.local/bin/claude"
    else
        skip "claude binary at /home/$alt_user/.local/bin/claude" \
            "not installed in this image (no Claude profile)"
    fi

    # The alt user must be in sudo + docker groups so the lifecycle
    # parity with `ubuntu` actually holds.
    local groups_out
    if groups_out=$(guest_exec groups); then
        if echo "$groups_out" | grep -qw docker && echo "$groups_out" | grep -qw sudo; then
            pass "$alt_user is in sudo and docker groups"
        else
            fail "$alt_user is in sudo and docker groups" "groups: $groups_out"
        fi
    else
        fail "$alt_user is in sudo and docker groups" "stderr: $(guest_stderr)"
    fi

    # Passwordless sudo for the alt user (the sudoers drop-in is keyed
    # by the configured user name).
    local sudo_out
    if sudo_out=$(guest_exec sudo whoami); then
        if [[ "$sudo_out" == "root" ]]; then
            pass "$alt_user has passwordless sudo"
        else
            fail "$alt_user has passwordless sudo" "got: $sudo_out"
        fi
    else
        fail "$alt_user has passwordless sudo" "stderr: $(guest_stderr)"
    fi

    unset GUEST_INSTANCE

    # Stop + restart — the restart path reads the persisted user via a
    # different code path (`start_existing` + `wait_for_lima_ssh`), so
    # this catches drift between the two SSH-target sites.
    if coop stop "$inst_name"; then
        pass "stop alt-user instance exits 0"
    else
        fail "stop alt-user instance exits 0" "exit code: $?"
    fi

    if coop start "$inst_name"; then
        pass "restart alt-user instance exits 0"
    else
        fail "restart alt-user instance exits 0" "exit code: $? stderr: $HARNESS_ERR"
    fi

    GUEST_INSTANCE="$inst_name"
    if whoami_out=$(guest_exec whoami); then
        if [[ "$whoami_out" == "$alt_user" ]]; then
            pass "whoami after restart still '$alt_user'"
        else
            fail "whoami after restart still '$alt_user'" "got: $whoami_out"
        fi
    else
        fail "whoami after restart still '$alt_user'" "stderr: $(guest_stderr)"
    fi
    unset GUEST_INSTANCE

    # Cleanup
    coop destroy "$inst_name" 2>/dev/null || true
    untrack_instance "$inst_name"

    if coop images --delete "$img_name"; then
        pass "images --delete $img_name exits 0"
    else
        fail "images --delete $img_name exits 0" "exit code: $?"
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
    test_mem_floor
    test_profiles_cli
    test_completions
    test_devcontainer_translator
    test_guest_user_validation

    # Setup + primary instance
    test_setup
    test_up_creates_primary_instance
    test_start_rejects_missing_instance
    test_duplicate_name
    test_status_running
    test_list_running
    test_auto_resolve_running
    test_shell_connectivity
    test_ssh_alias
    test_ssh_config
    test_editor
    test_exec
    test_claude_bin_path
    test_claude_settings_merge
    test_claude_onboarding_seed
    test_codex_bin_path
    test_codex_account_auth_support
    test_codex_chatgpt_mode
    test_codex_sandbox_bypass
    test_agent_update
    test_github_token_forwarding
    test_term_handling
    test_guest_environment
    test_sudo
    test_network
    test_docker
    test_logs
    test_profiles
    test_guest_fingerprint

    # Stop + restart + stopped-state verification
    test_stop
    test_stop_idempotency
    test_auto_resolve_stopped
    test_status_stopped
    test_agent_update_stopped
    test_list_stopped
    test_resize_status
    test_reconfigure
    test_commit_restore
    test_restart_stopped
    test_restart_rejects_ignored_flags
    test_destroy
    test_auto_resolve_no_instances
    test_list_empty

    # Idempotency: re-run commands that should be safe to repeat
    test_idempotency

    # Extended tests (each manages its own instance lifecycle)
    if [[ "$FULL" == "1" ]]; then
        test_quickstart
        test_up_project_workflow
        test_git_repo
        test_host_mount
        test_host_mount_custom_guest_path
        test_port_forwards
        test_workspace_sync
        test_appledouble_sidecars
        test_multi_instance
        test_named_images
        test_guest_user_alt
        test_custom_profiles
        test_builtin_profiles
        test_post_start
        test_devcontainer_apply
        test_devcontainer_oci_feature

        # Local marketplace directory copy
        test_local_marketplace

        # github = "pat" mode + per-repo token forwarding (uses a stub image)
        test_github_pat_forwarding

        # Config sources: CLAUDE.md + rules copy
        test_config_dir

        # [guest_env] config block + literal-over-forwarded precedence
        test_guest_env_config

        # coop model local/remote: local-model wiring lands and reverts
        test_local_model

        # credential-injecting proxy (#411): guest wiring, key suppression,
        # capability gate, teardown — Anthropic + OpenAI/Codex
        test_proxy

        # Interrupted setup: SIGKILL mid-build, verify clean recovery
        test_interrupted_setup

        # Provision failure rebuilds the golden image, so run before
        # destroy --all which removes it entirely.
        test_provision_failure

        # destroy --all wipes every coop-managed instance on the host,
        # not just ones this run created. Gate behind an opt-in env var
        # so dev machines aren't silently cleared. Run last because it
        # also removes the golden image.
        if [[ "${COOP_TEST_DESTRUCTIVE:-0}" == "1" ]]; then
            test_destroy_all
            echo ""
            echo "NOTE: destroy --all removed the golden image."
            echo "      Run 'coop setup -y' before next use."
        else
            echo ""
            echo "=== Phase: destroy --all ==="
            skip "destroy --all (set COOP_TEST_DESTRUCTIVE=1 to enable)"
        fi
    fi

    summary
}

main
