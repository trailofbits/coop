#!/usr/bin/env bash
set -uo pipefail

# Real-hardware checks for coop-sandbox (macos/coop-sandbox), the runtime
# behind the `apple-container` build. Unit tests cannot boot VMs; this boots
# real ones and checks what the backend's isolation contract relies on
# (docs/trust-model.md): peer isolation between sandboxes, no host mounts,
# agent sockets, or canary leakage, pinned SSH over the native channel, and
# the lifecycle (persistence, resources, disk growth, commit/restore, crash
# recovery and interrupted mutations, concurrency).
#
# Usage: tests/integration-apple-sandbox.sh [--only PHASE[,PHASE...]] [--keep]
#   Phases: setup disks machine isolation exposure identity persistence
#           resources growth snapshots recovery concurrency coop
#   CYCLES=5 stop/start cycles; CONCURRENCY="1 4 8" sandboxes per round;
#   KILL_FRACTIONS="50 75 90 95 100 105 110": an interrupted mutation is
#   killed at these percentages of the time an uninterrupted one took;
#   COOP_KILL_FRACTIONS="25 50 75" the same for coop's.
#
# Needs Apple Silicon, macOS 26+, Swift 6.2+, jq, and stock Apple `container`
# with its service running (builds the test image, supplies the kernel). It
# touches nothing but its own state root and image tag, both removed on exit.
# The coop phase also builds `coop --features apple-container` (into the work
# directory) and drives it end to end against a data directory there; the
# images `coop setup` builds in the stock `container` store are deleted too.

if [[ "$(uname -s)" != Darwin || "$(uname -m)" != arm64 ]]; then
    echo "SKIP: coop-sandbox needs an Apple Silicon Mac"
    exit 0
fi
CONTAINER=""
for candidate in /usr/local/bin/container /opt/homebrew/bin/container; do
    [[ -x "$candidate" ]] && { CONTAINER="$candidate"; break; }
done
[[ -n "$CONTAINER" ]] || { echo "SKIP: no Apple container CLI installed"; exit 0; }
for tool in swift cargo jq ssh ssh-keygen nc openssl python3; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
done

ONLY=""
KEEP=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --only) ONLY=",$2,"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done
want() { [[ -z "$ONLY" || "$ONLY" == *",$1,"* ]]; }

cd "$(dirname "$0")/.." || exit 1
FIXTURES="$PWD/tests/fixtures/apple-sandbox"
RUN="t$(openssl rand -hex 4)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/coop-sandbox-test.XXXXXX")"
ROOT="$WORK/root"
IMAGE="local/coop-sandbox-test:$RUN"
MAINTENANCE="local/coop-sandbox-test-maintenance:$RUN"
SANDBOX="$WORK/bin/coop-sandbox"
CYCLES="${CYCLES:-5}"
CONCURRENCY="${CONCURRENCY:-1 4 8}"
KILL_FRACTIONS="${KILL_FRACTIONS:-50 75 90 95 100 105 110}"
COOP_KILL_FRACTIONS="${COOP_KILL_FRACTIONS:-25 50 75}"
# A secret that exists only in this script's environment; it must never reach
# the runtime, its logs, the image, or a guest.
CANARY="coop-test-canary-$(openssl rand -hex 16)"
export CANARY

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

now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# timed_ms CMD...: run CMD and print how long it took, in milliseconds.
timed_ms() {
    local t0
    t0="$(now_ms)"
    "$@" >/dev/null 2>&1
    echo $(($(now_ms) - t0))
}

# delay_s MS PERCENT: PERCENT of MS, as seconds for sleep.
delay_s() {
    local ms=$(($1 * $2 / 100))
    printf '%d.%03d' $((ms / 1000)) $((ms % 1000))
}

check() {
    local label="$1"
    shift
    if "$@"; then pass "$label"; else fail "$label"; fi
}

# refuses CMD...: CMD must fail.
refuses() { ! "$@" >/dev/null 2>&1; }

summary() {
    echo ""
    echo "────────────────────────────────────────"
    echo "  $pass_count passed, $fail_count failed, $skip_count skipped"
    echo "────────────────────────────────────────"
    if [[ $fail_count -gt 0 ]]; then
        exit 1
    fi
}

# ── Runtime helpers ───────────────────────────────────────────

sbx() { "$SANDBOX" "$1" --root "$ROOT" "${@:2}"; }
# Two-word subcommands take --root after both words.
sbx2() { "$SANDBOX" "$1" "$2" --root "$ROOT" "${@:3}"; }
name() { echo "coop-test-$1-$RUN"; }
create() { sbx create "$1" --image "$IMAGE" --cpus "${2:-2}" --memory-mib "${3:-2048}" --disk-gib "${4:-8}" --owner "$RUN" >/dev/null; }
state() { sbx inspect "$1" 2>/dev/null | jq -r .status 2>/dev/null || echo missing; }
guest() { local n="$1"; shift; sbx exec "$n" -- "$@"; }
guest_in() { local n="$1"; shift; sbx exec -i "$n" -- "$@"; }
ip4() { sbx inspect "$1" | jq -r '.live.ipv4 // empty'; }
ip6() {
    local a i
    for ((i = 0; i < 40; i++)); do
        a="$(guest "$1" ip -6 -o addr show eth0 scope global | awk '{print $4}' | cut -d/ -f1 | head -1)"
        [[ -n "$a" ]] && { echo "$a"; return 0; }
        sleep 0.5
    done
    return 1
}

