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

By default, `up` copies/syncs the project into `/workspace`. Pass `--mount`
to use the mount transport for the project at `/workspace` instead. On
macOS/Lima this is a live virtiofs mount; on Linux/Firecracker it is a
one-time sync. `--copy` is accepted as an explicit spelling of the default.
Use `--git-repo <url>` instead of `DIR` to clone a remote repository into
`/workspace` inside the guest.

| Flag | Description |
|------|-------------|
| `DIR` | Project directory (default: current directory) |
| `--name <name>` | Instance name to use when creating the project environment |
| `--copy` | Copy/sync `DIR` into `/workspace` (default) |
| `--mount` | Mount `DIR` at `/workspace` instead of using `--copy` |
| `--extra-mount <spec>` | Additional host directory to mount into the guest (`HOST_PATH[:GUEST_PATH]`, repeatable; specify a guest path other than `/workspace` when using `--copy`) |
| `--git-repo <url>` | Clone a git repository into `/workspace` instead of copying a local project directory |
| `--vcpus <N>` | Number of vCPUs when creating a new instance |
| `--mem <MiB>` | Memory in MiB when creating a new instance |
| `--disk <GiB>` | Instance disk size when creating a new instance |
| `--no-agents` | Skip injecting Claude Code and Codex credentials/config into the VM |
| `--image <name>` | Named image to use when creating a new instance (default: `default`) |
| `--profile <list>` | Build or reuse a profile-derived image when creating a new instance, named from the sorted profiles (for example `node-python`) |
| `--exclude-git` | Skip `.git/` when copying/syncing local directories; does not strip `.git` from a `--git-repo` clone |
| `--no-prompt` | Suppress the interactive prompt to set up a scoped GitHub PAT when one is missing for the resolved repo |
| `--forward-port <spec>` | Forward a guest port to the host (`GUEST[:HOST]`, repeatable) |
| `--post-start <cmd>` | Shell command to run inside the guest after boot |
| `--env KEY=VALUE` | Literal env var to set in the guest (repeatable) |
| `--devcontainer <path>` | Explicit path to a `devcontainer.json` to use (skips discovery and prompt) |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json` for this invocation |
| `--dry-run` | Translate `devcontainer.json` and print the report, then exit before any VM work |
| `--json` | With `--dry-run`, emit the resolved plan as JSON on stdout (`{ report, profiles, guest_user, vm }`) instead of the text report on stderr |

```
coop up .
coop up ~/code/my-project --mount
coop up . --profile python,node
coop up . --copy --forward-port 3000
coop up . --extra-mount ~/data:/data
coop up --git-repo https://github.com/trailofbits/coop.git
```

Creation options such as `--vcpus`, `--mem`, `--disk`, `--image`,
`--profile`, `--extra-mount`, `--git-repo`, `--exclude-git`, and
`--devcontainer` are applied only when `up` creates a new instance. `coop up
--profile <list>`
derives an image name from the sorted profile list, runs the same stale-image
check as `coop setup`, and builds or rebuilds that image if needed. Explicit
named images are unchanged: use `coop setup --image <name> --profile ...`
followed by `coop up --image <name>` when you want to choose the image name
yourself. If a matching project instance already exists, destroy it first to
recreate it with different creation options. Runtime startup options such as
`--forward-port`, `--post-start`, and `--env` can be used when `up` creates or
restarts an instance; if the matching instance is already running, stop it
first so those options can take effect.

When a local `devcontainer.json` was applied while creating the instance, coop stores
its path and content hash. Later `coop up` reconnects or restarts warn if that
file changed, but the existing VM is not mutated automatically. Destroy and
recreate the instance to apply creation-time devcontainer changes such as
`features`, `hostRequirements`, `mounts`, `image`/`build`, or `remoteUser`.

### `quickstart`

One-shot entry point: ensure the default image exists, bring up an instance for
the current directory, and launch Claude Code inside it. Runs `setup`, `up`, and
`claude` in sequence, short-circuiting any step that is already done.

```
coop quickstart [FLAGS]
```

`quickstart` resolves the workspace to the current directory (unless
`--no-workspace`) and uses it as the project identity, the same way `coop up`
does. It then takes one of three branches based on the instance for that
workspace:

- **A running instance exists.** coop reconnects to it and launches Claude Code —
  no setup, restart, or recreation.
- **A stopped instance exists.** coop restarts it (reusing the instance's own
  image, not necessarily `default`) and launches Claude Code.
- **No instance exists.** coop creates one for the workspace, folding in any
  discovered `devcontainer.json`, then launches Claude Code.

Image setup runs only when the `default` template image is missing; otherwise it
is skipped. Setup runs non-interactively (no confirmation prompts).

| Flag | Description |
|------|-------------|
| `--no-workspace` | Skip mounting the current directory as the workspace. Without a workspace there is no instance to match, so quickstart always creates a fresh instance rather than reusing an existing one. |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json` (escape hatch for CI). |

