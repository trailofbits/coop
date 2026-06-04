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

- **Zero instances exist.** The command fails and tells you to run `coop up`.
- **One instance exists.** The name is optional. coop selects it automatically.
- **Multiple instances exist.** The name is required. coop lists available instances on error.

## Commands

### `up`

Ensure an environment exists and is running for a project directory.

```
coop up [DIR] [FLAGS]
```

`DIR` defaults to the current directory. coop canonicalizes it and uses it as
the project identity for instance naming, devcontainer discovery, GitHub PAT
lookup, and future `coop up DIR` affinity. If a matching instance is already
running, `up` reports success without creating another VM. If a matching
instance is stopped, `up` restarts it. If no matching instance exists, `up`
creates one.

By default, `up` copies/syncs the project into `/workspace`. Pass `--mount` to use the mount transport for the
project at `/workspace` instead. On macOS/Lima this is a live virtiofs mount;
on Linux/Firecracker it is a one-time sync.
`--copy` is accepted as an explicit spelling of the default.

| Flag | Description |
|------|-------------|
| `DIR` | Project directory (default: current directory) |
| `--name <name>` | Instance name to use when creating the project environment |
| `--copy` | Copy/sync `DIR` into `/workspace` (default) |
| `--mount` | Mount `DIR` at `/workspace` instead of using `--copy` |
| `--extra-mount <spec>` | Additional host directory to mount into the guest (`HOST_PATH[:GUEST_PATH]`, repeatable; specify a guest path other than `/workspace` when using `--copy`) |
| `--vcpus <N>` | Number of vCPUs when creating a new instance |
| `--mem <MiB>` | Memory in MiB when creating a new instance |
| `--disk <GiB>` | Instance disk size when creating a new instance |
| `--no-agents` | Skip injecting Claude Code and Codex credentials/config into the VM |
| `--image <name>` | Named image to use when creating a new instance (default: `default`) |
| `--profile <list>` | Build or reuse a profile-derived image when creating a new instance, named from the sorted profiles (for example `node-python`) |
| `--exclude-git` | Skip the `.git/` directory when copying/syncing |
| `--no-prompt` | Suppress the interactive prompt to set up a scoped GitHub PAT when one is missing for the resolved repo |
| `--forward-port <spec>` | Forward a guest port to the host (`GUEST[:HOST]`, repeatable) |
| `--post-start <cmd>` | Shell command to run inside the guest after boot |
| `--env KEY=VALUE` | Literal env var to set in the guest (repeatable) |
| `--devcontainer <path>` | Explicit path to a `devcontainer.json` to use (skips discovery and prompt) |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json` |
| `--dry-run` | Translate `devcontainer.json` and print the report, then exit before any VM work |

```
coop up .
coop up ~/code/my-project --mount
coop up . --profile python,node
coop up . --copy --forward-port 3000
coop up . --extra-mount ~/data:/data
```

Creation options such as `--vcpus`, `--mem`, `--disk`, `--image`,
`--profile`, `--extra-mount`, `--exclude-git`, and `--devcontainer` are
applied only when `up` creates a new instance. `coop up --profile <list>`
derives an image name from the sorted profile list, runs the same stale-image
check as `coop setup`, and builds or rebuilds that image if needed. Explicit
named images are unchanged: use `coop setup --image <name> --profile ...`
followed by `coop up --image <name>` when you want to choose the image name
yourself. If a matching project instance already exists, destroy it first to
recreate it with different creation options. Runtime startup options such as
`--forward-port`, `--post-start`, and `--env` can be used when `up` creates or
restarts an instance; if the matching instance is already running, stop it
first so those options can take effect.

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
| `--guest-user <name>` | Guest username to bake into the image (default: `ubuntu`). Use this for devcontainers that declare another `remoteUser`, such as `vscode`. |
| `--workspace <dir>` | Scan for `.devcontainer/devcontainer.json` and offer to apply its `features` / `hostRequirements` to this setup. Supported public `ghcr.io/devcontainers/features/*` entries are resolved and baked into the image. |
| `--devcontainer <path>` | Explicit path to a `devcontainer.json` to use (skips discovery and prompt). |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json`. |
| `--dry-run` | Translate `devcontainer.json` and print the report, then exit before any setup work. |

```
coop setup -y --profile python,node --template-size 12
coop setup --image ml-dev --profile python --extra-packages libopenblas-dev
coop setup -y --workspace . --devcontainer .devcontainer/devcontainer.json
```

See [docs/devcontainer.md](devcontainer.md) for the subset of `devcontainer.json` coop reads.

### `build`

Rebuild the rootfs image and fetch the kernel. Use `setup` for first-time installation; `build` handles subsequent rebuilds.

```
coop build
```

No additional flags.

### `devcontainer check`

Parse a `devcontainer.json` file and print the same translation report that `setup --dry-run` and `start --dry-run` use, without loading coop config, checking for updates, setting up an image, or starting a VM. Setup-stage checks resolve supported public GHCR OCI Features so the report can show the digest and `install.sh` hash that would run.

```
coop devcontainer check <path> [--stage setup|start|both]
```

| Flag | Description |
|------|-------------|
| `<path>` | Path to the `devcontainer.json` file to inspect |
| `--stage <stage>` | Which lifecycle translation to report: `setup`, `start`, or `both` (default: `both`) |

Use `--stage setup` to inspect setup-time keys such as `features`, `hostRequirements.cpus`, `hostRequirements.memory`, and `remoteUser`. Use `--stage start` to inspect start-time keys such as `postStartCommand`, `containerEnv`, `forwardPorts`, `mounts`, and `hostRequirements.storage`.

### `start`

Restart a stopped VM.

```
coop start [NAME] [FLAGS]
```

`start` normally restarts existing stopped instances. Use `coop up [DIR]` to
create or reconnect to a project environment. Without `NAME`, `start` restarts
the only stopped instance if exactly one exists; with multiple stopped
instances, pass the instance name.

| Flag | Description |
|------|-------------|
| `NAME` | Stopped instance name (optional only when exactly one stopped instance exists) |
| `--workspace <dir>` | Restart the stopped instance associated with this project path (conflicts with `--git-repo`) |
| `--git-repo <url>` | Deprecated creation option; rejected on restart |
| `--vcpus <N>` | Creation-time option retained only for compatibility; rejected on restart |
| `--mem <MiB>` | Creation-time option retained only for compatibility; rejected on restart |
| `--disk <GiB>` | Creation-time option retained only for compatibility; rejected on restart |
| `--no-agents` | Skip injecting Claude Code and Codex credentials/config into the VM |
| `--image <name>` | Creation-time option retained only for compatibility; rejected on restart |
| `--mount <spec>` | Creation-time option retained only for compatibility; rejected on restart |
| `--forward-port <spec>` | Forward a guest port to the host (`GUEST[:HOST]`, repeatable). Lives for the lifetime of the VM; torn down on `coop stop`. |
| `--exclude-git` | Creation-time option retained only for compatibility; rejected on restart |
| `--no-prompt` | Suppress the interactive prompt to set up a scoped GitHub PAT when one is missing for the resolved repo (see [`coop github setup-pat`](#github)). |
| `--post-start <cmd>` | Shell command to run inside the guest after boot. Overrides the `post_start` field in `config.toml`. Failure is logged but does not fail the start. |
| `--env KEY=VALUE` | Literal env var to set in the guest (repeatable). Overrides `guest_env` config entries and any forwarded values with the same name. |
| `--devcontainer <path>` | Explicit path to a `devcontainer.json` to use (skips discovery and prompt). |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json` (escape hatch for CI). |
| `--dry-run` | Translate `devcontainer.json` and print the report, then exit before any VM work. |

When `--workspace <dir>` contains a `.devcontainer/devcontainer.json`, or `--git-repo <url>` points at a GitHub repository with one, coop reads a subset of it and prompts before applying restart-time settings. See [docs/devcontainer.md](devcontainer.md) for the supported keys and discovery rules.

```
coop start
coop start my-project
coop start my-project --no-agents
coop start --env RUST_LOG=info --env MY_FLAG=1
coop start --forward-port 3000 --forward-port 8080:18080
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
| `-- COMMAND...` | Command to run non-interactively (no PTY allocated) |

Without a trailing command, `shell` drops you into an interactive shell at `/workspace`. With a trailing command, it executes the command and returns its exit code.

```
coop shell
coop shell my-project
coop shell my-project -- cat /etc/os-release
```

### `claude`

Launch Claude Code inside the VM. The guest's `~/.claude/settings.json` (written during VM startup) sets `defaultMode: bypassPermissions` and `skipDangerousModePermissionPrompt: true`, so Claude Code runs without permission prompts — the VM itself is the isolation boundary. Use `--ask` to override the guest default for that session (coop passes `--permission-mode default`).

```
coop claude [NAME] [FLAGS] [ARGS...]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--ask` | Prompt for permissions instead of skipping them |
| `ARGS...` | Extra arguments passed through to `claude` |

```
coop claude
coop claude my-project --ask
coop claude my-project -- --model sonnet
```

### `claude-agents`

Open the Claude Code agent view (`claude agents`) inside the VM. Claude Code's background agents are managed by its own daemon, so closing the terminal does not stop in-flight sessions; reconnect with `coop claude-agents` to see them again.

If the remote TUI stops responding, type Enter, then `~.` to disconnect the SSH session. coop forces OpenSSH's interactive escape character to `~`, so this works even if your user SSH config disables or changes `EscapeChar`. If the terminal remains in a broken raw/no-echo state afterward, run `stty sane`.

```
coop claude-agents [NAME] [FLAGS] [ARGS...]
coop ca [NAME] [FLAGS] [ARGS...]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `ARGS...` | Extra arguments passed through to `claude agents` |

Alias: `ca`.

```
coop claude-agents
coop ca my-project
coop ca my-project -- --cwd /workspace
```

### `codex`

Launch Codex inside the VM.

```
coop codex [NAME] [FLAGS] [ARGS...]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `ARGS...` | Extra arguments passed through to `codex` |

```
coop codex
coop codex my-project -- --model gpt-5
```

### `exec`

Run a command in the VM and print its output. No PTY is allocated and stdin is not forwarded; use `shell` for interactive work.

The command and its arguments must follow `--` so they are not mistaken for the instance name.

```
coop exec [NAME] -- COMMAND...
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `COMMAND...` | Command and arguments to run after `--` (required) |

```
coop exec -- uname -a
coop exec my-project -- docker ps
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

### `list`

Print every instance with its state (`running` or `stopped`). Reads from local on-disk state only — no SSH probing, so it returns instantly even when VMs are unreachable. Use `status` instead when you need resource usage or per-instance detail.

```
coop list
coop ls
```

Alias: `ls`.

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

Copy a local directory into the running VM at `/workspace`. Defaults to the
host path recorded when the instance was created with `coop up`.

```
coop push [NAME] [FLAGS]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--dir <dir>` | Local directory to push (defaults to the workspace host path) |
| `--force` | Overwrite guest changes without confirmation |
| `--exclude-git` | Skip the `.git/` directory in this transfer |

```
coop push
coop push my-project --dir ./src --force
```

### `pull`

Copy the VM's `/workspace` to a local directory. Defaults to the host path
recorded when the instance was created with `coop up`.

```
coop pull [NAME] [FLAGS]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--dir <dir>` | Local directory to pull into (defaults to the workspace host path) |
| `--force` | Overwrite local changes without confirmation |
| `--exclude-git` | Skip the `.git/` directory in this transfer |

```
coop pull
coop pull my-project --dir ./local-copy --force
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

### `uninstall`

Remove the coop binary and, optionally, its data directories (`~/.coop` and the update-check state). Refuses to remove the binary when it lives under `target/debug/` or `target/release/` so `cargo run -- uninstall` does not delete your build artifact.

```
coop uninstall [FLAGS]
```

| Flag | Description |
|------|-------------|
| `-y`, `--yes` | Skip interactive confirmation prompts. Removes data unless `--keep-data` is set. |
| `--keep-data` | Remove only the binary; preserve `~/.coop` and the update-check state. Conflicts with `--purge`. |
| `--purge` | Also remove `~/.coop` and the update-check state without prompting. Conflicts with `--keep-data`. Pairs with `--yes` for CI. |

Without `--yes`, the command prints a summary (binary path, data directory, instance and image counts) and asks for confirmation. A second prompt asks whether to also remove the data directory unless `--keep-data` or `--purge` is set. Non-interactive runs require `--yes`.

If the binary lives in a protected directory (e.g. `/usr/local/bin`), run with `sudo`. A config file outside the data directory is left in place and a note is printed.

```
coop uninstall                       # interactive: prompts for binary and data
coop uninstall --yes                 # CI: remove binary and data, no prompts
coop uninstall --yes --keep-data     # CI: remove binary only
coop uninstall --yes --purge         # CI: remove binary and data, explicit
```

### `completions`

Print a static shell completion script. Pair with `source <(COMPLETE=<shell> coop)` in your shell rc for dynamic completion of live instance, image, and profile names. See [docs/shell-completion.md](shell-completion.md) for full setup recipes per shell.

```
coop completions <SHELL>
```

| Argument | Description |
|----------|-------------|
| `SHELL` | Target shell: `bash`, `zsh`, `fish`, `powershell`, or `elvish` |

```
coop completions bash | sudo tee /etc/bash_completion.d/coop > /dev/null
coop completions bash > ~/.local/share/bash-completion/completions/coop
coop completions zsh > ~/.zfunc/_coop
coop completions fish > ~/.config/fish/completions/coop.fish
```

### `github`

Manage GitHub authentication. Specifically, the scoped fine-grained PAT (FGPAT) workflow that pairs `github = "pat"` mode with per-repo `[github.pat."owner/repo"]` entries in `config.toml`. See the [GitHub auth section](configuration.md#github-auth) of the configuration reference for the full data model.

```
coop github <subcommand>
```

| Subcommand | Effect |
|------------|--------|
| `setup-pat [--repo owner/name]` | Run the wizard end-to-end: open the GitHub PAT-creation form, validate the pasted token against `api.github.com`, store it in a chosen secret manager (Keychain / Secret Service / 1Password / file), and write a `[github.pat."owner/repo"]` entry. The repo is auto-detected from `git remote get-url origin` when `--repo` is omitted. |
| `rotate-pat --repo owner/name` | Re-run the wizard for an existing entry (FGPATs expire — max 1 year). |
| `status [--probe]` | List configured entries and their storage backend. By default the cmd-invocation is *not* resolved (so Keychain / 1Password prompts don't fire). Pass `--probe` to also resolve each entry and report whether the secret store still serves it. |
| `forget-pat --repo owner/name` | Delete the stored secret from its backend and drop the `[github.pat."owner/repo"]` entry. Does **not** add a skip marker — use the auto-prompt's `never` answer if you want coop to stop asking about this repo. Does **not** revoke the PAT on GitHub. |

```
coop github setup-pat --repo trailofbits/coop
coop github status
coop github status --probe
coop github rotate-pat --repo trailofbits/coop
coop github forget-pat --repo trailofbits/coop
```

### `validate`

Check the configuration file and prerequisites. Prints warnings and confirms the config loads correctly. With `--probe`, also exercises each `[github.pat]` entry against `api.github.com` to confirm the token is still live.

```
coop validate
coop validate --probe
```

| Flag | Description |
|------|-------------|
| `--probe` | For each `[github.pat]` entry, resolve the token and call `GET /user` on `api.github.com` to confirm it authenticates. Network-dependent; may trigger Keychain / 1Password prompts on macOS. |