# systemd running (or degraded) with sshd and Docker active.
ready() {
    local n="$1" i st
    for ((i = 0; i < 600; i++)); do
        st="$(guest "$n" systemctl is-system-running 2>/dev/null || true)"
        if [[ "$st" == running || "$st" == degraded ]] && guest "$n" systemctl is-active --quiet ssh docker 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

boot() { sbx start "$1" >/dev/null && ready "$1"; }
verify() { guest "$1" /usr/local/sbin/coop-test-verify; }

# ── Peer isolation probes ─────────────────────────────────────

# peer-probe.sh prints at least this many results; fewer means it failed.
MIN_PROBES=17

# listeners TARGET: TCP/UDP echo on 7777/7778 as systemd units.
listeners() {
    guest "$1" sh -c '
        sysctl -qw net.ipv4.icmp_echo_ignore_broadcasts=0
        systemctl is-active --quiet coop-test-tcp || systemd-run --quiet --unit=coop-test-tcp socat TCP6-LISTEN:7777,ipv6only=0,fork,reuseaddr SYSTEM:"echo pong"
        systemctl is-active --quiet coop-test-udp || systemd-run --quiet --unit=coop-test-udp socat UDP6-RECVFROM:7778,ipv6only=0,fork SYSTEM:"echo upong"' >/dev/null
    sleep 0.5
}

# probe_pair ATTACKER TARGET: prints "REACHED|HOST_MISSES|PROBES". The target
# must already run listeners. Probes rewrite the attacker's routes and
# addresses, so one attacker runs one probe_pair at a time.
probe_pair() {
    local from="$1" to="$2" t4 t6 mac ll results reached host
    t4="$(ip4 "$to")"
    t6="$(ip6 "$to")"
    mac="$(guest "$to" cat /sys/class/net/eth0/address)"
    ll="$(guest "$to" ip -6 -o addr show eth0 scope link | awk '{print $4}' | cut -d/ -f1 | head -1)"
    guest_in "$from" sh -c 'cat > /tmp/probe.sh && chmod +x /tmp/probe.sh' <"$FIXTURES/peer-probe.sh"
    results="$(guest "$from" /tmp/probe.sh "$t4" "$t6" "$mac" "$ll")"
    reached="$(jq -rs '[.[] | select(.reached) | .probe] | join(",")' <<<"$results")"
    # TCP and IPv6 replies reach host sockets; IPv4 UDP/ICMP replies from
    # vmnet guests do not on macOS, so those are not host controls.
    host="$("$FIXTURES/host-probe.sh" "$t4" "$t6" | jq -r '[to_entries[] | select(.value == false and (.key | IN("ipv4-icmp","ipv4-udp") | not)) | .key] | join(",")')"
    echo "$reached|$host|$(grep -c '"probe"' <<<"$results")"
}

# isolated RESULT: a probe_pair result with every vector blocked, every host
# control answered, and the full probe set run.
isolated() {
    local reached host n
    IFS='|' read -r reached host n <<<"$1"
    [[ -z "$reached" && -z "$host" && "${n:-0}" -ge $MIN_PROBES ]]
}

cleanup() {
    local rc=$?
    if (( KEEP == 0 )); then
        if [[ -x "$SANDBOX" && -d "$ROOT" ]]; then
            for n in $(sbx list 2>/dev/null | jq -r '.[].id' 2>/dev/null); do
                sbx stop "$n" >/dev/null 2>&1
                sbx delete "$n" --owner "$RUN" >/dev/null 2>&1
            done
        fi
        "$CONTAINER" image delete "$IMAGE" "$MAINTENANCE" >/dev/null 2>&1
        coop_cleanup
        rm -rf "$WORK"
    else
        echo "Kept $WORK"
    fi
    exit "$rc"
}
trap cleanup EXIT

# ── coop end to end ──────────────────────────────────────────

COOP="$WORK/target/debug/coop"
CDATA="$WORK/coop-data"
CSTATE="$CDATA/backends/apple-container-v1"
CROOT="$CSTATE/runtime"
CCFG="$WORK/coop.toml"
# Same config with a short boot deadline, for a restart whose guest never
# starts sshd.
CCFG_FAIL="$WORK/coop-fail.toml"

coop() { "$COOP" --config "$CCFG" "$@" </dev/null; }
csbx() { "$SANDBOX" "$1" --root "$CROOT" "${@:2}"; }
machine_id() { jq -r .machine_id "$CSTATE/instances/$1/apple-machine.json"; }
record() { csbx inspect "$(machine_id "$1")" | jq -r ".record.$2"; }
cstate() { coop status "$1" --json | jq -r .state; }

coop_cleanup() {
    [[ -d "$CROOT" && -x "$SANDBOX" ]] || return 0
    local id owner short
    for id in $(csbx list 2>/dev/null | jq -r '.[].id' 2>/dev/null); do
        owner="$(csbx inspect "$id" 2>/dev/null | jq -r .record.owner)"
        csbx stop "$id" >/dev/null 2>&1
        csbx delete "$id" --owner "$owner" >/dev/null 2>&1
    done
    short="$(jq -r '.owner_id // empty' "$CSTATE/owner.json" 2>/dev/null | cut -c1-8)"
    [[ -n "$short" ]] || return 0
    local images
    images="$("$CONTAINER" image list --quiet 2>/dev/null | grep "^local/coop-$short")"
    # shellcheck disable=SC2086 # one image reference per word.
    [[ -z "$images" ]] || "$CONTAINER" image delete $images >/dev/null 2>&1
}

# ── Pinned SSH (never the host agent or ~/.ssh) ─────────────

KEY="$WORK/id"
KNOWN="$WORK/known_hosts"

enroll() {
    guest_in "$1" sh -c 'umask 077; mkdir -p /root/.ssh; cat > /root/.ssh/authorized_keys' <"$KEY.pub"
    grep -v "^$1 " "$KNOWN" >"$KNOWN.tmp" 2>/dev/null || true
    echo "$1 $(guest "$1" cut -d' ' -f1-2 /etc/ssh/ssh_host_ed25519_key.pub)" >>"$KNOWN.tmp"
    mv "$KNOWN.tmp" "$KNOWN"
}

pinned() {
    local n="$1"
    shift
    ssh -F /dev/null -i "$KEY" -o IdentitiesOnly=yes -o IdentityAgent=none \
        -o StrictHostKeyChecking=yes -o UserKnownHostsFile="$KNOWN" -o GlobalKnownHostsFile=/dev/null \
        -o HostKeyAlias="$n" -o HostKeyAlgorithms=ssh-ed25519 -o BatchMode=yes -o ConnectTimeout=5 \
        -o ForwardAgent=no -o LogLevel=ERROR "root@$(ip4 "$n")" "$@"
}

A="$(name a)"
B="$(name b)"

# ── Phases ────────────────────────────────────────────────────

echo "=== Phase: setup ==="
mkdir -p "$WORK/bin"
if ./scripts/build-coop-sandbox.sh "$WORK" >"$WORK/build.log" 2>&1; then
    pass "coop-sandbox builds and signs"
else
    fail "coop-sandbox builds and signs" "see $WORK/build.log"
    summary
fi
check "version reports protocol 2 on containerization 0.45.0" \
    test "$("$SANDBOX" version | jq -r '"\(.protocol) \(.containerization)"')" = "2 0.45.0"
if "$CONTAINER" build --platform linux/arm64 -t "$IMAGE" "$FIXTURES/image" >"$WORK/image.log" 2>&1 &&
    "$CONTAINER" image save --platform linux/arm64 -o "$WORK/image.tar" "$IMAGE" >/dev/null 2>&1; then
    pass "test image builds"
else
    fail "test image builds" "see $WORK/image.log"
    summary
fi
kernel="$(readlink -f "$HOME/Library/Application Support/com.apple.container/kernels/default.kernel-arm64")"
check "init accepts the pinned kernel" sbx init --kernel "$kernel"
check "init refuses an unpinned kernel" refuses "$SANDBOX" init --root "$WORK/other" --kernel "$FIXTURES/image/Dockerfile"
imported="$("$SANDBOX" image import --root "$ROOT" --oci-tar "$WORK/image.tar")"
# shellcheck disable=SC2016 # jq program text.
check "image imports into the private store" jq -e --arg r "$IMAGE" 'any(.reference == $r)' <<<"$imported"
rm -f "$WORK/image.tar"
# A maintenance image equivalent to the one coop builds
# (image.rs maintenance_dockerfile): Ubuntu with e2fsprogs.
mkdir -p "$WORK/maintenance"
printf '%s\n' 'FROM docker.io/library/ubuntu:24.04@sha256:008173c23f95b170204355c12626cb5a965d779a7e1283b09e9cffbb1bf33ca3' \
    'RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends e2fsprogs && rm -rf /var/lib/apt/lists/*' \
    >"$WORK/maintenance/Dockerfile"
if "$CONTAINER" build --platform linux/arm64 -t "$MAINTENANCE" "$WORK/maintenance" >"$WORK/maintenance.log" 2>&1 &&
    "$CONTAINER" image save --platform linux/arm64 -o "$WORK/maintenance.tar" "$MAINTENANCE" >/dev/null 2>&1 &&
    sbx2 image import --oci-tar "$WORK/maintenance.tar" >/dev/null; then
    pass "maintenance image builds and imports"
else
    fail "maintenance image builds and imports" "see $WORK/maintenance.log"
fi
check "maintenance installs outside the image store" sbx2 maintenance install --image "$MAINTENANCE" --version 1
sbx2 image delete "$MAINTENANCE" >/dev/null
# shellcheck disable=SC2016 # jq program text.
check "maintenance survives deleting its store image" jq -e --arg r "$MAINTENANCE" '.version == "1" and .reference == $r' <<<"$(sbx2 maintenance inspect)"
rm -f "$WORK/maintenance.tar"

if want disks; then
    echo ""
    echo "=== Phase: disks ==="
    for gib in 8 32 64; do
        n="$(name "d$gib")"
        create "$n" 2 2048 "$gib"
        if boot "$n"; then
            size="$(guest "$n" df -B1 --output=size / | tail -1 | xargs)"
            # ext4 metadata takes a little under 2 %; allow 5 %.
            check "${gib} GiB disk is ${gib} GiB in the guest" test "$size" -ge $((gib * 1024 * 1024 * 1024 * 95 / 100))
        else
            fail "${gib} GiB sandbox boots"
        fi
        sbx stop "$n"
        sbx delete "$n" --owner "$RUN"
    done
    t0="$(date +%s)"
    create "$(name cached)"
    check "a second create from the same image is a clone (<5 s)" test $(($(date +%s) - t0)) -lt 5
    sbx delete "$(name cached)" --owner "$RUN"
fi

create "$A" 4 8192 16
create "$B" 4 8192 16
check "created sandboxes are stopped and owned" test "$(sbx inspect "$A" | jq -r '"\(.status) \(.record.owner)"')" = "stopped $RUN"
boot "$A" || fail "sandbox A boots"
boot "$B" || fail "sandbox B boots"

if want machine; then
    echo ""
    echo "=== Phase: machine ==="
    v="$(verify "$A")"
    check "PID 1 is systemd" test "$(jq -r .pid1 <<<"$v")" = systemd
    check "systemd is running with no failed units" test "$(jq -r '"\(.system_state) \(.failed_units|length)"' <<<"$v")" = "running 0"
    check "sshd and Docker stay active after the exec" test "$(guest "$A" systemctl is-active ssh docker | tr '\n' ' ')" = "active active "
    check "docker runs a container" guest "$A" docker run --rm alpine:3.20 /bin/true
    check "docker builds an image" guest "$A" sh -c 'mkdir -p /tmp/b && printf "FROM alpine:3.20\nRUN echo built > /built\n" > /tmp/b/Dockerfile && docker build -q -t t /tmp/b >/dev/null'
    eff="$(sbx inspect "$A" | jq .effective)"
    check "effective config: kernel pseudo-filesystems only" \
        jq -e '[.mounts[] | select(.type | IN("proc","sysfs","devtmpfs","mqueue","tmpfs","cgroup2","devpts") | not)] | length == 0' <<<"$eff"
    check "effective config: no relays, ports, or agent forwarding" \
        jq -e '.socketRelays == 0 and .publishedPorts == 0 and .sshAgentForwarding == false' <<<"$eff"
    check "effective config: one interface on its own vmnet subnet" \
        jq -e '(.interfaces | length) == 1 and (.interfaces[0].network | startswith("vmnet-shared:10.231."))' <<<"$eff"
    # shellcheck disable=SC2016 # jq program text.
    check "effective config: boots its own disk under the root" \
        jq -e --arg p "/sandboxes/$A/rootfs.ext4" '.rootfs.type == "ext4" and (.rootfs.source | endswith($p))' <<<"$eff"
    check "effective config: requested CPUs and memory" jq -e '.cpus == 4 and .memoryBytes == 8589934592' <<<"$eff"
fi

if want isolation; then
    echo ""
    echo "=== Phase: isolation ==="
    # probe ATTACKER TARGET LABEL: every vector blocked, host control reaches the target.
    probe() {
        local label="$3" result reached host n
        listeners "$2"
        result="$(probe_pair "$1" "$2")"
        IFS='|' read -r reached host n <<<"$result"
        if [[ -n "$reached" ]]; then
            fail "$label: guest blocked on every vector" "reached via $reached"
        elif [[ -n "$host" ]]; then
            fail "$label: host positive control reaches the target" "no reply over $host"
        elif [[ "${n:-0}" -lt $MIN_PROBES ]]; then
            fail "$label: the probe ran" "only ${n:-0} of at least $MIN_PROBES results"
        else
            pass "$label: TCP/UDP/ICMP over IPv4/IPv6, forged routes, static neighbours, spoofed source, broadcast/multicast all blocked"
        fi
    }
    probe "$A" "$B" "A -> B"
    probe "$B" "$A" "B -> A"
    sbx stop "$A"; sbx stop "$B"
    boot "$A"; boot "$B"
    probe "$A" "$B" "A -> B after restarts"
fi

if want exposure; then
    echo ""
    echo "=== Phase: exposure ==="
    mi="$(guest "$A" cat /proc/self/mountinfo)"
    check "no virtiofs, 9p, FUSE, NFS, or SMB mounts" refuses grep -Eq ' - (virtiofs|9p|fuse|fuse\.[^ ]+|nfs4?|cifs|smb3?|smbfs) ' <<<"$mi"
    check "no host path in the mount table" refuses grep -q '/Users/' <<<"$mi"
    token="coop-test-file-$(openssl rand -hex 12)"
    printf '%s\n' "$token" >"$HOME/.coop-test-canary-$RUN"
    check "a host home file is not visible in the guest" \
        test -z "$(guest "$A" sh -c "grep -rslF '$token' / --exclude-dir=proc --exclude-dir=sys --exclude-dir=dev 2>/dev/null | head -1")"
    rm -f "$HOME/.coop-test-canary-$RUN"
    genv="$(guest "$A" sh -c 'tr "\0" "\n" < /proc/1/environ; env')"
    check "no SSH_AUTH_SOCK in the guest" refuses grep -q SSH_AUTH_SOCK <<<"$genv"
    socks="$(guest "$A" sh -c 'find / -xdev -type s 2>/dev/null')"
    check "no agent-like socket in the guest" refuses grep -Eiq 'agent|ssh-auth|host-services' <<<"$socks"
    # shellcheck disable=SC2016 # Expand in the guest.
    check "no host vsock listener reachable" \
        test -z "$(guest "$A" sh -c 'for p in $(seq 1 1024) 2375 5000 8080 268435456 268435457; do timeout 1 socat -u /dev/null VSOCK-CONNECT:2:$p 2>/dev/null && echo $p; done; true')"
    leaked=""
    # shellcheck disable=SC2009 # pgrep cannot match the environment `ps -E` shows.
    ps -axwwE -o command= | grep -E 'coop-sandbox (run|start)' | grep -v grep | grep -qF "$CANARY" && leaked+=" runtime-env"
    grep -qF "$CANARY" "$ROOT/sandboxes/$A/owner.log" "$ROOT/sandboxes/$A/boot.log" 2>/dev/null && leaked+=" logs"
    sbx inspect "$A" | grep -qF "$CANARY" && leaked+=" inspect"
    [[ -n "$(guest "$A" sh -c "grep -rlsF '$CANARY' / --exclude-dir=proc --exclude-dir=sys --exclude-dir=dev 2>/dev/null | head -1")" ]] && leaked+=" guest"
    check "a secret in the caller's environment reaches no runtime process, log, or guest" test -z "$leaked"
    skip "host services on the NAT gateway" "reachable by design; see docs/trust-model.md"
fi

if want identity; then
    echo ""
    echo "=== Phase: identity ==="
    ssh-keygen -q -t ed25519 -N '' -C "coop-test-$RUN" -f "$KEY"
    : >"$KNOWN"
    enroll "$A"
    check "strict SSH against the key read over the native channel" test "$(pinned "$A" echo ok)" = ok
    # shellcheck disable=SC2016 # Expand in the guest.
    check "no agent in the SSH session" test "$(pinned "$A" 'echo ${SSH_AUTH_SOCK:-none}')" = none
    fwd="$(
        eval "$(ssh-agent -s)" >/dev/null
        ssh -F /dev/null -i "$KEY" -o IdentitiesOnly=yes -o StrictHostKeyChecking=yes -o UserKnownHostsFile="$KNOWN" \
            -o GlobalKnownHostsFile=/dev/null -o HostKeyAlias="$A" -o BatchMode=yes -o ForwardAgent=yes -o LogLevel=ERROR \
            "root@$(ip4 "$A")" 'echo ${SSH_AUTH_SOCK:-none}'
        ssh-agent -k >/dev/null
    )"
    check "sshd refuses to forward even a throwaway agent" test "$fwd" = none
    key1="$(guest "$A" cut -d' ' -f1-2 /etc/ssh/ssh_host_ed25519_key.pub)"
    sbx stop "$A"
    boot "$A"
    check "host key is stable across restart" test "$(guest "$A" cut -d' ' -f1-2 /etc/ssh/ssh_host_ed25519_key.pub)" = "$key1"
    check "strict SSH works after restart" test "$(pinned "$A" echo ok)" = ok
    guest "$A" sh -c 'rm -f /etc/ssh/ssh_host_* && ssh-keygen -A >/dev/null && systemctl restart ssh'
    check "a replaced host key is rejected" refuses pinned "$A" true
    enroll "$A"