```
coop quickstart
coop quickstart --no-devcontainer
coop quickstart --no-workspace
```

`quickstart` takes no instance name or per-instance tuning flags. For control
over profiles, mounts, image, resources, or named instances, use `coop setup`
and `coop up` directly. When run from your `$HOME` or `/`, coop prompts before
mounting (or bails in a non-TTY); pass `--no-workspace` to skip the mount.

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
| `--builder-timeout <duration>` | Duration to wait for setup image build commands before timing out. Accepts seconds by default, or `s`, `m`, and `h` suffixes. |
| `--workspace <dir>` | Scan for `.devcontainer/devcontainer.json` and offer to apply its `features` / `hostRequirements` to this setup. Supported public `ghcr.io/devcontainers/features/*` entries are resolved and baked into the image. |
| `--devcontainer <path>` | Explicit path to a `devcontainer.json` to use (skips discovery and prompt). |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json` for this invocation. |
| `--dry-run` | Translate `devcontainer.json` and print the report, then exit before any setup work. |

```
coop setup -y --profile python,node --template-size 12
coop setup --image ml-dev --profile python --extra-packages libopenblas-dev
coop setup -y --workspace . --devcontainer .devcontainer/devcontainer.json
```

See [docs/devcontainer.md](devcontainer.md) for the subset of `devcontainer.json` coop reads.

### `devcontainer check`

Parse a `devcontainer.json` file and print the same translation report that `setup --dry-run` and `start --dry-run` use, without loading coop config, checking for updates, setting up an image, or starting a VM. Setup-stage checks resolve supported public GHCR OCI Features so the report can show the digest and `install.sh` hash that would run.

```
coop devcontainer check <path> [--stage setup|start|both]
```

| Flag | Description |
|------|-------------|
| `<path>` | Path to the `devcontainer.json` file to inspect |
| `--stage <stage>` | Which lifecycle translation to report: `setup`, `start`, or `both` (default: `both`) |
| `--json` | Emit the report as JSON on stdout instead of the text table on stderr |

Use `--stage setup` to inspect setup-time keys such as `features`, `hostRequirements.cpus`, `hostRequirements.memory`, and `remoteUser`. Use `--stage start` to inspect start-time keys such as `postStartCommand`, `containerEnv`, `forwardPorts`, `mounts`, and `hostRequirements.storage`.

With `--json`, a single stage emits one report object (`{ entries, source_path, ignored_paths }`, or `null` when no file applied); `--stage both` emits `{ "setup": <report>, "start": <report> }`. Each entry is `{ key, status, source, value, note }`, with `status` one of `applied`/`overridden`/`unsupported`/`invalid` and `source` one of `cli`/`devcontainer`. CI can branch on the translation without scraping the table.

### `devcontainer ignore`

Record a persistent opt-out for a project directory. Future automatic discovery for that project skips `.devcontainer/devcontainer.json` and reports that the stored preference was used. Explicit `--devcontainer <path>` still applies a file for that run.

```
coop devcontainer ignore <project-dir>
```

### `devcontainer status`

Inspect persistent devcontainer opt-outs. With no project argument, this lists all stored opt-outs.

```
coop devcontainer status [project-dir]
```

### `devcontainer clear`

Remove a persistent devcontainer opt-out for a project. If the project directory was moved or deleted, use the absolute path shown by `coop devcontainer status`.

```
coop devcontainer clear <project-dir>
```

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
| `--workspace <dir>` | Restart the stopped instance associated with this project path |
| `--no-agents` | Skip injecting Claude Code and Codex credentials/config into the VM |
| `--forward-port <spec>` | Forward a guest port to the host (`GUEST[:HOST]`, repeatable). Lives for the lifetime of the VM; torn down on `coop stop`. |
| `--no-prompt` | Suppress the interactive prompt to set up a scoped GitHub PAT when one is missing for the resolved repo (see [`coop github setup-pat`](#github)). |
| `--post-start <cmd>` | Shell command to run inside the guest after boot. Overrides the `post_start` field in `config.toml`. Failure is logged but does not fail the start. |
| `--env KEY=VALUE` | Literal env var to set in the guest (repeatable). Overrides `guest_env` config entries and any forwarded values with the same name. |
| `--devcontainer <path>` | Dry-run translation aid; normal restarts reject devcontainer creation options. |
| `--no-devcontainer` | Ignore any discovered `devcontainer.json` for this invocation (escape hatch for CI). |
| `--dry-run` | Translate `devcontainer.json` and print the report, then exit before any VM work. |
| `--json` | With `--dry-run`, emit the resolved plan as JSON on stdout instead of the text report on stderr. |

Normal `start` restarts an existing VM without re-reading or re-applying
`devcontainer.json`. If the instance was created with a devcontainer file, coop
warns when the recorded file path now has different contents. See
[docs/devcontainer.md](devcontainer.md) for the supported keys, discovery
rules, and recreate guidance.

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

Launch Codex inside the VM. By default coop passes `--dangerously-bypass-approvals-and-sandbox`, so Codex runs without its sandbox or approval prompts — parity with `coop claude`. The VM is the isolation boundary, and Codex's own Linux sandbox does not work in the guest (no functioning bubblewrap), so leaving it enabled makes every shell command Codex runs fail. Use `--ask` to keep Codex's sandbox and approval prompts for that session.

```
coop codex [NAME] [FLAGS] [ARGS...]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--ask` | Keep Codex's sandbox and approval prompts instead of bypassing them |
| `ARGS...` | Extra arguments passed through to `codex` |

```
coop codex
coop codex my-project --ask
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

