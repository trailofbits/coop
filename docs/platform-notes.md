# Platform notes and gotchas

Durable, non-obvious environment facts that repeatedly bite contributors. These
are engineering notes, not user documentation — for user-facing backend setup
see [`backends.md`](backends.md).

## Firecracker CI-kernel workarounds

The Firecracker CI kernel (`vmlinux-6.1.155`) is minimal and missing several
modules. Two workarounds are applied when provisioning the guest image
(`scripts/guest/guest-config.sh`, baked into the golden image):

1. **iptables-legacy.** The kernel lacks nftables support (`CONFIG_NF_TABLES`
   not set). Docker's default `iptables-nft` backend fails with "Protocol not
   supported". Fix: `update-alternatives --set iptables /usr/sbin/iptables-legacy`.
2. **Static `resolv.conf`.** The CI rootfs ships `/etc/resolv.conf` as a symlink
   to systemd-resolved's stub (`127.0.0.53`), but `systemd-resolved` is not
   installed, so DNS fails silently. Fix: replace the symlink with a static file
   pointing to `8.8.8.8` / `8.8.4.4`.

Both would be resolved by building a custom Firecracker kernel with the needed
netfilter modules enabled, rather than using the minimal CI kernel. The Lima
backend uses a full kernel and needs neither.

## Docker networking in the guest

The Firecracker CI kernel also lacks the `iptable_raw` module (`CONFIG_IP_NF_RAW`
not set). Docker 28+ uses the raw table for "direct access filtering" — a
PREROUTING DROP rule that prevents direct routing to published container ports,
ensuring traffic goes through Docker's port-mapping rules.

Without the raw table, Docker refuses to start bridge networking. The fix uses
Docker 28.0.2's `DOCKER_INSECURE_NO_IPTABLES_RAW=1` env var (moby/moby#49621),
set via a systemd drop-in at `/etc/systemd/system/docker.service.d/no-raw.conf`.
This tells Docker to skip raw-table rules while keeping full bridge networking:
NAT, port mapping (`-p`), container-to-container communication, and embedded DNS
all work normally.

The "insecure" label refers to the fact that without raw-table rules, other
hosts on the local network could route directly to published container ports
even if they're bound to loopback. This is irrelevant here — the guest's only
network neighbor is the Firecracker host, and the VM itself is the isolation
boundary. See [`trust-model.md`](trust-model.md#documented-accepted-trade-offs).

## scp tilde expansion (OpenSSH 9+)

Modern scp (OpenSSH 9+) uses SFTP by default, which does **not** expand `~` in
remote paths. `scp file user@host:~/.claude/CLAUDE.md` silently creates a literal
`~` directory instead of writing to the home directory.

Fix: `GuestPath` values use `./` instead of `~/` in remote paths (e.g.
`GuestPath::new("./.claude")`). SFTP defaults to the user's home directory, so
`./path` is equivalent to `~/path`. This convention is used in `scp_to` and
`scp_to_recursive`.

SSH commands (`exec`) are unaffected — the remote shell expands `~` normally.
Only scp's SFTP mode has this issue.

## Tracing output goes to stderr

The coop binary's tracing output (INFO/DEBUG/WARN logs) goes to **stderr**, so
stdout stays clean for machine-readable (`--json`) output and piped consumers.
