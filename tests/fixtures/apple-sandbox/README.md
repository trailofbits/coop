# Apple sandbox integration fixtures

Used by [`tests/integration-apple-sandbox.sh`](../../integration-apple-sandbox.sh),
which boots real `coop-sandbox` VMs (parser fixtures for the unit tests are in
[`../coop-sandbox`](../coop-sandbox)):

- `image/` — the small test image the suite builds with stock `container`:
  systemd, sshd, Docker, and the probe tools. `verify.sh` checks it from inside
  the guest.
- `peer-probe.sh` — run as root in one sandbox against another; prints one
  JSON line per isolation probe (TCP/UDP/ICMP over IPv4/IPv6, forged routes,
  static neighbours, spoofed source, broadcast/multicast).
- `host-probe.sh` — run in a sandbox against the host's NAT gateway; reports
  which host services answer (reachable by design, see `docs/trust-model.md`).
