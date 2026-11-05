# coop

Isolated VM environments for running Claude Code, Codex, and Grok Build.

> **Pronunciation:** "coop" (/kuːp/) — one syllable, rhymes with "loop", like the thing you keep chickens in. Not "co-op".

coop is a Rust CLI that manages disposable virtual machines where Claude Code, Codex, and Grok Build have full tool access: Docker, git, compilers, package managers, all without risk to your host machine. Each VM is isolated, reproducible, and cheap to create and destroy.

## Setup

Check the [platform prerequisites](docs/getting-started.md#prerequisites).

Install the latest release:

```shell
curl -fsSL https://raw.githubusercontent.com/trailofbits/coop/main/install.sh | bash
```

Build the VM template image:

```shell
coop setup
```

Update coop's binaries:

```shell
coop update
```

See Getting Started for [building from source](docs/getting-started.md#build-from-source)
and alternative installation methods: [Homebrew](docs/getting-started.md#homebrew)
and [Nix](docs/getting-started.md#nix).

## Usage

Start an instance for the current project and launch an agent CLI:

```
cd ~/code/my-project
coop up
coop claude
# or
coop codex
# or
coop grok
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
- [Grok Build integration](docs/grok-integration.md)
- [Editor integration](docs/editor.md)
- [Multi-instance](docs/multi-instance.md)
- [Platform backends](docs/backends.md)
- [Shell completion](docs/shell-completion.md)
- [Architecture](docs/ARCHITECTURE.md) and [trust model](docs/trust-model.md)
- [Contributing](CONTRIBUTING.md) and [security policy](SECURITY.md)
