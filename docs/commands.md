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
| `--workspace <dir>` | Scan for `.devcontainer/devcontainer.json` and offer to apply its `features` / `hostRequirements` to this setup. |
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
| `--forward-port <spec>` | Forward a guest port to the host (`GUEST[:HOST]`, repeatable). Lives for the lifetime of the VM; torn down on `coop stop`. |
| `--exclude-git` | Skip the `.git/` directory when syncing the workspace (conflicts with `--git-repo`). |
| `--no-prompt` | Suppress the interactive prompt to set up a scoped GitHub PAT when one is missing for the resolved repo (see [`coop github setup-pat`](#github)). |
| `--post-start <cmd>` | Shell command to run inside the guest after boot. Overrides the `post_start` field in `config.toml`. Failure is logged but does not fail the start. |
| `--env KEY=VALUE` | Literal env var to set in the guest (repeatable). Overrides `guest_env` config entries and any forwarded values with the same name. |
| `--devcontainer <path>` | Explicit path to a `devcontainer.json` to use (skips discovery and prompt). |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json` (escape hatch for CI). |
| `--dry-run` | Translate `devcontainer.json` and print the report, then exit before any VM work. |

When `--workspace <dir>` contains a `.devcontainer/devcontainer.json` (or one of `--mount`'s host roots does), coop reads a subset of it and prompts before applying. See [docs/devcontainer.md](devcontainer.md) for the supported keys and discovery rules.

`--workspace` and `--git-repo` are mutually exclusive. Use `--workspace` to tar-pipe a local directory into the guest. Use `--git-repo` to clone a repository inside the VM at boot.

For private GitHub repos, `--git-repo` resolves a host-side token (`gh auth token` first, then `GITHUB_TOKEN`) and hands it to git in the guest via a one-shot credential helper. Without a token, the clone runs unauthenticated and will fail for private repos.

```
coop start my-project --workspace ./src --vcpus 4 --mem 8192
coop start --git-repo https://github.com/org/repo.git --disk 50
coop start --image ml-dev --no-agents
coop start --mount ~/data:/mnt/data
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

Launch Claude Code inside the VM. The guest's `~/.claude/settings.json` (written during `coop start`) sets `defaultMode: bypassPermissions` and `skipDangerousModePermissionPrompt: true`, so Claude Code runs without permission prompts — the VM itself is the isolation boundary. Use `--ask` to override the guest default for that session (coop passes `--permission-mode default`).

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

Copy a local directory into the running VM at `/workspace`. Defaults to the host path recorded when the instance was started with `--workspace`.

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

Copy the VM's `/workspace` to a local directory. Defaults to the host path recorded when the instance was started with `--workspace`.

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