fi

if want persistence; then
    echo ""
    echo "=== Phase: persistence ==="
    m="$(openssl rand -hex 8)"
    guest "$A" sh -c "echo $m > /var/lib/coop-test/marker && docker volume create coopvol >/dev/null && docker run --rm -v coopvol:/v alpine:3.20 sh -c 'echo $m > /v/m'"
    before="$(verify "$A" | jq -c '{machine_id, ssh_host_key}')"
    lost=""
    ips=()
    for ((c = 1; c <= CYCLES; c++)); do
        u="$(openssl rand -hex 4)"
        # An unsynced write immediately before a normal stop must survive it.
        guest "$A" sh -c "echo $u > /var/lib/coop-test/unsynced"
        sbx stop "$A"
        boot "$A" || { lost+=" boot@$c"; break; }
        [[ "$(guest "$A" cat /var/lib/coop-test/marker)" == "$m" ]] || lost+=" file@$c"
        [[ "$(guest "$A" cat /var/lib/coop-test/unsynced)" == "$u" ]] || lost+=" unsynced@$c"
        [[ "$(guest "$A" docker run --rm -v coopvol:/v alpine:3.20 cat /v/m)" == "$m" ]] || lost+=" docker@$c"
        [[ "$(verify "$A" | jq -c '{machine_id, ssh_host_key}')" == "$before" ]] || lost+=" identity@$c"
        ips+=("$(ip4 "$A")")
    done
    if [[ -z "$lost" ]]; then
        pass "$CYCLES stop/start cycles keep files, an unsynced pre-stop write, Docker state, machine-id, and host key"
    else
        fail "$CYCLES stop/start cycles keep files, an unsynced pre-stop write, Docker state, machine-id, and host key" "lost:$lost"
    fi
    check "the address is stable across restarts" test "$(printf '%s\n' "${ips[@]}" | sort -u | wc -l | tr -d ' ')" = 1
    n="$(name fresh)"
    create "$n"
    boot "$n"
    check "a new sandbox from the same image gets its own identity" \
        test "$(verify "$n" | jq -c '{machine_id, ssh_host_key}')" != "$before"
    sbx stop "$n"
    sbx delete "$n" --owner "$RUN"
