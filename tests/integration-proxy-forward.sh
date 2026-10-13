#!/usr/bin/env bash
set -euo pipefail

# Real OpenSSH, no VM: a disposable guest network namespace separates the
# reverse listener from the host destination on the same loopback port.
if [[ "$(uname -s)" != Linux ]]; then
    echo "SKIP: proxy reverse-forward test requires Linux namespaces"
    exit 0
fi
required_tools=(python3 sudo unshare timeout mount ip ssh sshd ssh-keygen hostname cp chmod mkdir grep)
if [[ -z "${COOP_TEST_PREBUILT_BINARY:-}" ]]; then
    required_tools+=(cargo)
fi
for tool in "${required_tools[@]}"; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
done
cd "$(dirname "$0")/.."
artifacts=""
trap '[[ -z "$artifacts" ]] || rm -f "$artifacts"' EXIT
if [[ -n "${COOP_TEST_PREBUILT_BINARY:-}" ]]; then
    binary=$COOP_TEST_PREBUILT_BINARY
else
    artifacts=$(mktemp)
    cargo test --locked --lib --no-run --message-format=json >"$artifacts"
    binary=$(python3 - "$artifacts" <<'PY'
import json
import sys
with open(sys.argv[1]) as source:
    tests = [item["executable"] for line in source
             if (item := json.loads(line)).get("reason") == "compiler-artifact"
             and item.get("executable") and item["target"]["name"] == "coop"
             and item["profile"]["test"]]
if len(tests) != 1:
    raise SystemExit(f"Expected one coop library test executable, found {tests!r}")
print(tests[0])
PY
    )
fi
test_name=proxy::tests::reverse_forward_requires_authenticated_bind_acknowledgment
env_test_name=backend::tests::forwarding_over_openssh_preserves_session_behavior
"$binary" --list --ignored --exact "$test_name" | grep -Fx "$test_name: test"
"$binary" --list --ignored --exact "$env_test_name" | grep -Fx "$env_test_name: test"
sudo -n timeout --kill-after=5s 60s \
    unshare --mount --net --uts --pid --fork --mount-proc --kill-child \
    bash -s -- "$binary" "$test_name" "$(command -v ssh)" "$(command -v sshd)" "$env_test_name" <<'INNER'
set -euo pipefail
mount --make-rprivate /
hostname localhost
mount -t tmpfs -o mode=755 tmpfs /run
# Preserve the executable before covering /tmp (Cargo targets may live there).
cp "$1" /run/coop-test
mount -t tmpfs tmpfs /tmp
mkdir -p /run/netns /run/sshd /run/coop-forward/bin
export COOP_FORWARD_TEST_DIR=/run/coop-forward
export COOP_FORWARD_TEST_AMBIENT_PORT=17991
chmod 700 "$COOP_FORWARD_TEST_DIR"
ip link set lo up
ip netns add guest
ip link add host0 type veth peer name guest0 netns guest
ip addr add 192.0.2.1/24 dev host0
ip link set host0 up
ip -n guest addr add 192.0.2.2/24 dev guest0
ip -n guest link set guest0 up
ip -n guest link set lo up
for key in host client; do
    ssh-keygen -q -t ed25519 -N '' -f "$COOP_FORWARD_TEST_DIR/$key"
done
cat >"$COOP_FORWARD_TEST_DIR/sshd_config" <<CONFIG
ListenAddress 192.0.2.2
Port 2222
HostKey $COOP_FORWARD_TEST_DIR/host
AuthorizedKeysFile $COOP_FORWARD_TEST_DIR/client.pub
StrictModes yes
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin prohibit-password
UsePAM yes
AllowUsers root
AllowTcpForwarding remote
GatewayPorts clientspecified
AcceptEnv COOP_SSH_ENV_*
AcceptEnv COOP_AMBIENT_SENTINEL
PidFile $COOP_FORWARD_TEST_DIR/sshd.pid
LogLevel ERROR
CONFIG
# Install hostile system client policy inside the disposable mount namespace.
# The production `-F none` boundary must suppress it completely.
cat >"$COOP_FORWARD_TEST_DIR/hostile_ssh_config" <<CONFIG
Host *
    SendEnv *
    SetEnv COOP_AMBIENT_SENTINEL=ambient-config-value-must-not-reach-guest
    RemoteForward 0.0.0.0:$COOP_FORWARD_TEST_AMBIENT_PORT 127.0.0.1:9
    PermitLocalCommand yes
    LocalCommand /usr/bin/touch $COOP_FORWARD_TEST_DIR/ambient-policy-ran
CONFIG
mount --bind "$COOP_FORWARD_TEST_DIR/hostile_ssh_config" /etc/ssh/ssh_config
export COOP_AMBIENT_SENTINEL=ambient-host-value-must-not-reach-guest

# Exec the real client and record the direct master PID independently of the
# production pidfile under test. Use a fixed path because the production
# builder deliberately clears the wrapper's environment.
cp "$3" "$COOP_FORWARD_TEST_DIR/bin/real-ssh"
chmod 700 "$COOP_FORWARD_TEST_DIR/bin/real-ssh"
cat >"$COOP_FORWARD_TEST_DIR/bin/ssh" <<'WRAPPER'
#!/bin/sh
[ -z "${COOP_AMBIENT_SENTINEL-}" ] || touch /run/coop-forward/inherited-env-ran
case " $* " in *' -N '*) echo $$ >/run/coop-forward/master.pid ;; esac
exec /run/coop-forward/bin/real-ssh "$@"
WRAPPER
chmod 700 "$COOP_FORWARD_TEST_DIR/bin/ssh"
export PATH="$COOP_FORWARD_TEST_DIR/bin:$PATH"
ip netns exec guest "$4" -D -e -f "$COOP_FORWARD_TEST_DIR/sshd_config" &
# Readiness probes only the isolated fixture, never a host SSH daemon.
python3 - <<'PY'
import socket,time
end = time.monotonic() + 5
while True:
    try:
        with socket.create_connection(('192.0.2.2', 2222), timeout=.2):
            break
    except OSError:
        if time.monotonic() >= end:
            raise
        time.sleep(.02)
PY
# Namespace init exit kills every descendant, including on test panic/timeout.
echo "Running authenticated reverse-forward test"
/run/coop-test --ignored --exact "$2" --nocapture
echo "Running isolated environment transport test"
/run/coop-test --ignored --exact "$5" --nocapture
if [[ -e "$COOP_FORWARD_TEST_DIR/ambient-policy-ran" ]]; then
    echo "hostile SSH LocalCommand executed" >&2
    exit 1
fi
if [[ -e "$COOP_FORWARD_TEST_DIR/inherited-env-ran" ]]; then
    echo "ambient host environment reached the managed SSH process" >&2
    exit 1
fi
INNER
