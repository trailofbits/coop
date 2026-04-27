# coop

Isolated VM environment for running Claude Code and Codex — Firecracker on Linux, Lima on macOS.

## Architecture

Rust CLI that orchestrates VM lifecycle: setup, start, shell, stop, destroy, status, logs.

Two backends, auto-detected by platform:
- **Linux**: Firecracker microVMs with KVM. Cross-compiled from arm64 macOS (`x86_64-unknown-linux-musl` via `musl-cross`).
- **macOS**: Lima VMs using Apple Virtualization.framework (`limactl`). Native arm64 binary.

Backend abstraction in `src/backend.rs` provides `SshTarget` and `Backend` enum. All SSH-based operations (config injection, workspace sync, VS Code) are shared across backends.

## Before committing

Pre-commit hooks (prek) run automatically: fmt, clippy, test, trailing whitespace, EOF fixer, large file check, merge conflict check. If hooks aren't installed, run `prek install`.

After hooks pass, run integration tests on **both platforms** — these are too slow for hooks:

```bash
# Local (macOS/Lima) — builds and runs automatically
./tests/run-integration.sh

# Remote (Linux/Firecracker) — detects remote arch, cross-compiles, copies, and runs
./tests/run-integration.sh --remote user@remote-host
```

## Testing

### Integration tests

Two scripts:
- `tests/integration.sh` — the test suite. Runs locally, requires `--binary`.
- `tests/run-integration.sh` — the runner. Builds, deploys (if remote), and invokes the test suite.

Run on **both platforms** before every commit:

```bash
# Local (macOS/Lima)
./tests/run-integration.sh

# Remote (Linux/Firecracker)
./tests/run-integration.sh --remote user@remote-host

# With options (forwarded to integration.sh)
./tests/run-integration.sh --remote user@remote-host --full
./tests/run-integration.sh --profile python,node --name my-test
```

You can also run the test script directly if you already have a binary:

```bash
./tests/integration.sh --binary /path/to/coop --full
```

When adding new features, consider whether they should be covered by the integration test. The test exercises the full VM lifecycle (setup → start → status → shell → guest environment → docker → stop → destroy). New commands or guest-visible changes are good candidates for new test phases.

## Known workarounds (revisit later)

The Firecracker CI kernel (`vmlinux-6.1.155`) is minimal and missing several modules. Two workarounds are applied in the guest install script (`src/setup.rs`, `guest_install_script()`):

1. **iptables-legacy** — The kernel lacks nftables support (`CONFIG_NF_TABLES` not set). Docker's default `iptables-nft` backend fails with "Protocol not supported". Fix: `update-alternatives --set iptables /usr/sbin/iptables-legacy`. A custom kernel with nftables enabled would remove this workaround.

2. **Static resolv.conf** — The CI rootfs ships `/etc/resolv.conf` as a symlink to systemd-resolved's stub (`127.0.0.53`), but `systemd-resolved` is not installed. DNS fails silently. Fix: replace the symlink with a static file pointing to `8.8.8.8`. Installing `systemd-resolved` in the guest would be the proper fix, letting systemd-networkd's DNS= directives propagate automatically.

Both could be resolved by building a custom Firecracker kernel with the needed netfilter modules enabled, rather than using the minimal CI kernel.

## Docker networking in the guest

The Firecracker CI kernel lacks the `iptable_raw` module (`CONFIG_IP_NF_RAW` not set). Docker 28+ uses the raw table for "direct access filtering" — a PREROUTING DROP rule that prevents direct routing to published container ports, ensuring traffic goes through Docker's port-mapping rules.

Without the raw table, Docker refuses to start bridge networking. The fix uses Docker 28.0.2's `DOCKER_INSECURE_NO_IPTABLES_RAW=1` env var (moby/moby#49621), set via a systemd drop-in at `/etc/systemd/system/docker.service.d/no-raw.conf`. This tells Docker to skip raw table rules while keeping full bridge networking: NAT, port mapping (`-p`), container-to-container communication, and embedded DNS all work normally.

The "insecure" label refers to the fact that without raw table rules, other hosts on the local network could route directly to published container ports even if they're bound to loopback. This is irrelevant here — the guest's only network neighbor is the Firecracker host, and the VM itself is the isolation boundary.

## scp tilde expansion (OpenSSH 9+)

Modern scp (OpenSSH 9+) uses SFTP by default, which does **not** expand `~` in remote paths. `scp file user@host:~/.claude/CLAUDE.md` silently creates a literal `~` directory instead of writing to the home directory.

Fix: `GuestPath` values use `./` instead of `~/` in remote paths (e.g., `GuestPath::new("./.claude")`). SFTP defaults to the user's home directory, so `./path` is equivalent to `~/path`. This convention is used in `scp_to` and `scp_to_recursive`.

SSH commands (`exec`) are unaffected — the remote shell expands `~` normally. Only scp's SFTP mode has this issue.

## Tracing output goes to stderr

The coop binary's tracing output (INFO/DEBUG/WARN logs) goes to **stderr**.
