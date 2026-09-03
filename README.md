# coop

Isolated VM environments for running Claude Code and Codex.

> **Pronunciation:** "coop" (/kuːp/) — one syllable, rhymes with "loop", like the thing you keep chickens in. Not "co-op".

coop is a Rust CLI that manages disposable virtual machines where Claude Code and Codex have full tool access: Docker, git, compilers, package managers, all without risk to your host machine. Each VM is isolated, reproducible, and cheap to create and destroy.

## Setup

Install the latest release:

```shell
curl -fsSL https://raw.githubusercontent.com/trailofbits/coop/main/install.sh | bash
```

Or build from source (requires [Rust](https://rustup.rs/)):

```shell
cargo build --release
cp target/release/coop /usr/local/bin/
```

Then build the VM template image:

```shell
coop setup
```

On Linux, `coop setup` also installs Firecracker and fetches a guest kernel. On macOS, install Lima first (`brew install lima`) — setup fails without it. coop is tested on macOS arm64 (Apple Silicon) and Linux x86_64; Linux arm64 builds are available but untested. Each backend has its own host requirements — see [Prerequisites](docs/getting-started.md#prerequisites).

Keep coop updated:

```shell
coop update
```

See [`coop update`](docs/commands.md#update) and the [`updates` config section](docs/configuration.md#updates-section).

## Usage

Start an instance for the current project and launch an agent CLI:

```
cd ~/code/my-project
coop up
coop claude
# or
coop codex
```

## Documentation

- [Documentation index](docs/index.md)
- [Getting started](docs/getting-started.md)
- [Command reference](docs/commands.md)
- [Configuration reference](docs/configuration.md)
- [Images and profiles](docs/images-and-profiles.md)
- [Workspace sync](docs/workspaces.md)
- [Claude Code integration](docs/claude-integration.md)
- [Codex integration](docs/codex-integration.md)
- [Editor integration](docs/editor.md)
- [Multi-instance](docs/multi-instance.md)
- [Platform backends](docs/backends.md)
- [Shell completion](docs/shell-completion.md)
- [Architecture](docs/ARCHITECTURE.md) and [trust model](docs/trust-model.md)
- [Contributing](CONTRIBUTING.md) and [security policy](SECURITY.md)
