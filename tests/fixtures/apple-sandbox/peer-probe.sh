#!/usr/bin/env bash
# Runs as root inside the attacking guest. Args: <target-ipv4> <target-ipv6> <target-mac-or-empty> [target-ipv6-link-local]
# Prints one JSON line per probe: {"probe":..,"reached":true|false,"detail":..}
set -uo pipefail
[[ -n "${1:-}" && -n "${2:-}" ]] || { echo "usage: $0 <ipv4> <ipv6> [mac] [ipv6-link-local]" >&2; exit 2; }
T4="$1"; T6="$2"; TMAC="${3:-}"; T6LL="${4:-$2}"
emit() { jq -cn --arg p "$1" --argjson r "$2" --arg d "${3:-}" '{probe:$p,reached:$r,detail:$d}'; }
tcp() { local out; out="$(timeout 4 nc "$@" 7777 </dev/null 2>/dev/null)"; [[ "$out" == pong ]]; }
udp() { local out; out="$(echo x | timeout 4 nc -u -w2 "$@" 7778 2>/dev/null)"; [[ "$out" == upong ]]; }
icmp() { ping -c2 -W2 "$@" >/dev/null 2>&1; }
# probe NAME CMD...: emit whether CMD reached the target.
probe() { local name="$1"; shift; if "$@"; then emit "$name" true; else emit "$name" false; fi; }

probe ipv4-tcp tcp -w2 "$T4"
probe ipv4-udp udp "$T4"
probe ipv4-icmp icmp "$T4"
probe ipv6-tcp tcp -6 -w2 "$T6"
probe ipv6-udp udp -6 "$T6"
probe ipv6-icmp icmp -6 "$T6"

# Forged on-link routes: claim the target is directly on eth0 (bypass gateway).
ip route add "$T4/32" dev eth0 2>/dev/null
probe ipv4-onlink-route-tcp tcp -w2 "$T4"
probe ipv4-onlink-route-icmp icmp "$T4"
ip -6 route add "$T6/128" dev eth0 2>/dev/null
probe ipv6-onlink-route-tcp tcp -6 -w2 "$T6"

# Neighbor manipulation: a static ARP/NDP entry for the target with its real
# MAC (if known), then retry L2-direct.
if [[ -n "$TMAC" ]]; then
    ip neigh replace "$T4" lladdr "$TMAC" dev eth0 nud permanent 2>/dev/null
    ip -6 neigh replace "$T6" lladdr "$TMAC" dev eth0 nud permanent 2>/dev/null
    probe ipv4-static-neigh-tcp tcp -w2 "$T4"
    probe ipv6-static-neigh-tcp tcp -6 -w2 "$T6"
    ip neigh del "$T4" dev eth0 2>/dev/null; ip -6 neigh del "$T6" dev eth0 2>/dev/null
fi
ip route del "$T4/32" dev eth0 2>/dev/null; ip -6 route del "$T6/128" dev eth0 2>/dev/null

# Source spoofing: move our own address into the target's subnet and send.
T4NET="${T4%.*}"
SPOOF="$T4NET.250"
ip addr add "$SPOOF/24" dev eth0 2>/dev/null
probe ipv4-spoofed-src-tcp tcp -s "$SPOOF" -w2 "$T4"
probe ipv4-spoofed-src-icmp icmp -I "$SPOOF" "$T4"
ip addr del "$SPOOF/24" dev eth0 2>/dev/null

# Broadcast / multicast from our segment toward anything listening.
# Only a reply *from the target* counts; the gateway/self answering is expected.
sysctl -qw net.ipv4.icmp_echo_ignore_broadcasts=0 2>/dev/null
bcast="$(ip -4 -o addr show eth0 | awk '{for (i = 1; i < NF; i++) if ($i == "brd") print $(i + 1)}')"
for dst in 255.255.255.255 ${bcast:+$bcast} "$T4NET.255" 224.0.0.1; do
    reply="$(ping -b -c2 -W2 "$dst" 2>/dev/null | grep -o 'from [0-9.]*' | sort -u | tr '\n' ' ')"
    if grep -qwF "$T4" <<<"$reply"; then emit "ipv4-bcast-mcast:$dst" true "$reply"; else emit "ipv4-bcast-mcast:$dst" false "$reply"; fi
done
reply="$(ping -6 -c2 -W2 ff02::1%eth0 2>/dev/null | grep -o 'from [^ ]*' | sort -u | tr '\n' ' ')"
if grep -qiF -e "$T6" -e "$T6LL" <<<"$reply"; then emit ipv6-allnodes-multicast true "$reply"; else emit ipv6-allnodes-multicast false "$reply"; fi
