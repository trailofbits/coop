#!/usr/bin/env bash
set -euo pipefail

# Host-only Linux bridge test. Build unprivileged; elevate only the disposable
# network/mount/PID namespace. No KVM, VM images, or running coop instances needed.
if [[ "$(uname -s)" != Linux ]]; then
    echo "SKIP: bridge isolation requires Linux"
    exit 0
fi
for tool in cargo python3 sudo unshare timeout mount ip bridge iptables ping readlink hostname; do
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
test_name=network::isolation_tests::bridge_port_isolation_blocks_peers_without_firewall
# An exact filter matching zero tests otherwise exits successfully.
"$binary" --list --ignored --exact "$test_name" | grep -Fx "$test_name: test"
host_ns=$(readlink /proc/self/ns/net)
sudo -n timeout --kill-after=5s 60s \
    unshare --mount --net --uts --pid --fork --mount-proc --kill-child \
    bash -s -- "$host_ns" "$binary" "$test_name" <<'INNER'
set -euo pipefail
export COOP_NETWORK_TEST_HOST_NS="$1"
[[ "$(readlink /proc/self/ns/net)" != "$COOP_NETWORK_TEST_HOST_NS" ]]
# Named namespaces and their bind mounts live only in this private mount tree.
mount --make-rprivate /
# sudo resolves the hostname even for root; no host DNS is reachable here.
hostname localhost
mount -t tmpfs tmpfs /run
mkdir -p /run/netns
ip link add br0 type bridge
ip addr add 192.0.2.1/24 dev br0
ip link set br0 up
for endpoint in a b; do
    ip netns add "guest-$endpoint"
    ip link add "port-$endpoint" type veth peer name eth0 netns "guest-$endpoint"
    ip link set "port-$endpoint" master br0
    ip link set "port-$endpoint" up
    ip -n "guest-$endpoint" link set lo up
    ip -n "guest-$endpoint" link set eth0 up
done
ip -n guest-a addr add 192.0.2.2/24 dev eth0
ip -n guest-b addr add 192.0.2.3/24 dev eth0
# No routing or firewall rules: only the production bridge-port helper may
# change peer reachability. Namespace exit removes all links and mounts.
"$2" --ignored --exact "$3" --nocapture
INNER