fi

if want resources; then
    echo ""
    echo "=== Phase: resources ==="
    mid="$(guest "$A" cat /etc/machine-id)"
    sbx stop "$A"
    sbx set "$A" --cpus 2 --memory-mib 4096 >/dev/null
    boot "$A"
    v="$(verify "$A")"
    # The runtime adds one vCPU of its own.
    check "CPU change applies at the next start" test "$(jq -r .nproc <<<"$v")" = 3
    check "memory change applies at the next start" test "$(jq -r .mem_kb <<<"$v")" -lt $((4200 * 1024))
    check "the disk keeps its identity" test "$(jq -r .machine_id <<<"$v")" = "$mid"
    check "set refuses a running sandbox" refuses sbx set "$A" --cpus 1
    sbx stop "$A"
    sbx set "$A" --cpus 4 --memory-mib 8192 >/dev/null
    boot "$A"
fi

if want growth; then
    echo ""
    echo "=== Phase: growth ==="
    g="$(name grow)"
    create "$g" 2 2048 8
    boot "$g"
    m="$(openssl rand -hex 6)"
    guest "$g" sh -c "echo $m > /var/lib/coop-test/marker"
    key="$(guest "$g" cat /etc/ssh/ssh_host_ed25519_key.pub)"
    sbx stop "$g"
    check "grow refuses to shrink" refuses sbx grow "$g" --disk-gib 4
    check "8 -> 32 GiB grows offline" sbx grow "$g" --disk-gib 32
    boot "$g"
    check "the guest filesystem is 32 GiB" test "$(guest "$g" df -B1 --output=size / | tail -1 | xargs)" -ge $((31 * 1024 * 1024 * 1024))
    check "data and host key survive the grow" test "$(guest "$g" cat /var/lib/coop-test/marker)$(guest "$g" cat /etc/ssh/ssh_host_ed25519_key.pub)" = "$m$key"
    sbx stop "$g"
    # Same-sandbox races serialize in the runtime: of two identical grows,
    # exactly one applies; a start racing a grow either waits for it and boots
    # the grown disk, or wins and the grow is refused, never both.
    sbx grow "$g" --disk-gib 36 >/dev/null 2>&1 &
    g1=$!
    sbx grow "$g" --disk-gib 36 >/dev/null 2>&1 &
    g2=$!
    ok=0
    wait "$g1" && ok=$((ok + 1))
    wait "$g2" && ok=$((ok + 1))
    check "two concurrent grows of one sandbox apply once" test "$ok" -eq 1
    sbx grow "$g" --disk-gib 40 >/dev/null 2>&1 &
    g1=$!
    check "a start racing a grow boots" boot "$g"
    grew=0
    wait "$g1" && grew=1
    size="$(guest "$g" df -B1 --output=size / | tail -1 | xargs)"
    gib39=$((39 * 1024 * 1024 * 1024))
    serialized() { if ((grew)); then test "$size" -ge "$gib39"; else test "$size" -lt "$gib39"; fi; }
    check "start and grow serialize (grow $( ((grew)) && echo first || echo refused))" serialized
    sbx stop "$g"
    sbx delete "$g" --owner "$RUN"
