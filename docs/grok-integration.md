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

`config_dir` selects a host directory to overlay into the guest's `~/.grok/`
on every agent bootstrap (`coop up` or `coop start`, without `--no-agents`).
The default is `~/.grok`; a custom path supports `~` expansion. The copied
entries are `AGENTS.md`, `auth.json`, `lsp.json`, `rules/`, `skills/`,
`commands/`, `plugins/`, `hooks/`, `agents/`, and `workflows/`.

Guest `config.toml` is the merge base. Host `config.toml` keys are
overlaid except `[plugins]`. coop then forces
`ui.permission_mode = "always-approve"` and, when `[grok.mcp_servers]` is
set, replaces the `mcp_servers` table. The host `[plugins]` table is
dropped: those names resolve through `installed-plugins/`, which is not
copied. Guest `[plugins]` (especially `enabled`) is kept. UI, model, and
HTTP Model Context Protocol entries travel.
Host-absolute paths (`auth_provider_command`, local marketplace `path =`)
will not resolve in the guest.

`plugins/` is the user-scoped *source* tree (markdown, scripts). It is
auto-trusted. Directory symlinks inside it (or `plugins/` itself as a
link) are skipped so a host checkout cannot be followed into the guest.
Hidden directories (`.git`, `.venv`, caches) and bare git repos (`*.git`)
inside a copied tree stay on the host. `.grok-plugin/` and
`.claude-plugin/` are copied so a manifest that points at a custom
component path still reaches the guest.
`installed-plugins/` and `registry.json` are **not** copied: they record
absolute host paths and local checkouts, so they are not portable from
macOS to a Linux guest. Marketplace plugins belong in `[grok] plugins` so
the guest installs them itself.

A copied `auth.json` is set to owner-only (`0600`) on the guest.

```toml
[grok]
config_dir = "~/.grok"
```

Files follow an overlay lifecycle: restart overwrites files still present on
the host, but host deletions do not delete previous guest copies.
`config_dir = false` stops copying and retains previous copies, including
guest `[plugins]`. A missing default source likewise retains previous
copies; custom paths must exist at config validation time. To remove
retained content, remove it in the guest or recreate the VM.

Project files under `/workspace` (`AGENTS.md`, `.grok/`) are already in the
workspace and do not need to be copied.

### Environment variable forwarding

`env_forward` lists additional environment variable names to forward from
the host to the guest via SSH `SendEnv`. These are forwarded on every SSH
session, not just during bootstrap.

`XAI_API_KEY` and `GITHUB_TOKEN` are handled through their own mechanisms
and do not need to appear here. An active VM PAT assignment rejects
`GITHUB_TOKEN` and `GH_TOKEN` in this list.

### MCP server registration

`mcp_servers` maps server names to their definitions. coop merges these
definitions into the guest `~/.grok/config.toml` under `mcp_servers`.
Stdio `env` values are host variable names in coop config; they are written
as `${NAME}` so Grok expands them from the guest environment, and those
host names are forwarded automatically. An active VM PAT assignment
rejects a mapping whose host name is `GITHUB_TOKEN` or `GH_TOKEN`.

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

1. **User content**: Overlay the allowlisted entries from `config_dir`
   into `~/.grok/` in the guest, including `auth.json` and `plugins/`
   when present. Host `config.toml` is merged in the next step.
2. **Managed settings**: Overlay host `config.toml` keys onto the guest
   `~/.grok/config.toml` (except `[plugins]`), set
   `ui.permission_mode = "always-approve"`, and merge configured MCP
   servers. Guest `[plugins]` and other guest keys are kept.
3. **Folder trust**: Record `/workspace` in `~/.grok/trusted_folders.toml`.
4. **Marketplaces & plugins** (first boot only): Install the configured
   `marketplaces`/`plugins` not already baked into the golden image.

On restart, allowlisted files still present on the host are overlaid again
so host-side updates reach the guest. Guest-only files stay in place.
Marketplaces and plugins are not reinstalled.

### Skipping bootstrap

```bash
coop up . --no-agents
coop start --no-agents
```

This skips the guest bootstrap sequence entirely. The VM still includes the
Grok Build CLI because it is baked into the image during `coop setup`.

## Updating Grok Build

Grok Build auto-updates in the background by default. To force an update
immediately, run `grok update` inside the VM, or from the host:

```bash
coop agent update --grok
```

`coop agent update --grok` runs `grok update` synchronously as the guest
user. See [`agent update`](commands.md#agent-update).
