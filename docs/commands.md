# Command Reference

coop creates isolated VM environments for running Claude Code and Codex. It runs Firecracker microVMs on Linux and Lima VMs on macOS, selecting the backend automatically based on platform.

## Global Flags

| Flag | Description |
|------|-------------|
| `--config <path>` | Path to config file (default: `~/.coop/config.toml`) |
| `-v`, `--verbose` | Increase log verbosity. Once for debug, twice for trace. |
| `--version` | Print version and exit. |

## Instance Name Resolution

Most commands accept an optional instance name. coop resolves the target instance with three rules:

- **Zero instances exist.** The command fails and tells you to run `coop start`.
- **One instance exists.** The name is optional. coop selects it automatically.
- **Multiple instances exist.** The name is required. coop lists available instances on error.

## Commands

### `init`

Generate a starter config file at `~/.coop/config.toml`.

```
coop init
```

No additional flags.

### `setup`

Run this once after installing coop. It checks prerequisites, installs the backend runtime, fetches a kernel, and builds a template root filesystem.

```
coop setup [FLAGS]
```

| Flag | Description |
|------|-------------|
| `-y`, `--yes` | Skip confirmation prompts (accept all) |
| `--vcpus <N>` | Number of vCPUs (overrides config) |
| `--mem <MiB>` | Memory in MiB (overrides config) |
| `--rebuild` | Force rebuild of template rootfs |
| `--profile <list>` | Comma-separated install profiles: `python`, `node`, `c`, `fuzz`, `rust`, `go` |
| `--extra-packages <list>` | Comma-separated extra apt packages to install |
| `--post-install <path>` | Path to a post-install script to run in the chroot |
| `--template-size <GiB>` | Template rootfs size in GiB (default: 8) |
| `--image <name>` | Named image to build (default: `default`) |

```
coop setup -y --profile python,node --template-size 12
coop setup --image ml-dev --profile python --extra-packages libopenblas-dev
```

### `build`

Rebuild the rootfs image and fetch the kernel. Use `setup` for first-time installation; `build` handles subsequent rebuilds.

```
coop build
```

No additional flags.

### `start`

Launch a new VM instance.

```
coop start [NAME] [FLAGS]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (auto-generated if omitted) |
| `--workspace <dir>` | Local directory to sync into the VM (conflicts with `--git-repo`) |
| `--git-repo <url>` | Git repository URL to clone inside the VM (conflicts with `--workspace`) |
| `--vcpus <N>` | Number of vCPUs (overrides config) |
| `--mem <MiB>` | Memory in MiB (overrides config) |
| `--disk <GiB>` | Instance disk size in GiB (grows from template size if larger) |
| `--no-agents` | Skip injecting Claude Code and Codex credentials/config into the VM |
| `--image <name>` | Named image to use (default: `default`) |
| `--mount <spec>` | Mount host directory into guest (`HOST_PATH[:GUEST_PATH]`, repeatable). Conflicts with `--workspace` and `--git-repo`. |

`--workspace` and `--git-repo` are mutually exclusive. Use `--workspace` to tar-pipe a local directory into the guest. Use `--git-repo` to clone a repository inside the VM at boot.

```
coop start my-project --workspace ./src --vcpus 4 --mem 8192
coop start --git-repo https://github.com/org/repo.git --disk 50
coop start --image ml-dev --no-agents
coop start --mount ~/data:/mnt/data
```

`--no-claude` is accepted as a deprecated alias for `--no-agents` and will be removed in a future release. Using it prints a deprecation warning.

### `shell`

Open an interactive shell in the VM, or run a single command non-interactively.

```
coop shell [NAME] [FLAGS] [-- COMMAND...]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--session <name>` | tmux session name (default: `main`) |
| `--no-tmux` | Skip tmux session persistence (raw SSH connection) |
| `-- COMMAND...` | Command to run non-interactively (no PTY allocated) |

Without a trailing command, `shell` drops you into a tmux session named `main` (or the name given by `--session`). With a trailing command, it executes the command and returns its exit code.

`--session` and `--no-tmux` are mutually exclusive.

```
coop shell
coop shell my-project --no-tmux
coop shell my-project --session work
coop shell my-project -- cat /etc/os-release
```

### `claude`

Launch Claude Code inside the VM. By default, coop passes `--dangerously-skip-permissions` because the VM itself is the isolation boundary. Use `--ask` to restore the permissions prompt.

```
coop claude [NAME] [FLAGS] [ARGS...]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--ask` | Prompt for permissions instead of skipping them |
| `--session <name>` | tmux session name (default: `claude`) |
| `--no-tmux` | Skip tmux session persistence (raw SSH connection) |
| `ARGS...` | Extra arguments passed through to `claude` |

The session runs inside tmux under the name `claude` (or the name given by `--session`). `--session` and `--no-tmux` are mutually exclusive.

```
coop claude
coop claude my-project --ask
coop claude my-project -- --model sonnet
```

### `codex`

Launch Codex inside the VM.

```
coop codex [NAME] [FLAGS] [ARGS...]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--session <name>` | tmux session name (default: `codex`) |
| `--no-tmux` | Skip tmux session persistence (raw SSH connection) |
| `ARGS...` | Extra arguments passed through to `codex` |

The session runs inside tmux under the name `codex` (or the name given by `--session`). `--session` and `--no-tmux` are mutually exclusive.

```
coop codex
coop codex my-project -- --model gpt-5
```

### `exec`

Run a command in the VM and print its output. No PTY is allocated and stdin is not forwarded; use `shell` for interactive work.

```
coop exec [--name NAME] COMMAND...
```

| Flag | Description |
|------|-------------|
| `--name <name>` | Instance name (required if multiple instances exist) |
| `COMMAND...` | Command and arguments to run (required) |

```
coop exec uname -a
coop exec --name my-project docker ps
```

### `stop`

Gracefully stop a running VM. The instance disk is preserved. Use `start` to relaunch or `destroy` to remove it.

```
coop stop [NAME]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |

```
coop stop
coop stop my-project
```

### `destroy`

Stop the VM and remove its resources: disk, config, and SSH entries. Templates and the kernel are preserved unless you pass `--all`.

```
coop destroy [NAME] [FLAGS]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--all` | Remove all instances, templates, kernel, Firecracker binary, and SSH keys |

```
coop destroy my-project
coop destroy --all
```

### `status`

Print instance status. Without a name, lists every instance with its state, image, backend, and resource usage (for running instances). With a name, prints detailed status for that instance.

```
coop status [NAME]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (shows all if omitted) |

```
coop status
coop status my-project
```

### `logs`

Stream the VM serial console output.

```
coop logs [NAME] [FLAGS]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `-f`, `--follow` | Follow log output (like `tail -f`) |

```
coop logs
coop logs my-project -f
```

### `push`

Copy a local directory into the running VM at `/workspace`. Defaults to the host path recorded when the instance was started with `--workspace`.

```
coop push [--name NAME] [DIR] [FLAGS]
```

| Flag | Description |
|------|-------------|
| `--name <name>` | Instance name (required if multiple instances exist) |
| `DIR` | Local directory to push (defaults to the workspace host path) |
| `--force` | Overwrite guest changes without confirmation |

```
coop push
coop push --name my-project ./src --force
```

### `pull`

Copy the VM's `/workspace` to a local directory. Defaults to the host path recorded when the instance was started with `--workspace`.

```
coop pull [--name NAME] [DIR] [FLAGS]
```

| Flag | Description |
|------|-------------|
| `--name <name>` | Instance name (required if multiple instances exist) |
| `DIR` | Local directory to pull into (defaults to the workspace host path) |
| `--force` | Overwrite local changes without confirmation |

```
coop pull
coop pull --name my-project ./local-copy --force
```

### `vscode`

Open VS Code connected to the guest VM over SSH remote.

```
coop vscode [NAME] [--project PATH] [--editor EDITOR] [--clean]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--project <path>` | Remote path to open in VS Code (default: `/workspace`) |
| `--editor <name>` | Editor to use (e.g. `code`). Overrides auto-detection. |
| `--clean` | Remove the SSH config entry for this instance and exit |

```
coop vscode
coop vscode my-project --project /workspace/subdir
coop vscode my-project --editor code
coop vscode my-project --clean
```

### `images`

List or delete golden images. Without flags, prints every image with its profiles, creation date, and size.

```
coop images [FLAGS]
```

| Flag | Description |
|------|-------------|
| `--delete <name>` | Delete a named image |

```
coop images
coop images --delete old-image
```

### `resize`

Resize a stopped instance's disk. The VM must be stopped first.

```
coop resize [NAME] --size <SIZE>
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--size <size>` | New size (required). Absolute: `150` or `150G`. Relative: `+20` or `+20G`. |

Absolute values set the disk to that exact size. A `+` prefix adds to the current size.

```
coop resize my-project --size 150G
coop resize --size +20
```

### `profiles`

List or inspect available profiles. With no subcommand, lists every profile (builtin and custom).

```
coop profiles [SUBCOMMAND]
```

| Subcommand | Description |
|------------|-------------|
| `list` | List builtin and custom profiles with a one-line summary each (default) |
| `show <name>` | Print the full definition of a profile: apt packages, pre/post-install scripts, marketplaces, plugins |

```
coop profiles
coop profiles list
coop profiles show rust
```

`list` groups builtin and custom profiles separately. `show` resolves the name against custom profiles first, then builtins, and prints `(custom)` or `(builtin)` next to the name.

### `update`

Replace the running coop binary with a release from `github.com/trailofbits/coop`. Downloads the tarball matching the current host triple, verifies its SHA-256 against the release's `SHA256SUMS`, and (when `gh` is installed) verifies the GitHub build-provenance attestation before swapping the binary atomically.

While `trailofbits/coop` is private, `coop update` requires either [`gh`](https://cli.github.com/) authenticated against `github.com` or `GITHUB_TOKEN` in the environment to reach the API and download release assets. Once the repository is public, no auth is needed.

```
coop update [FLAGS]
```

| Flag | Description |
|------|-------------|
| `--check` | Report whether a newer release exists. Do not download or install. |
| `--force` | Reinstall even if the current binary is already at the target version. |
| `--version <VERSION>` | Install a specific release tag (e.g. `v0.3.2` or `0.3.2`). |
| `-y`, `--yes` | Skip the interactive confirmation prompt. |

If coop is installed in a protected directory (e.g. `/usr/local/bin`), run with `sudo`. Dev builds (built from an untagged or dirty tree) refuse to self-update; use `install.sh` to replace them.

```
coop update --check
coop update
coop update --yes
coop update --version v0.3.2
coop update --force
```

See also the [`updates` section](configuration.md#updates-section) of the configuration reference for the background-notification settings.

### `validate`

Check the configuration file and prerequisites. Prints warnings and confirms the config loads correctly.

```
coop validate
```

No additional flags.
