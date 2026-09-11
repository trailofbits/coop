#!/usr/bin/env bash
set -euo pipefail

# Real OpenSSH, no VM: a disposable guest network namespace separates the
# reverse listener from the host destination on the same loopback port.
if [[ "$(uname -s)" != Linux ]]; then
    echo "SKIP: proxy reverse-forward test requires Linux namespaces"
    exit 0
fi
for tool in cargo python3 sudo unshare timeout mount ip ssh sshd ssh-keygen hostname cp chmod mkdir grep; do
    command -v "$tool" >/dev/null || { echo "Missing prerequisite: $tool" >&2; exit 1; }
done
cd "$(dirname "$0")/.."
artifacts=$(mktemp)
trap 'rm -f "$artifacts"' EXIT
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
test_name=proxy::tests::reverse_forward_requires_authenticated_bind_acknowledgment
"$binary" --list --ignored --exact "$test_name" | grep -Fx "$test_name: test"
sudo -n timeout --kill-after=5s 60s \
    unshare --mount --net --uts --pid --fork --mount-proc --kill-child \
    bash -s -- "$binary" "$test_name" "$(command -v ssh)" "$(command -v sshd)" <<'INNER'
set -euo pipefail
mount --make-rprivate /
hostname localhost
mount -t tmpfs -o mode=755 tmpfs /run
# Preserve the executable before covering /tmp (Cargo targets may live there).
cp "$1" /run/coop-test
mount -t tmpfs tmpfs /tmp
mkdir -p /run/netns /run/sshd /run/coop-forward/bin
export COOP_FORWARD_TEST_DIR=/run/coop-forward
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
PidFile $COOP_FORWARD_TEST_DIR/sshd.pid
LogLevel ERROR
CONFIG
# Exec the real client, suppress all user/system config, and record the direct
# master PID independently of the production pidfile under test.
export COOP_FORWARD_TEST_SSH="$3"
cat >"$COOP_FORWARD_TEST_DIR/bin/ssh" <<'WRAPPER'
#!/bin/sh
case " $* " in *' -N '*) echo $$ >"$COOP_FORWARD_TEST_DIR/master.pid" ;; esac
exec "$COOP_FORWARD_TEST_SSH" -F /dev/null "$@"
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
/run/coop-test --ignored --exact "$2" --nocapture
INNER