fi

if want snapshots; then
    echo ""
    echo "=== Phase: snapshots ==="
    guest "$A" sh -c 'echo A > /var/lib/coop-test/state && docker volume create cp >/dev/null && docker run --rm -v cp:/v alpine:3.20 sh -c "echo A > /v/s"'
    mid="$(guest "$A" cat /etc/machine-id)"
    # Adversarial: a root guest disables its own `rm`; the identity reset must
    # not depend on the guest's tools.
    guest "$A" sh -c 'cp /usr/bin/rm /usr/bin/rm.coop-test && cp /usr/bin/true /usr/bin/rm && sync'
    sbx stop "$A"
    check "commit saves the stopped disk" sbx commit "$A" snap
    check "commit refuses an existing name without --replace" refuses sbx commit "$A" snap
    boot "$A"
    guest "$A" sh -c 'echo B > /var/lib/coop-test/state && docker run --rm -v cp:/v alpine:3.20 sh -c "echo B > /v/s" && echo x > /var/lib/coop-test/after && sync'
    sbx stop "$A"
    gen="$(sbx inspect "$A" | jq .record.diskGeneration)"
    check "restore replaces the disk" sbx restore "$A" snap
    check "restore bumps the disk generation" test "$(sbx inspect "$A" | jq .record.diskGeneration)" -gt "$gen"
    boot "$A"
    check "files and Docker volumes are back at the committed state" \
        test "$(guest "$A" cat /var/lib/coop-test/state)$(guest "$A" docker run --rm -v cp:/v alpine:3.20 cat /v/s)" = AA
    check "writes after the commit are gone" refuses guest "$A" test -e /var/lib/coop-test/after
    check "the restored disk generated a fresh identity, despite the guest's disabled rm" \
        test "$(guest "$A" cat /etc/machine-id)" != "$mid"
    guest "$A" sh -c 'cp /usr/bin/rm.coop-test /usr/bin/rm'
    c="$(name clone)"
    check "a new sandbox can be created from a committed disk" \
        sbx create "$c" --from-disk snap --cpus 2 --memory-mib 2048 --disk-gib 20 --owner "$RUN"
    boot "$c"
    check "it is grown to the requested size" test "$(guest "$c" df -B1 --output=size / | tail -1 | xargs)" -ge $((19 * 1024 * 1024 * 1024))
    check "it has its own identity" test "$(guest "$c" cat /etc/machine-id)" != "$mid"
    sbx stop "$c"
    sbx delete "$c" --owner "$RUN"
    sbx2 disk delete snap
    enroll "$A" 2>/dev/null || true
fi