| Flag | Description |
|------|-------------|
| `--json` | Emit a JSON array (`[{ "name", "state" }, …]`) instead of the text table |

### `status`

Print instance status. Without a name, lists every instance with its state, image, backend, and resource usage (for running instances). With a name, prints detailed status for that instance.

```
coop status [NAME]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (shows all if omitted) |
| `--json` | Emit machine-readable JSON instead of the text output |

```
coop status
coop status my-project
```

With `--json`, a bare `coop status` emits a JSON array and `coop status NAME`
emits a single object. Each carries the common fields — `name`, `state`
(`running`/`stopped`), `image`, `backend` (`firecracker`/`lima`), and `usage`
(raw MiB / load, or `null` when stopped or the query fails). The rich
single-instance text report (guest IP, PID, SSH port, …) is text-only. JSON goes
to stdout; tracing stays on stderr, so `coop status --json | jq` stays clean.

```
$ coop status my-project --json
{
  "name": "my-project",
  "state": "running",
  "image": "default",
  "backend": "firecracker",
  "usage": { "load_1m": 0.12, "mem_used_mib": 512, "mem_total_mib": 2048,
             "disk_used_mib": 8192, "disk_total_mib": 20480 }
}
```

### `agent update`

Update the coding agents (Claude Code and Codex) installed inside a running VM
to their latest versions, without rebuilding the golden image. Both agents are
installed "latest at build time" during `coop setup`, so they can go stale in
long-running VMs and in new VMs created from an old image. To refresh the image
itself instead, rebuild it with `coop setup --rebuild`.

```
coop agent update [NAME] [--claude] [--codex] [--check] [-y]
```

| Argument / Flag | Description |
|-----------------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--claude` | Update Claude Code |
| `--codex` | Update Codex |
| `--check` | Only report installed vs. latest versions — change nothing |
| `-y`, `--yes` | Skip the confirmation prompt |

