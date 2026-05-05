# coop

Isolated VM environments for running Claude Code and Codex.

coop is a Rust CLI that manages disposable virtual machines where Claude Code and Codex have full tool access: Docker, git, compilers, package managers, all without risk to your host machine. Each VM is isolated, reproducible, and cheap to create and destroy. On Linux, coop runs Firecracker microVMs backed by KVM. On macOS, it uses Lima with Apple's Virtualization.framework. The backend is selected automatically based on platform.

## Quick start

Install:

```
curl -fsSL https://raw.githubusercontent.com/trailofbits/coop/main/install.sh | bash
```

For internal/private repos (requires [GitHub CLI](https://cli.github.com/)):

```
gh api repos/trailofbits/coop/contents/install.sh -H "Accept: application/vnd.github.raw" | bash
```

Or build from source (requires [Rust](https://rustup.rs/)):

```
cargo build --release
cp target/release/coop /usr/local/bin/
```

Set up the VM template, start an instance, and launch an agent CLI:

```
export ANTHROPIC_API_KEY=sk-ant-...
export OPENAI_API_KEY=sk-proj-...
coop setup
coop start my-project --workspace ~/code/my-project
coop claude
# or
coop codex
```

That gives you a Claude Code or Codex session running inside an isolated VM with your project synced in. By default, `coop claude` launches with `--dangerously-skip-permissions` since the VM is the isolation boundary. Pass `--ask` to prompt for permissions instead.

## Features

- **Two backends**: Firecracker microVMs (Linux/KVM) and Lima VMs (macOS/Virtualization.framework), auto-detected by platform
- **Workspace sync**: push a local directory into the VM, or clone a git repo directly with `--git-repo`
- **Profiles**: customizable guest environments with apt packages and install scripts; built-in profiles for Python, Node, C, Rust, Go, and fuzzing
- **Named images**: build multiple template images with different profiles (`coop setup --image ml-dev --profile python`)
- **Claude Code integration**: API key forwarding, CLAUDE.md injection, plugin/marketplace support, MCP server configuration
- **Codex integration**: API key forwarding, `~/.codex` config sync, MCP server configuration, dedicated `coop codex` launcher
- **VS Code remote SSH**: `coop vscode` opens VS Code connected to the guest
- **Multi-instance**: run multiple VMs side by side, each with its own name and disk
- **Disk resize**: grow a stopped instance's disk with `coop resize --size +20`
- **Config optional**: works with sensible defaults; customize via `~/.coop/config.toml` when needed

## Commands

| Command | Description |
|---------|-------------|
| `setup` | Install backend runtime, fetch kernel, build template rootfs |
| `build` | Rebuild rootfs image and fetch kernel |
| `start` | Launch a new VM instance |
| `stop` | Stop a running VM (preserves disk) |
| `destroy` | Stop and remove a VM instance |
| `shell` | Interactive shell session in a running VM |
| `claude` | Launch Claude Code inside the VM |
| `codex` | Launch Codex inside the VM |
| `exec` | Run a command in the VM non-interactively |
| `push` | Sync local directory into the VM |
| `pull` | Sync VM workspace back to the host |
| `status` | Show instance status and resource usage |
| `logs` | Stream VM serial console output |
| `vscode` | Open VS Code connected to the guest |
| `images` | List or delete template images |
| `resize` | Grow a stopped instance's disk |
| `validate` | Check config and prerequisites |
| `update` | Self-update coop to the latest GitHub release |

## Updating

`coop update` replaces the running binary with the latest release from
`github.com/trailofbits/coop`. It downloads the tarball matching the current
host triple, verifies the SHA-256 against the release's `SHA256SUMS`, and
(when `gh` is installed) verifies the GitHub build-provenance attestation
before swapping the binary atomically.

While `trailofbits/coop` is private, `coop update` requires either
[`gh`](https://cli.github.com/) authenticated against `github.com` or
`GITHUB_TOKEN` in the environment to reach the API and download release
assets. Once the repository is public, no auth is needed.

```sh
coop update --check             # report whether a newer release exists
coop update                     # prompt, then install the latest release
coop update --yes               # skip confirmation
coop update --version v0.3.2    # pin to a specific release
coop update --force             # reinstall the current version
```

If coop is installed in a protected directory (e.g. `/usr/local/bin`), run
with `sudo`. Dev builds (built from an untagged or dirty tree) refuse to
self-update; use `install.sh` to replace them.

By default, coop checks for a newer release in the background at most once
per day and prints a one-line notice on stderr when an update is available.
Opt out with either:

- `updates.mode = "off"` in `~/.coop/config.toml`, or
- `COOP_NO_UPDATE_CHECK=1` in the environment.

The check is also silent when `CI=true` or when stdin is not a TTY.

## Verifying a release

Every release tarball is published with a Sigstore build-provenance
attestation via [`actions/attest-build-provenance`](https://github.com/actions/attest-build-provenance).
The attestation proves the artifact was built from this repository by the
tagged release workflow.

Both `install.sh` and `coop update` run this verification automatically
when the [GitHub CLI](https://cli.github.com/) is installed. Without `gh`,
they fall back to checksum verification against the release's `SHA256SUMS`
and print a note explaining what was and wasn't verified.

To verify a downloaded tarball manually:

```sh
gh attestation verify coop-<version>-<triple>.tar.gz --repo trailofbits/coop
```

## Requirements

Tested on macOS arm64 (Apple Silicon) and Linux x86_64. Linux arm64 builds are available but untested.

**macOS (Lima backend)**

- macOS with Apple Silicon
- [Lima](https://github.com/lima-vm/lima) with `limactl` on your PATH (installed automatically by `coop setup`)
- Rosetta 2 for x86_64 guests on Apple Silicon: `softwareupdate --install-rosetta`

**Linux (Firecracker backend)**

- x86_64 or arm64 architecture
- KVM access (`/dev/kvm` must exist and be writable by your user)
- `sudo` privileges (Firecracker uses jailer and TAP networking)
- `curl`, `tar`, `e2fsprogs` (for `mkfs.ext4`, `resize2fs`)

## Documentation

- [Getting started](docs/getting-started.md)
- [Command reference](docs/commands.md)
- [Configuration reference](docs/configuration.md)
- [Images and profiles](docs/images-and-profiles.md)
- [Workspace sync](docs/workspaces.md)
- [Claude Code integration](docs/claude-integration.md)
- [Codex integration](docs/codex-integration.md)
- [VS Code integration](docs/vscode.md)
- [Multi-instance](docs/multi-instance.md)
- [Platform backends](docs/backends.md)