if want recovery; then
    echo ""
    echo "=== Phase: recovery ==="
    r="$(name crash)"
    create "$r"
    # Owner killed during boot: a crashed state that start recovers.
    "$SANDBOX" run --root "$ROOT" "$r" >/dev/null 2>&1 &
    pid=$!
    sleep 0.3
    kill -9 "$pid" 2>/dev/null
    wait "$pid" 2>/dev/null
    sleep 1
    st="$(state "$r")"
    check "an owner killed during boot leaves a stopped or crashed sandbox" test "$st" = stopped -o "$st" = crashed
    check "start recovers it" boot "$r"
    # Owner killed while running: the VM dies with it and launchd respawns it.
    m="$(openssl rand -hex 4)"
    guest "$r" sh -c "echo $m > /var/lib/coop-test/crash && sync"
    old="$(sbx inspect "$r" | jq .live.pid)"
    kill -9 "$old"
    respawned=0
    for _ in $(seq 60); do
        now="$(sbx inspect "$r" | jq -r '.live.pid // empty')"
        [[ -n "$now" && "$now" != "$old" && "$(state "$r")" == running ]] && { respawned=1; break; }
        sleep 1
    done
    check "launchd respawns a killed owner" test "$respawned" = 1
    ready "$r"
    check "synced data survives the crash" test "$(guest "$r" cat /var/lib/coop-test/crash)" = "$m"
    # A client killed mid-stop does not stop the halt.
    "$SANDBOX" stop --root "$ROOT" "$r" >/dev/null 2>&1 &
    sleep 0.05
    kill -9 $! 2>/dev/null
    for _ in $(seq 100); do [[ "$(state "$r")" == stopped ]] && break; sleep 0.2; done
    sbx stop "$r" >/dev/null 2>&1
    check "a stop whose client was killed still ends stopped" test "$(state "$r")" = stopped
    # A create that never committed is removed by reconcile.
    mkdir -p "$ROOT/sandboxes/$(name half)"
    touch "$ROOT/sandboxes/$(name half)/rootfs.ext4"
    check "reconcile removes an uncommitted create" jq -e 'any(.action == "removed-uncommitted-create")' <<<"$(sbx reconcile)"
    check "delete refuses another owner" refuses sbx delete "$r" --owner someone-else
    check "delete removes the sandbox" sbx delete "$r" --owner "$RUN"

    # Interrupted mutations: SIGKILL the client of a grow, commit, or restore
    # at fractions of its uninterrupted duration, then reconcile. No staged
    # or scratch state may remain, and the record must describe the installed
    # disk
    # (docs/design/apple-sandbox-transactions.md INV-03, INV-04).
    t="$(name txn)"
    tdir="$ROOT/sandboxes/$t"
    create "$t"
    boot "$t"
    guest "$t" sh -c 'echo base > /var/lib/coop-test/txn && sync'
    sbx stop "$t"
    sbx commit "$t" txn-base >/dev/null
    rec() { sbx inspect "$t" | jq -r ".record.$1"; }
    # interrupt DELAY CMD...: run CMD, SIGKILL it after DELAY seconds, reconcile.
    interrupt() {
        local d="$1" pid
        shift
        "$@" >/dev/null 2>&1 &
        pid=$!
        sleep "$d"
        kill -9 "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
        sbx reconcile >/dev/null
    }
    settled() {
        [[ ! -e "$tdir/disk-update.pending.json" ]] &&
            [[ -z "$(find "$tdir" "$ROOT/disks" -maxdepth 1 \( -name '.update-*' -o -name '.tmp-*' -o -name '.pending-*' \
                -o -name '.grow-*' -o -name '.restore-*' -o -name '.maintenance-*' \) 2>/dev/null)" ]]
    }
    # The installed disk file: inode and size. `create` sizes it from the
    # image unpacker, so only a disk a grow or restore installed has exactly
    # the recorded size.
    disk() { stat -f '%i %z' "$tdir/rootfs.ext4"; }
    # installed APPLIED BEFORE: an applied update installed a new disk of the
    # recorded size; any other outcome left the previous disk in place.
    installed() {
        local now inode size
        now="$(disk)"
        read -r inode size <<<"$now"
        if (($1)); then
            test "${inode}" != "${2%% *}" -a "$size" = "$(rec diskBytes)"
        else
            test "$now" = "$2"
        fi
    }
    fs_matches_record() {
        local size
        size="$(guest "$t" df -B1 --output=size / | tail -1 | xargs)"
        test "${size:-0}" -ge $(($(rec diskBytes) * 95 / 100)) -a "${size:-0}" -le "$(rec diskBytes)"
    }

    ms="$(timed_ms sbx grow "$t" --disk-gib 9)"
    bad=""
    applied=0
    tries=0
    for f in $KILL_FRACTIONS; do
        d="$(delay_s "$ms" "$f")"
        tries=$((tries + 1))
        op="grow-$RUN-$tries"
        before="$(rec diskBytes)"
        file="$(disk)"
        target=$((before / 1073741824 + 2))
        interrupt "$d" "$SANDBOX" grow --root "$ROOT" "$t" --disk-gib "$target" --operation "$op"
        settled || bad+=" unsettled@$d"
        if [[ "$(rec lastOperation)" == "$op" ]]; then
            applied=$((applied + 1))
            [[ "$(rec diskBytes)" == $((target * 1073741824)) ]] || bad+=" record@$d"
            installed 1 "$file" || bad+=" disk@$d"
        else
            [[ "$(rec diskBytes)" == "$before" ]] || bad+=" record@$d"
            installed 0 "$file" || bad+=" disk@$d"
        fi
    done
    label="killed grows settle to the old or the new disk ($applied of $tries applied; uninterrupted ${ms} ms)"
    if [[ -z "$bad" ]]; then pass "$label"; else fail "$label" "$bad"; fi
    # Timed here, while the disk still holds the base content.
    restore_ms="$(timed_ms sbx restore "$t" txn-base)"
    check "the sandbox boots after the interrupted grows" boot "$t"
    check "its filesystem matches the recorded disk size" fs_matches_record
    guest "$t" sh -c 'echo newer > /var/lib/coop-test/txn && sync'
    sbx stop "$t"

    ms="$(timed_ms sbx commit "$t" txn-timed)"
    sbx2 disk delete txn-timed
    bad=""
    applied=0
    tries=0
    for f in $KILL_FRACTIONS; do
        d="$(delay_s "$ms" "$f")"
        tries=$((tries + 1))
        interrupt "$d" "$SANDBOX" commit --root "$ROOT" "$t" "txn-c$tries"
        settled || bad+=" unsettled@$d"
        disk="$ROOT/disks/txn-c$tries"
        if [[ -e "$disk.ext4" && -e "$disk.json" ]]; then
            applied=$((applied + 1))
            sbx2 disk delete "txn-c$tries"
        elif [[ -e "$disk.ext4" || -e "$disk.json" ]]; then
            bad+=" half-published@$d"
        fi
    done
    label="killed commits publish a whole disk or none ($applied of $tries applied; uninterrupted ${ms} ms)"
    if [[ -z "$bad" ]]; then pass "$label"; else fail "$label" "$bad"; fi

    ms="$restore_ms"
    bad=""
    applied=0
    tries=0
    for f in $KILL_FRACTIONS; do
        d="$(delay_s "$ms" "$f")"
        tries=$((tries + 1))
        op="restore-$RUN-$tries"
        gen="$(rec diskGeneration)"
        file="$(disk)"
        interrupt "$d" "$SANDBOX" restore --root "$ROOT" "$t" txn-base --operation "$op"
        settled || bad+=" unsettled@$d"
        if [[ "$(rec lastOperation)" == "$op" ]]; then
            applied=$((applied + 1))
            [[ "$(rec diskGeneration)" == $((gen + 1)) ]] || bad+=" generation@$d"
            # Not size: a restore keeps a committed disk's own size when it
            # needs no growth.
            [[ "$(disk | cut -d' ' -f1)" != "${file%% *}" ]] || bad+=" disk@$d"
        else
            [[ "$(rec diskGeneration)" == "$gen" ]] || bad+=" generation@$d"
            installed 0 "$file" || bad+=" disk@$d"
        fi
    done
    label="killed restores settle to the old or the new disk ($applied of $tries applied; uninterrupted ${ms} ms)"
    if [[ -z "$bad" ]]; then pass "$label"; else fail "$label" "$bad"; fi
    check "the sandbox boots after the interrupted restores" boot "$t"
    expected=newer
    ((applied > 0)) && expected=base
    check "its content is the $expected disk the record describes" test "$(guest "$t" cat /var/lib/coop-test/txn)" = "$expected"
    check "its filesystem matches the recorded disk size" fs_matches_record
    sbx stop "$t"
    check "an uninterrupted grow still applies" sbx grow "$t" --disk-gib $(($(rec diskBytes) / 1073741824 + 1))
    sbx delete "$t" --owner "$RUN"
    sbx2 disk delete txn-base