With no agent flag, both agents are updated; passing both `--claude` and
`--codex` is the same as passing neither. The VM must be running.

Codex has no background updater, so `coop agent update --codex` re-runs coop's
own installer inside the guest as root, overwriting `/usr/local/bin/codex` with
the current release. Claude Code already auto-updates in the background;
`coop agent update --claude` runs `claude update` now, synchronously — a
convenience rather than a fix.

`--check` reports each agent's installed version and, for Codex, the latest
release on GitHub, changing nothing:

```
$ coop agent update my-project --check
Claude Code  1.2.3            up to date (auto-updates in background)
Codex        0.4.1 → 0.5.0    update available — run: coop agent update --codex
```

```
coop agent update                 # both agents, resolved instance
coop agent update my-project      # both agents, instance "my-project"
coop agent update --codex         # Codex only
coop agent update --check         # report versions, change nothing
```

### `model`

Show or switch a VM's model backend between cloud (Anthropic / OpenAI) and a
host-side local model server. The selection is per-instance and persists across
restarts. Switching rewrites the guest agent config; it never rebuilds or
restarts the VM.

```
coop model [NAME] [local|remote]
```

| Argument | Description |
|----------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `local` | Route this VM's agents at a host-side local model server |
| `remote` | Restore cloud defaults (Anthropic / OpenAI) |

With no subcommand, `model` prints the current mode and the endpoint each tool
(Claude, Codex) resolves to:

```
$ coop model my-project
Instance: my-project
Mode:     local
Claude   local — qwen2.5-coder:32b @ http://172.16.0.1:11434
Codex    cloud (no local endpoint configured)
```

`coop model NAME local` switches the VM to local mode. Each tool routes locally
only if it resolves an endpoint — from `[claude.local_model]` /
`[codex.local_model]` in `config.toml`, or from one saved earlier. For any tool
that has neither, and only in an interactive terminal, coop prompts for a host
URL, model name, and optional auth token, then saves that endpoint for the
instance. (A non-interactive run declines the prompt.) If no tool ends up with
an endpoint, the command fails. Claude and Codex are independent: you can put
one on a local model and leave the other on cloud.

`coop model NAME remote` switches back to cloud defaults for both tools. Saved
endpoints are kept, so a later `local` does not re-prompt.

Switching never requires a VM restart — coop rewrites the guest config live over
SSH when the VM is running, or saves it to apply on the next start. An
already-running `claude`/`codex` reads its config at launch, so relaunch the
agent (for example `coop claude NAME`) to pick up the change.

```
coop model
coop model my-project
coop model my-project local
coop model my-project remote
```

