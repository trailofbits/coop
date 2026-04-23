# Getting Started

coop runs Claude Code and Codex inside isolated virtual machines. On Linux, it spins up Firecracker microVMs backed by KVM. On macOS, it uses Lima with Apple's Virtualization.framework. Each VM gets its own filesystem, network stack, and Docker daemon. Agent CLIs never touch your host.

## Prerequisites

**macOS (Lima backend)**

- [Lima](https://github.com/lima-vm/lima) installed with `limactl` on your `PATH`
- Apple Silicon (arm64)
- Rosetta 2 for x86_64 guests on Apple Silicon: `softwareupdate --install-rosetta`

**Linux (Firecracker backend)**

- KVM access (`/dev/kvm` must exist and be writable by your user)
- x86_64 architecture
- `sudo` privileges (Firecracker uses jailer and TAP networking)
- `curl`, `tar`, `e2fsprogs` (for `mkfs.ext4`, `resize2fs`)

## Build from source

coop is a Rust project. Install [Rust](https://rustup.rs/), then:

```
cargo build --release
```

The binary lands at `target/release/coop`.

## Configuration

coop reads `~/.coop/config.toml` by default. Override the path with `--config`. If the file doesn't exist, coop falls back to built-in defaults. Run `coop init` to generate a starter config file.

A minimal config (an empty file is valid; all fields have defaults):

```toml
```

Defaults: 2 vCPUs, 4 GiB RAM, 8 GiB template disk. Override any of them:

```toml
[vm]
vcpu_count = 4
mem_size_mib = 8192
template_size_gib = 20
```

All VM artifacts (kernel, rootfs images, instance disks) live under `~/.coop/`.

### Claude Code and Codex integration

Forward your API keys and GitHub credentials into the guest:

```toml
github = "auto"

[vm]
vcpu_count = 4
mem_size_mib = 8192

[claude]
config_dir = "~/.claude"

[codex]
config_dir = "~/.codex"
```

The `github` field controls how coop resolves a GitHub token for the guest:

- `"off"` (default): disables GitHub auth forwarding
- `"auto"`: checks `$GITHUB_TOKEN` env var first, falls back to `gh auth token` if unset
- `"env"`: requires `GITHUB_TOKEN` in your environment

GitHub auth is off by default. Set `github = "auto"` explicitly to enable it.

coop picks up `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` from your environment automatically. Setting them explicitly under `claude.api_key` or `codex.api_key` also works, but environment variables are preferred.

**Codex auth note**: if `~/.codex/auth.json` exists on the host (i.e., you have run `codex login`), coop copies it into every new guest at `~/.codex/auth.json` so Codex starts already signed in. This copies an OAuth access token and a long-lived refresh token onto the guest disk. See [Codex integration: Auth handling](codex-integration.md#auth-handling) for how to opt out.

## First run

### 1. Setup

`coop setup` downloads the Firecracker binary and kernel (Linux) or configures Lima (macOS), then builds a template rootfs image. The template ships with base packages (git, curl, build-essential, Docker, tmux, and others), the GitHub CLI, Claude Code, and Codex.

```
coop setup
```

Install language toolchains into the template with `--profile`:

```
coop setup --profile python,node
```

Built-in profiles: `python`, `node`, `c`, `fuzz`, `rust`, `go`, `full` (all of the above).

Skip confirmation prompts with `-y`:

```
coop setup -y --profile python
```

Setup is idempotent. Rerunning with the same profiles skips completed work. Pass `--rebuild` to force a fresh template build.

### 2. Start an instance

```
coop start
```

This creates a VM instance from the template, boots it, waits for SSH, and injects Claude Code and Codex credentials/config. The instance gets an auto-generated name.

Name it explicitly:

```
coop start my-project
```

Sync a local directory into the VM as `/workspace`:

```
coop start my-project --workspace ~/code/my-project
```

Clone a git repository inside the VM instead:

```
coop start my-project --git-repo https://github.com/user/repo
```

Set a custom disk size for the instance (must be >= template size):

```
coop start my-project --disk 40
```

Mount host directories into the VM:

```
coop start my-project --mount ~/data
coop start my-project --mount ~/data:/guest/data --mount ~/models:/guest/models
```

`--mount HOST_PATH[:GUEST_PATH]` is repeatable. On macOS/Lima, mounts are live virtiofs mounts. On Linux/Firecracker, mounts are a one-time rsync sync. Conflicts with `--workspace` and `--git-repo`.

Start from a specific named image:

```
coop start my-project --image python-dev
```

Skip Claude Code and Codex credential/config injection:

```
coop start my-project --no-agents
```

### 3. Connect

**Launch Claude Code inside the VM:**

```
coop claude
```

This runs Claude Code with `--dangerously-skip-permissions` by default. The VM itself is the isolation boundary. For permission prompts:

```
coop claude --ask
```

Pass extra arguments through to `claude`:

```
coop claude -- --model opus
```

**Launch Codex inside the VM:**

```
coop codex
```

Pass extra arguments through to `codex`:

```
coop codex -- --model gpt-5
```

**Open a shell in the VM:**

```
coop shell
```

`coop shell`, `coop claude`, and `coop codex` attach to persistent tmux sessions. Use `--no-tmux` for a raw SSH connection, or `--session <name>` to pick a named session.

**Run a command non-interactively:**

```
coop shell -- ls /workspace
coop exec -- docker ps
```

### 4. Check status

```
coop status
```

For a specific instance:

```
coop status my-project
```

Running instances report resource usage: load average, memory, and disk.

### 5. Sync files

Push local changes into a running VM:

```
coop push
```

Pull guest changes back to the host:

```
coop pull
```

Both commands default to the workspace path from `coop start --workspace`. Override with a positional argument:

```
coop push ~/other-dir
coop pull ~/other-dir
```

### 6. Tear down

Stop an instance (preserves disk state):

```
coop stop my-project
```

Destroy an instance (deletes its disk and resources):

```
coop destroy my-project
```

Remove everything, including all instances, images, kernel, and Firecracker binary:

```
coop destroy --all
```

## Named images

Build multiple template images with different profiles:

```
coop setup --image python-dev --profile python
coop setup --image full-dev --profile full
```

Start an instance from a specific image:

```
coop start --image python-dev
```

List images:

```
coop images
```

Delete an image:

```
coop images --delete python-dev
```

## Other commands

| Command | Description |
|---------|-------------|
| `coop validate` | Check config and prerequisites without changing anything |
| `coop logs` | Stream VM serial console logs (`-f` to follow) |
| `coop vscode` | Open VS Code connected to the guest via SSH |
| `coop resize --size +20` | Grow a stopped instance's disk by 20 GiB |
| `coop resize --size 100` | Set a stopped instance's disk to 100 GiB |

## Further reading

- [Configuration reference](configuration.md)
- [Command reference](commands.md)
- [Images and profiles](images-and-profiles.md)
- [Workspace sync](workspaces.md)
- [Claude Code integration](claude-integration.md)
- [Codex integration](codex-integration.md)
- [VS Code and editor integration](vscode.md)
- [Running multiple instances](multi-instance.md)
- [Platform backends](backends.md)