fi

if want concurrency; then
    echo ""
    echo "=== Phase: concurrency ==="
    # Each round boots COUNT sandboxes at once beside B, the fixed peer, so a
    # one-sandbox round still has a neighbour. Every new sandbox attacks B and
    # its ring successor, and B attacks the first, with the full peer probe.
    # Attackers run in parallel; each one's probes run in sequence.
    sbx stop "$A" >/dev/null 2>&1
    [[ "$(state "$B")" == running ]] || boot "$B"
    listeners "$B"
    for count in $CONCURRENCY; do
        names=()
        for ((i = 1; i <= count; i++)); do
            names+=("$(name "n${count}c$i")")
            create "$(name "n${count}c$i")" 2 1024 8
        done
        for n in "${names[@]}"; do sbx start "$n" >/dev/null & done
        wait
        all=1
        for n in "${names[@]}"; do ready "$n" || all=0; done
        check "$count at once: all boot" test "$all" = 1
        check "$count at once: each has its own address" \
            test "$( { ip4 "$B"; for n in "${names[@]}"; do ip4 "$n"; done; } | sort -u | wc -l | tr -d ' ')" = $((count + 1))
        check "$count at once: each has its own subnet" \
            test "$(for n in "$B" "${names[@]}"; do sbx inspect "$n" | jq -r '.effective.interfaces[0].network'; done | sort -u | wc -l | tr -d ' ')" = $((count + 1))
        for n in "${names[@]}"; do listeners "$n"; done
        out="$WORK/concurrency-$count"
        mkdir -p "$out"
        for ((i = 0; i < count; i++)); do
            (
                probe_pair "${names[i]}" "$B" >"$out/c$((i + 1))-B"
                if ((count > 1)); then
                    probe_pair "${names[i]}" "${names[(i + 1) % count]}" >"$out/c$((i + 1))-c$(((i + 1) % count + 1))"
                fi
            ) &
        done
        probe_pair "$B" "${names[0]}" >"$out/B-c1" &
        wait
        bad=""
        pairs=0
        for f in "$out"/*; do
            pairs=$((pairs + 1))
            isolated "$(<"$f")" || bad+=" ${f##*/}=$(<"$f")"
        done
        if [[ -z "$bad" ]]; then
            pass "$count at once: all $pairs directed pairs blocked on every vector, host reaches every target"
        else
            fail "$count at once: all $pairs directed pairs blocked on every vector, host reaches every target" \
                "attacker-target=reached|host misses|probes:$bad"
        fi
        for n in "${names[@]}"; do sbx stop "$n" >/dev/null; sbx delete "$n" --owner "$RUN"; done
    done
fi

