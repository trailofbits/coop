# Grok Build Integration

coop installs Grok Build into every guest image and gives you a dedicated
`coop grok` launcher. This guide covers the command, the configuration that
controls what gets injected into the guest, and the bootstrap sequence that
runs when a VM starts.

## Launching Grok Build

```bash
coop grok [instance-name] [-- extra-args...]
```

This SSHes into the guest and runs the `grok` CLI with `--always-approve`,
`--trust`, and `--cwd /workspace`. The VM is the isolation boundary, so Grok
Build's own permission prompts are redundant. `--trust` records `/workspace`
as a trusted folder so project `.grok/` hooks, Model Context Protocol servers,
and permission rules load without a first-run question.

To restore permission prompts for a single session, pass `--ask`. coop then
passes `--permission-mode default`, which overrides the guest
`ui.permission_mode = "always-approve"` (folder trust and the working
directory stay):

```bash
coop grok --ask
```

Trailing arguments go straight through to the `grok` CLI:

```bash
coop grok -- --model grok-4.6
```

`login` and `logout` run without `--always-approve`. If the host has
`~/.grok/auth.json` (from `grok login` on the host), coop copies it into the
guest on boot — same as Codex — and sets it to owner-only (`0600`). A
copied session token takes precedence over `XAI_API_KEY`. If there is no
host file, sign in from the guest with device-code auth (there is no
browser in the VM):

```bash
coop grok -- login --device-auth
```

## Configuration

Grok-related settings live under the `[grok]` section in `config.toml`, except
`github` which is a top-level field:

```toml
github = "auto"

[grok]
api_key = "xai-..."
env_forward = ["MYORG_KEY"]
config_dir = "~/.grok"

[grok.mcp_servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

Every field is optional. An empty `[grok]` section (or omitting it entirely)
still installs the CLI in the image and still writes managed guest settings
on boot.

### API key forwarding

coop forwards `XAI_API_KEY` to the guest via SSH `SendEnv` on every session:
`coop grok`, `coop shell`, and `coop exec` alike. The key is never written to
disk inside the guest.

Resolution order:

1. `grok.api_key` in `config.toml`
2. `XAI_API_KEY` environment variable on the host

If neither is set, the guest starts without an API key unless host
`auth.json` was copied (see [Config directory](#config-directory)).

A grok.com session token in `~/.grok/auth.json` takes precedence over the
forwarded API key. That file comes from the host copy on boot, or from
`coop grok -- login --device-auth` inside the guest.

### Config directory

`config_dir` specifies a host directory from which coop copies an allowlist
of entries (`AGENTS.md`, `auth.json`, `config.toml`, `lsp.json`, `rules/`,
`skills/`, `commands/`, `plugins/`, `hooks/`, `agents/`, `workflows/`) into
`~/.grok/` in the guest.

`config.toml` is the merge base: coop then forces
`ui.permission_mode = "always-approve"` and, when `[grok.mcp_servers]` is
set, replaces the `mcp_servers` table. The host `[plugins]` table is
dropped: those names resolve through `installed-plugins/`, which is not
copied. UI, model, and HTTP Model Context Protocol entries travel.
Host-absolute paths (`auth_provider_command`, local marketplace `path =`)
will not resolve in the guest.

`plugins/` is the user-scoped *source* tree (markdown, scripts). It is
auto-trusted. Directory symlinks inside it (or `plugins/` itself as a
link) are skipped so a host checkout cannot be followed into the guest.
`installed-plugins/` and `registry.json` are **not** copied: they record
absolute host paths and local checkouts, so they are not portable from
macOS to a Linux guest. Marketplace plugins belong in `[grok] plugins` so
the guest installs them itself.

A copied `auth.json` is set to owner-only (`0600`) on the guest.

```toml
[grok]
config_dir = "~/.grok"
```

The default is `~/.grok`. Set to `false` to disable config file copying
entirely. Project files under `/workspace` (`AGENTS.md`, `.grok/`) are
already in the workspace and do not need to be copied.

### Environment variable forwarding

`env_forward` lists additional environment variable names to forward from
the host to the guest via SSH `SendEnv`. These are forwarded on every SSH
session, not just during bootstrap.

`XAI_API_KEY` and `GITHUB_TOKEN` are handled through their own mechanisms
and do not need to appear here.

### MCP server registration

`mcp_servers` maps server names to their definitions. coop merges these
definitions into the guest `~/.grok/config.toml` under `mcp_servers`.
Stdio `env` values are host variable names in coop config; they are written
as `${NAME}` so Grok expands them from the guest environment, and those
host names are forwarded automatically.

Definitions use the same schema as Claude and Codex integration.

### Plugin marketplaces

`marketplaces` and `plugins` declare Grok Build plugin marketplaces and the
plugins to install from them:

```toml
[grok]
marketplaces = ["owner/grok-plugins"]
plugins = ["my-skill"]
```

Each marketplace source is registered with `grok plugin marketplace add` and
each plugin installed with `grok plugin install <name> --trust` (Grok takes
a plugin name after the marketplace is added, or a git URL / `owner/repo`
source). A source that is an absolute local directory is copied into the
guest first.

These are baked into the golden image during `coop setup` (on the Lima/macOS
backend) and recorded in the image's template config. On a VM's first boot
coop installs only the delta not already baked in; on the Firecracker/Linux
backend, where nothing is baked, the full set installs on first boot.

## Bootstrap sequence

When `coop up` creates/restarts a project VM or `coop start` restarts a
stopped VM (without `--no-agents`), coop executes the following steps after
the VM boots and SSH becomes available:

1. **User content**: Copy the allowlisted entries from `config_dir` to
   `~/.grok/` in the guest, including `auth.json`, `config.toml`, and
   `plugins/` when present.
2. **Managed settings**: Merge `ui.permission_mode = "always-approve"` into
   the guest `~/.grok/config.toml`, drop the host `[plugins]` table, and
   merge configured MCP servers. Other keys in that file are preserved.
3. **Folder trust**: Record `/workspace` in `~/.grok/trusted_folders.toml`.
4. **Marketplaces & plugins** (first boot only): Install the configured
   `marketplaces`/`plugins` not already baked into the golden image.

On restart, the same config files are refreshed so host-side updates are
reflected in the guest; marketplaces and plugins are not reinstalled.

### Skipping bootstrap

```bash
coop up . --no-agents
coop start --no-agents
```

This skips the guest bootstrap sequence entirely. The VM still includes the
Grok Build CLI because it is baked into the image during `coop setup`.

## Updating Grok Build

Grok Build auto-updates in the background by default. To force an update
immediately:

```bash
coop agent update --grok
```

This runs `grok update` synchronously inside the guest as the guest user.
See [`agent update`](commands.md#agent-update).