See the [`local_model`](configuration.md#local-model-routing) configuration
reference and the local-model sections of
[docs/claude-integration.md](claude-integration.md) and
[docs/codex-integration.md](codex-integration.md) for the resolution and
materialization details.

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

### `ssh-config`

Install a `coop-<name>` alias into `~/.ssh/config` so plain `ssh`, `scp`, and
`rsync` reach the guest without remembering its host, port, user, or key. This
is the same SSH config block `coop vscode` writes, but without launching an
editor.

```
coop ssh-config [NAME] [--clean]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--clean` | Remove the SSH config entry for this instance and exit |

```
coop ssh-config
coop ssh-config my-project
ssh coop-my-project
scp ./file coop-my-project:/workspace/
rsync -az ./dir/ coop-my-project:/workspace/dir/
coop ssh-config my-project --clean
```

The alias is created only when you run `coop ssh-config` (or `coop vscode`).
The lifecycle keeps it tidy: `coop stop` and `coop destroy` remove the block,
and `coop start` refreshes an already-installed block so it stays valid across
a restart. On macOS/Lima the forwarded SSH port changes on each start; the
refresh keeps the alias current without you re-running the command. On
Linux/Firecracker the host and port are stable, so the refresh is a no-op.

The block sets `StrictHostKeyChecking no` and `UserKnownHostsFile /dev/null`,
so `ssh coop-*` connections skip host-key verification. This is intentional —
these VMs regenerate their host keys, so pinning them would only produce
spurious mismatch warnings.

Use `ssh-config` for ad-hoc copies of arbitrary paths. To sync the tracked
workspace directory in bulk, use [`push`](#push) / [`pull`](#pull) instead.

### `images`

List or delete golden images. Without flags, prints every image with its profiles, creation date, and size.

```
coop images [FLAGS]
```

| Flag | Description |
|------|-------------|
| `--delete <name>` | Delete a named image |
| `--json` | Emit a JSON array instead of the text table |

```
coop images
coop images --delete old-image
```

With `--json`, each element is `{ "name", "profiles", "created", "size_bytes" }`.
Absence is modelled honestly: `profiles` is `[]` (not `"none"`), `created` is
`null` (not `"unknown"`), and `size_bytes` is the raw byte count (the text path's
`"8.0 GiB"` is presentation only).

### `resize`

Change a stopped instance's disk size, memory, or vCPU count. The VM must
be stopped first. At least one of `--size`, `--mem`, or `--vcpus` is required;
they can be combined in a single command.

```
coop resize [NAME] [--size <SIZE>] [--mem <MIB>] [--vcpus <N>] [--start]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--size <size>` | New disk size. Absolute: `150` or `150G`. Relative: `+20` or `+20G`. |
| `--mem <mib>` | New memory in MiB (minimum 128). |
| `--vcpus <n>` | New vCPU count (> 0). |
| `--start` | Start the instance after applying the change instead of leaving it stopped. |

Absolute disk values set the disk to that exact size; a `+` prefix adds to the
current size. Memory and vCPU changes are written to the instance's backend
config (the Firecracker per-instance JSON or the Lima `lima.yaml`), which is
authoritative — the value survives restarts and is reported by `coop status`.
The global `[vm]` settings in `config.toml` only seed these values for *new*
instances.

By default the instance is left stopped and the change takes effect on the next
`coop start`. Pass `--start` to boot it immediately. On Firecracker, if a
`--start` boot fails (e.g. more memory than the host has), the previous mem/vcpu
is restored so a plain `coop start` still works; on Lima the `lima.yaml` is
likewise restored.

Combining `--size` with `--mem`/`--vcpus` applies the disk change first, then the
machine-resource change; the two are separate artifacts and are not applied
transactionally, so if the second step fails the disk change has already taken
effect.

```
coop resize my-project --size 150G
coop resize --size +20
coop resize my-project --mem 8192 --vcpus 4
coop resize my-project --mem 4096 --start
```

### `commit`

Save a stopped instance's filesystem as a reusable image, like `docker container commit`. The committed image is an ordinary coop image: `coop images` lists it and `coop up --image <name>` launches new instances from it. The instance must be stopped first so the filesystem is consistent.

```
coop commit [NAME] --image <name> [FLAGS]
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--image <name>` | Name of the image to create (required) |
| `--force` | Overwrite an existing image with the same name |

```
coop stop my-project
coop commit my-project --image my-project-baseline
coop up . --image my-project-baseline --name fork
```

### `restore`

Roll a stopped instance back to an image's filesystem in place. The instance keeps its name, index, IP, and workspace association — only the disk is replaced and its recorded image is updated. Run `coop start` afterwards to bring it back up.

This pairs with `commit` for a known-good checkpoint before a risky run:

```
coop stop my-project
coop commit my-project --image safe-point   # checkpoint
coop start my-project
# ... a bypass-permissions agent run trashes the environment ...
coop stop my-project
coop restore my-project --image safe-point   # back to the checkpoint, same VM
coop start my-project
```

```
coop restore [NAME] --image <name>
```

| Flag | Description |
|------|-------------|
| `NAME` | Instance name (required if multiple instances exist) |
| `--image <name>` | Name of the image to restore from (required) |

Unlike `destroy` + `up --image`, `restore` keeps the same instance identity (name, index, IP) instead of allocating a new one. The disk is reset to the image's size, so restoring an image built before a `coop resize` returns the instance to the smaller size.

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
coop profiles list --json
coop profiles show rust
```

`list` groups builtin and custom profiles separately. `show` resolves the name against custom profiles first, then builtins, and prints `(custom)` or `(builtin)` next to the name.

`coop profiles list --json` emits `{ "builtin": [...], "custom": [...] }`, each entry `{ "name", "summary" }`.

### `update`

Replace the running coop binary with a release from `github.com/trailofbits/coop`. Downloads the tarball matching the current host triple, verifies its SHA-256 against the release's `SHA256SUMS`, and (when `gh` is installed) verifies the GitHub build-provenance attestation before swapping the binary atomically.

No authentication is required. When [`gh`](https://cli.github.com/) is authenticated against `github.com` or `GITHUB_TOKEN` is set, `coop update` uses it, which helps avoid GitHub API rate limits.

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
| `status [--probe] [--json]` | List configured entries and their storage backend. By default the cmd-invocation is *not* resolved (so Keychain / 1Password prompts don't fire). Pass `--probe` to also resolve each entry and report whether the secret store still serves it. Pass `--json` for machine-readable output. |
| `forget-pat --repo owner/name` | Delete the stored secret from its backend and drop the `[github.pat."owner/repo"]` entry. Does **not** add a skip marker — use the auto-prompt's `never` answer if you want coop to stop asking about this repo. Does **not** revoke the PAT on GitHub. |

```
coop github setup-pat --repo trailofbits/coop
coop github status
coop github status --probe
coop github status --json
coop github rotate-pat --repo trailofbits/coop
coop github forget-pat --repo trailofbits/coop
```

`coop github status --json` emits `{ "mode", "entries", "skip" }`. `mode` is
`off`/`auto`/`env`/`pat`; each entry is `{ "repo", "storage", "probe" }` with
`storage` a stable token (`macos_keychain`/`linux_secret_service`/`one_password`/
`file`, or `null` when unparseable) and `probe` (`ok`/`unexpected_format`/
`resolve_failed`, or `null` unless `--probe`). The token value is never emitted.

### `proxy`

Manage the host-side credential-injecting proxy. When a `[proxy.<provider>]`
upstream is configured, coop runs a `coop-proxy` process on the host for the
lifetime of each remote-mode VM: the guest is pointed at the proxy and holds
only a per-instance capability token, while the real API key stays on the host
and is injected onto outbound requests the guest never sees. Applies only in
remote model mode (`coop model <vm> remote`); local mode takes precedence. See
the [credential proxy guide](credential-proxy.md) and the
[`proxy` configuration reference](configuration.md#proxy-section) for the data
model.

| Subcommand | Effect |
|------------|--------|
| `setup [--anthropic] [--openai] [--vm <name>] [--api-key]` | Store a provider credential in a secret backend and wire it into `[proxy.<provider>]` (the default) or a per-VM override. Anthropic (Claude) is the default provider; pass `--openai` for Codex. `--vm <name>` stores the credential as a per-VM override in that instance's state instead of the global default. Anthropic only: `--api-key` stores an API key (`x-api-key`) instead of a Claude `setup-token`; ignored for `--openai`, whose keys are always injected as `Authorization: Bearer`. |
| `status [--vm <name>]` | Show what each VM's agents resolve to (per-VM override → default → off), with credentials redacted. Pass `--vm <name>` for the effective resolution of a single VM instead of all. |

```
coop proxy setup
coop proxy setup --openai
coop proxy setup --vm my-project
coop proxy setup --api-key
coop proxy status
coop proxy status --vm my-project
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
