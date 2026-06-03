# Getting Started

coop runs Claude Code and Codex inside isolated virtual machines. On Linux, it spins up Firecracker microVMs backed by KVM. On macOS, it uses Lima with Apple's Virtualization.framework. Each VM gets its own filesystem, network stack, and Docker daemon. Agent CLIs never touch your host.

## Prerequisites

**macOS (Lima backend)**

- [Lima](https://github.com/lima-vm/lima) installed with `limactl` on your `PATH`
- Apple Silicon (arm64)
- Rosetta 2 for x86_64 guests on Apple Silicon: `softwareupdate --install-rosetta`

**Linux (Firecracker backend)**

- KVM access (`/dev/kvm` must exist and be writable by your user)
- x86_64 or arm64 architecture (x86_64 is the primary test target; arm64 builds are available but less exercised)
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
- `"pat"`: forwards a per-repo fine-grained PAT recorded under `[github.pat."owner/repo"]`. GitHub enforces the token's scope server-side — see [GitHub auth](configuration.md#fine-grained-pat-github--pat) for the full reference.

GitHub auth is off by default. Set `github = "auto"` (or run `coop github setup-pat --repo owner/name` for a scoped PAT) to enable it. `coop up` offers to run the PAT wizard inline the first time you bring up a project backed by a GitHub repo without auth configured.

coop picks up `ANTHROPIC_API_KEY` and `OPENAI_API_KEY` from your environment automatically. Setting them explicitly under `claude.api_key` or `codex.api_key` also works, but environment variables are preferred.

## First run

### 1. Setup

`coop setup` downloads the Firecracker binary and kernel (Linux) or configures Lima (macOS), then builds a template rootfs image. The template ships with base packages (git, curl, build-essential, Docker, and others), the GitHub CLI, Claude Code, and Codex.

```
coop setup
```

Install language toolchains into the template with `--profile`:

```
coop setup --profile python,node
```

Built-in profiles: `python`, `node`, `c`, `fuzz`, `rust`, `go`. Combine them with commas (e.g. `--profile python,node,rust`). Use `coop profiles list` to inspect what each one installs.

You can also let `start` build a profile-derived image on demand:

```
coop setup --profile python,node
```

This creates or refreshes an image named from the sorted profile list
(`node-python` here), then starts a new instance from it.

Skip confirmation prompts with `-y`:

```
coop setup -y --profile python
```

Setup is idempotent. Rerunning with the same profiles skips completed work. Pass `--rebuild` to force a fresh template build.

### 2. Bring up a project environment

For normal project work, use `coop up` from your project directory:

```
cd ~/code/my-project
coop up
```

`coop up` is project-oriented and re-runnable. It creates an instance the
first time, reuses it if it is already running, and restarts it after
`coop stop`. By default it copies/syncs the project into `/workspace`.

Choose mount transport explicitly:

```
coop up . --mount
```

On macOS/Lima this is live filesystem sharing. On Linux/Firecracker it is a
one-time sync.

Mount additional data directories when creating the project instance:

```
coop up . --extra-mount ~/data:/data
```

After the environment is running, connect to it:

```
coop shell
coop claude
coop codex
```

### 3. Restart a stopped instance

```
coop start
```

`coop start` only starts existing stopped instances. Use it after `coop stop`
when you want to boot the same VM disk again. If exactly one stopped instance
exists, the name is optional.

Restart a specific stopped instance:

```
coop start my-project
```

Restart by project path when the instance was created for that project:

```
coop start --workspace ~/code/my-project
```

Creation options belong to `coop up`, not `coop start`:

```
coop up ~/code/my-project --disk 40 --mount
```

Skip Claude Code and Codex credential/config injection:

```
coop start my-project --no-agents
```

### 4. Connect

**Launch Claude Code inside the VM:**

```
coop claude
```

During VM startup, coop writes `~/.claude/settings.json` in the guest with `defaultMode: bypassPermissions` and `skipDangerousModePermissionPrompt: true`, so Claude Code runs without permission prompts by default. The VM itself is the isolation boundary. For permission prompts (coop launches `claude` with `--permission-mode default`):

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

**Run a command non-interactively:**

```
coop shell -- ls /workspace
coop exec -- docker ps
```

### 5. Check status

```
coop status
```

For a specific instance:

```
coop status my-project
```

Running instances report resource usage: load average, memory, and disk.

### 6. Sync files

Push local changes into a running VM:

```
coop push
```

Pull guest changes back to the host:

```
coop pull
```

Both commands default to the workspace path recorded by `coop up`. Override with `--dir`:

```
coop push --dir ~/other-dir
coop pull --dir ~/other-dir
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
coop setup --image polyglot --profile python,node,rust
```

Create a project environment from a specific image:

```
coop up . --image python-dev
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