if want coop; then
    echo ""
    echo "=== Phase: coop ==="
    # Free the host for coop's own sandbox.
    sbx stop "$A" >/dev/null 2>&1
    sbx stop "$B" >/dev/null 2>&1
    kernel="$(readlink -f "$HOME/Library/Application Support/com.apple.container/kernels/default.kernel-arm64")"
    write_cfg() {
        printf '%s\n' "data_dir = \"$CDATA\"" 'github = "off"' '' '[vm]' 'vcpu_count = 2' \
            'mem_size_mib = 2048' 'template_size_gib = 8' '' '[apple_container]' \
            "binary = \"$SANDBOX\"" "builder = \"$CONTAINER\"" "kernel = \"$kernel\"" "$@"
    }
    write_cfg >"$CCFG"
    write_cfg 'boot_timeout_seconds = 15' >"$CCFG_FAIL"
    mkdir -p "$WORK/project"
    echo "$RUN" >"$WORK/project/marker"
    if cargo build --quiet --features apple-container --target-dir "$WORK/target" >"$WORK/coop-build.log" 2>&1 &&
        coop setup -y >"$WORK/coop-setup.log" 2>&1; then
        pass "coop setup builds, verifies, and publishes the image"
    else
        fail "coop setup builds, verifies, and publishes the image" "see $WORK/coop-build.log, $WORK/coop-setup.log"
        summary
    fi
    if coop up "$WORK/project" --name e2e --no-agents --no-github >"$WORK/coop-up.log" 2>&1; then
        pass "coop up creates and boots an instance"
    else
        fail "coop up creates and boots an instance" "see $WORK/coop-up.log"
        summary
    fi
    check "status reports running on the apple-container backend" \
        test "$(coop status e2e --json | jq -r '"\(.state) \(.backend)"')" = "running apple-container"
    check "the workspace is copied in" test "$(coop exec e2e -- cat /workspace/marker)" = "$RUN"
    coop exec e2e -- sh -c 'echo before > ~/snap-before' >/dev/null
    check "coop stop stops the sandbox" coop stop e2e
    check "status reports stopped" test "$(cstate e2e)" = stopped
    check "coop start boots it again" coop start e2e --no-agents --no-github
    check "guest data survives stop/start" test "$(coop exec e2e -- sh -c 'cat ~/snap-before' 2>/dev/null)" = before

    # What coop's own sandbox exposes, with a live agent and the canary in
    # coop's environment.
    mid="$(machine_id e2e)"
    eff="$(csbx inspect "$mid" | jq .effective)"
    check "coop's sandbox: kernel pseudo-filesystems only" \
        jq -e '[.mounts[] | select(.type | IN("proc","sysfs","devtmpfs","mqueue","tmpfs","cgroup2","devpts") | not)] | length == 0' <<<"$eff"
    check "coop's sandbox: no relays, ports, or agent forwarding" \
        jq -e '.socketRelays == 0 and .publishedPorts == 0 and .sshAgentForwarding == false' <<<"$eff"
    mi="$(coop exec e2e -- cat /proc/self/mountinfo)"
    check "coop's guest: no file-sharing mounts or host paths" \
        refuses grep -Eq ' - (virtiofs|9p|fuse|fuse\.[^ ]+|nfs4?|cifs|smb3?|smbfs) |/Users/' <<<"$mi"
    # shellcheck disable=SC2016 # Expand in the guest.
    agent="$(
        eval "$(ssh-agent -s)" >/dev/null
        coop exec e2e -- sh -c 'echo ${SSH_AUTH_SOCK:-none}'
        ssh-agent -k >/dev/null
    )"
    check "coop exec forwards no host agent" test "$agent" = none
    # The pattern splits the canary with an empty group: sudo logs its
    # command line to the guest journal, which must not be a match.
    pattern="${CANARY:0:24}()${CANARY:24}"
    leaks="$(coop exec e2e -- sudo sh -c "grep -rlsE '$pattern' / --exclude-dir=proc --exclude-dir=sys --exclude-dir=dev | head -3
        cat /proc/[0-9]*/environ 2>/dev/null | tr '\0' '\n' | grep -cE '$pattern'")"
    if [[ "$leaks" == 0 ]]; then
        pass "the canary in coop's environment reaches no guest file or process"
    else
        fail "the canary in coop's environment reaches no guest file or process" "$(tr '\n' ' ' <<<"$leaks")"
    fi

    # Pinned identity: coop refuses a guest whose host key changed, both on a
    # live connection and at the next start.
    coop exec e2e -- sudo sh -c 'cp -a /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub /root/ &&
        rm -f /etc/ssh/ssh_host_ed25519_key /etc/ssh/ssh_host_ed25519_key.pub &&
        ssh-keygen -q -t ed25519 -N "" -f /etc/ssh/ssh_host_ed25519_key && systemctl restart ssh' >/dev/null 2>&1
    check "coop exec refuses a changed host key" refuses coop exec e2e -- true
    coop stop e2e >/dev/null 2>&1
    changed="$(coop start e2e --no-agents --no-github 2>&1)"
    check "coop start refuses a changed host key" grep -q APPLE_HOST_KEY_CHANGED <<<"$changed"
    check "the refused start leaves the sandbox stopped" test "$(csbx inspect "$mid" | jq -r .status)" = stopped
    # repair CMD...: run CMD as root over the runtime's own channel, which
    # needs no SSH, with the sandbox stopped before and after.
    repair() {
        csbx start "$mid" >/dev/null
        for _ in $(seq 100); do csbx exec "$mid" -- true >/dev/null 2>&1 && break; sleep 0.2; done
        csbx exec "$mid" -- "$@" >/dev/null 2>&1
        csbx stop "$mid" >/dev/null
    }
    repair cp -a /root/ssh_host_ed25519_key /root/ssh_host_ed25519_key.pub /etc/ssh/
    check "the pinned key restored, coop starts again" coop start e2e --no-agents --no-github

    # sshd will not start on the next boot, so a restart after a resize fails.
    coop exec e2e -- sudo systemctl mask ssh.service ssh.socket >/dev/null 2>&1
    coop stop e2e >/dev/null 2>&1
    check "resize --mem/--vcpus records the change" coop resize e2e --mem 3072 --vcpus 3
    check "the runtime record holds the new memory" test "$(record e2e memoryBytes)" = $((3072 * 1024 * 1024))
    check "a failed resize --start is refused" \
        refuses "$COOP" --config "$CCFG_FAIL" resize e2e --mem 4096 --start
    check "the failed restart leaves the sandbox stopped" test "$(csbx inspect "$(machine_id e2e)" | jq -r .status)" = stopped
    check "the failed restart rolls the memory back" test "$(record e2e memoryBytes)" = $((3072 * 1024 * 1024))
    check "no journal is left behind" test ! -e "$CSTATE/instances/e2e/operation.json"
    repair systemctl unmask ssh.service ssh.socket
    resize_ms="$(timed_ms coop resize e2e --size 12)"
    check "resize --size grows the disk" test "$(record e2e diskBytes)" = $((12 * 1073741824))
    check "start after the resizes succeeds" coop start e2e --no-agents --no-github
    check "the guest sees the new vCPU count (+1 runtime vCPU)" test "$(coop exec e2e -- nproc)" = 4
    size="$(coop exec e2e -- df -B1 --output=size / | tail -1 | xargs)"
    check "the guest sees the grown disk" test "${size:-0}" -ge $((12 * 1024 * 1024 * 1024 * 95 / 100))

    coop stop e2e >/dev/null 2>&1
    check "coop commit saves an image" coop commit e2e --image e2e-snap
    coop start e2e --no-agents --no-github >/dev/null 2>&1
    coop exec e2e -- sh -c 'echo after > ~/snap-after' >/dev/null
    coop stop e2e >/dev/null 2>&1
    gen="$(record e2e diskGeneration)"
    restore_ms="$(timed_ms coop restore e2e --image e2e-snap)"
    check "coop restore replaces the disk" test "$(record e2e diskGeneration)" -gt "$gen"
    check "start after restore re-pins the new host key" coop start e2e --no-agents --no-github
    check "restore keeps data from before the commit" test "$(coop exec e2e -- sh -c 'cat ~/snap-before' 2>/dev/null)" = before
    check "restore drops data written after the commit" \
        test "$(coop exec e2e -- sh -c 'test -e ~/snap-after && echo present || echo absent')" = absent

    # Interrupted coop mutations: SIGKILL coop and its runtime client partway
    # through; the next start reconciles coop's journal with the runtime.
    # kill_coop DELAY ARGS...: run coop ARGS, kill it and its children after DELAY.
    kill_coop() {
        local d="$1" pid
        shift
        "$COOP" --config "$CCFG" "$@" </dev/null >/dev/null 2>&1 &
        pid=$!
        sleep "$d"
        pkill -9 -P "$pid" 2>/dev/null
        kill -9 "$pid" 2>/dev/null
        wait "$pid" 2>/dev/null
    }
    gib=12
    for f in $COOP_KILL_FRACTIONS; do
        for op in restore resize; do
            coop stop e2e >/dev/null 2>&1
            if [[ "$op" == restore ]]; then
                d="$(delay_s "$restore_ms" "$f")"
                kill_coop "$d" restore e2e --image e2e-snap
            else
                d="$(delay_s "$resize_ms" "$f")"
                gib=$((gib + 1))
                kill_coop "$d" resize e2e --size "$gib"
            fi
            check "a $op killed after ${d}s: the next start recovers" coop start e2e --no-agents --no-github
            check "a $op killed after ${d}s: no journal is left" test ! -e "$CSTATE/instances/e2e/operation.json"
            check "a $op killed after ${d}s: pinned SSH works" test "$(coop exec e2e -- echo ok 2>/dev/null)" = ok
            size="$(coop exec e2e -- df -B1 --output=size / | tail -1 | xargs)"
            check "a $op killed after ${d}s: the filesystem matches the record" \
                test "${size:-0}" -ge $(($(record e2e diskBytes) * 95 / 100))
        done
    done

    # A disk-generation increase coop did not make does not authorize a new
    # host key (INV-07): an out-of-band restore resets the guest's identity.
    coop stop e2e >/dev/null 2>&1
    mid="$(machine_id e2e)"
    csbx commit "$mid" oob >/dev/null && csbx restore "$mid" oob >/dev/null
    check "coop start refuses a restore it did not make" refuses coop start e2e --no-agents --no-github
    check "that refused start leaves the sandbox stopped" test "$(csbx inspect "$mid" | jq -r .status)" = stopped
    "$SANDBOX" disk delete --root "$CROOT" oob >/dev/null 2>&1

    check "coop destroy removes the instance" coop destroy e2e
    check "the runtime has no sandbox left" test "$(csbx list | jq length)" = 0
    check "the instance state is gone" test ! -e "$CSTATE/instances/e2e"
    check "the committed image can be deleted" coop images --delete e2e-snap
fi

summary
