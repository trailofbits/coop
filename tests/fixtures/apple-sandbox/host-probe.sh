#!/usr/bin/env bash
# Host-side positive control for peer isolation (tests/integration-apple-sandbox.sh): can the host reach the
# target's listeners? Args: <ipv4> <ipv6>. Prints one JSON object.
# System binaries only: macOS Local Network privacy blocks unentitled
# interpreters (e.g. python3) from reaching vmnet guests.
set -uo pipefail
v4="$1"; v6="$2"
tcp() { [[ "$(/usr/bin/nc "$@" -w2 7777 </dev/null 2>/dev/null)" == pong ]] && echo true || echo false; }
udp() { [[ "$( (echo x; sleep 2) | /usr/bin/nc -u "$@" -w2 7778 2>/dev/null | head -1)" == upong ]] && echo true || echo false; }
png() { "$@" >/dev/null 2>&1 && echo true || echo false; }
jq -n --argjson t4 "$(tcp "$v4")" --argjson u4 "$(udp "$v4")" --argjson i4 "$(png /sbin/ping -c1 -t2 "$v4")" \
      --argjson t6 "$(tcp -6 "$v6")" --argjson u6 "$(udp -6 "$v6")" --argjson i6 "$(png /sbin/ping6 -c1 -i1 "$v6")" \
      '{"ipv4-tcp":$t4, "ipv4-udp":$u4, "ipv4-icmp":$i4, "ipv6-tcp":$t6, "ipv6-udp":$u6, "ipv6-icmp":$i6}'
