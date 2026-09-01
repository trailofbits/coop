# coop

Isolated VM environments for running Claude Code and Codex.

> **Pronunciation:** "coop" (/kuːp/) — one syllable, rhymes with "loop", like the thing you keep chickens in. Not "co-op".

coop is a Rust CLI that manages disposable virtual machines where Claude Code and Codex have full tool access: Docker, git, compilers, package managers, all without risk to your host machine. Each VM is isolated, reproducible, and cheap to create and destroy. On Linux, coop runs Firecracker microVMs backed by KVM. On macOS, it uses Lima with Apple's Virtualization.framework. The backend is selected automatically based on platform.

## Setup and updating

Install the latest release:

```
curl -fsSL https://raw.githubusercontent.com/trailofbits/coop/main/install.sh | bash
```

Or build from source (requires [Rust](https://rustup.rs/)):

```
cargo build --release
cp target/release/coop /usr/local/bin/
```

Then build the VM template image:

```
coop setup
```

On Linux, `coop setup` also installs Firecracker and fetches a guest kernel. On macOS, install Lima first (`brew install lima`) — setup fails without it. coop is tested on macOS arm64 (Apple Silicon) and Linux x86_64; Linux arm64 builds are available but untested. Each backend has its own host requirements — see [Prerequisites](docs/getting-started.md#prerequisites).

`coop update` replaces the running binary with the latest GitHub release, verifying its checksum and build-provenance attestation first. coop also checks for new releases in the background at most once a day; turn that off with `updates.mode = "off"` in `~/.coop/config.toml` or `COOP_NO_UPDATE_CHECK=1`. See [`coop update`](docs/commands.md#update) and the [`updates` config section](docs/configuration.md#updates-section).

## Usage

Start an instance for the current project and launch an agent CLI:

```
export ANTHROPIC_API_KEY=sk-ant-...
export OPENAI_API_KEY=sk-proj-...
cd ~/code/my-project
coop up
coop claude
# or
coop codex
```

That gives you a Claude Code or Codex session running inside an isolated VM with your project synced in. `coop up` is re-runnable: it creates an environment for the current project the first time, reuses it if it is already running, and restarts it after `coop stop`.

During startup, coop writes `~/.claude/settings.json` in the guest with `defaultMode: bypassPermissions` and `skipDangerousModePermissionPrompt: true`, so Claude Code runs without permission prompts — the VM itself is the isolation boundary. Pass `--ask` to `coop claude` to restore prompts for that session (`--permission-mode default`).

Every subcommand, flag, and example is in the [command reference](docs/commands.md); `coop --help` lists them too.

## Development

Requires the Rust toolchain pinned in `rust-toolchain.toml`. Install the pinned dev tools with `./scripts/install-dev-tools.sh --all`, then `prek install` to enable the git hooks.

```bash
cargo build                                                # debug build
cargo fmt -- --check                                       # format check
cargo clippy --all-targets --all-features -- -D warnings   # lints (zero warnings)
cargo test                                                 # unit tests
prek run --all-files                                       # all pre-commit hooks
./tests/run-integration.sh                                 # full VM lifecycle
```

The integration suite drives a real VM, so run it on both backends — macOS/Lima and Linux/Firecracker (`./tests/run-integration.sh --remote user@host`) — before opening a pull request. [CONTRIBUTING.md](CONTRIBUTING.md) covers the full workflow, and [docs/testing.md](docs/testing.md) covers mutation testing, fuzzing, and proofs.

## Documentation

- [Documentation index](docs/index.md)
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
- [Shell completion](docs/shell-completion.md)
- [Architecture](docs/ARCHITECTURE.md) and [trust model](docs/trust-model.md)
- [Contributing](CONTRIBUTING.md) and [security policy](SECURITY.md)
