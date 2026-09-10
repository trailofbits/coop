# Claude Code Integration

coop sets up Claude Code inside guest VMs and gives you a single command to launch it. This guide covers the `coop claude` command, the configuration that controls what gets injected into the guest, and the bootstrap sequence that runs when a VM starts.

## Launching Claude Code

```bash
coop claude [instance-name] [-- extra-args...]
```

This SSHes into the guest and runs the `claude` CLI. The guest's managed `~/.claude/settings.json` (written during VM startup) sets `defaultMode: bypassPermissions` and `skipDangerousModePermissionPrompt: true`, so Claude operates without confirmation prompts. The VM is the isolation boundary; permission prompts inside it are redundant.

To restore permission prompts for a single session, pass `--ask`. coop then launches `claude` with `--permission-mode default`, overriding the guest default:

```bash
coop claude --ask
```

Trailing arguments go straight through to the `claude` CLI:

```bash
coop claude -- --model sonnet --verbose
```

## Managing background agents

```bash
coop claude-agents [instance-name] [-- extra-args...]
# or the short alias:
coop ca
```

This runs `claude agents` in the guest, which opens the agent view — an interactive TUI for monitoring background agent sessions. Background sessions are managed by Claude Code (not by coop), so closing the TUI and reconnecting later with `coop ca` keeps you in sync with whatever is still running.

The agent view itself has no sign-in prompt, but it forwards a `/login` command to a new Claude Code session. If you haven't signed Claude in with `coop claude` (and aren't forwarding an `ANTHROPIC_API_KEY`), run `/login` at the start of your `coop ca` session to start the sign-in flow.

If the remote TUI appears stuck or stops responding, use OpenSSH's local escape: type Enter, then `~.` to disconnect. coop forces the interactive SSH escape character to `~`, so the escape path is available even if your user SSH config changes or disables `EscapeChar`. If your terminal remains in raw/no-echo mode after the disconnect, run:

```bash
stty sane
```

This is separate from SSH startup failures that exit with code 255. coop already restores the terminal after those failures; the escape sequence is for sessions where SSH is still connected and forwarding keystrokes to the remote TUI.

`claude agents` accepts `--cwd <path>` (filter sessions by working directory) and `--setting-sources <sources>`; pass them after `--`:

```bash
coop ca -- --cwd /workspace
```

Closing the TUI does not stop background sessions; reopening `coop ca` reattaches to whatever Claude Code's daemon is still running.

## Configuration

Claude-related settings live under the `[claude]` section in `config.toml`, except `github` which is a top-level field:

```toml
github = "auto"

[claude]
api_key = "sk-ant-..."
env_forward = ["MYORG_KEY"]
config_dir = "~/.claude"
marketplaces = [
  "https://github.com/anthropics/claude-plugins-official",
  "/path/to/local/marketplace",
]
plugins = ["rust-analyzer-lsp@claude-plugins-official"]

[claude.mcp_servers.sentry]
type = "http"
url = "https://mcp.sentry.dev/mcp"
```

Every field is optional. An empty `[claude]` section (or omitting it entirely) skips all bootstrap steps.

### API key forwarding

coop forwards `ANTHROPIC_API_KEY` to the guest via SSH `SendEnv` on every session: `coop claude`, `coop shell`, and `coop exec` alike. The key is never written to disk inside the guest.

Resolution order:

1. `claude.api_key` in `config.toml`
2. `ANTHROPIC_API_KEY` environment variable on the host

If neither is set, the guest starts without an API key. You can authenticate interactively the first time you run `claude` inside the VM.

### GitHub auth

The `github` field controls how coop obtains a `GITHUB_TOKEN` for the guest. This token enables private repo cloning and `gh` CLI usage inside the VM.

| Value    | Behavior |
|----------|----------|
| `"auto"` | Check the `GITHUB_TOKEN` env var first. If unset, run `gh auth token` on the host to extract a token from the GitHub CLI. |
| `"env"`  | Require `GITHUB_TOKEN` in the host environment. Warns if missing. |
| `"off"`  | Skip GitHub token forwarding entirely. This is the default when `github` is unset. |
| `"pat"`  | Use a per-repo fine-grained PAT from `[github.pat]`. Scope is server-enforced to one repo. Run `coop github setup-pat --repo owner/name` to add an entry; see [configuration.md](configuration.md#fine-grained-pat-github--pat) for the full reference. |

When a token is available, coop runs `gh auth setup-git` in the guest during bootstrap. This configures the git credential helper so `git clone` works against private repositories without further setup.

### Config directory

`config_dir` selects a host directory to overlay into the guest's `~/.claude/`
on every agent bootstrap (`coop up` or `coop start`, without `--no-agents`).
The default is `~/.claude`; a custom path supports `~` expansion:

```toml
[claude]
config_dir = "~/claude-customizations"
```

The copied entries are `CLAUDE.md`, `keybindings.json`, `rules/`, `commands/`, `skills/`, `agents/`, `output-styles/`, `themes/`, and `workflows/`.
Directories are copied recursively in full, including nested skill references,
scripts, executable assets, hidden supporting files, and skills-directory plugin
bundles. This uses the existing copier: symlinks are followed and materialized,
including targets outside the source. Directory names do **not** guarantee that
recursive content is free of credentials; select a source containing only content
you intend to put in the VM. Absolute host paths inside content are not rewritten.

Current Claude Code loading contracts (verified with 2.1.259):

| Content | Native behavior |
| --- | --- |
| `CLAUDE.md`, `rules/`, `commands/` | Personal instructions, rules and commands. |
| `skills/<name>/SKILL.md` | Personal skills with supporting files in the same bundle; see [skills](https://code.claude.com/docs/en/skills). |
| `skills/<directory>/.claude-plugin/plugin.json` | Immediate, non-hidden directories load as `<manifest-name>@skills-dir` without installation. Names are matched exactly in preferences; directory names and skill frontmatter do not determine plugin IDs. Hidden and reserved `synced` directories are not discovered. See [skills-directory plugins](https://code.claude.com/docs/en/plugins-reference#skills-directory-plugins). |
| `agents/` | Personal [subagents](https://code.claude.com/docs/en/sub-agents). |
| `output-styles/` | Personal [output styles](https://code.claude.com/docs/en/output-styles), selected by `outputStyle`. |
| `themes/` | Personal theme JSON files with `base` and `overrides`, available in `/theme`; selection from host `.claude.json` is not imported. See [themes](https://code.claude.com/docs/en/terminal-config). |
| `workflows/` | Personal `.js` workflows with a literal `export const meta` name/description, discovered as commands; see [workflows](https://code.claude.com/docs/en/workflows). Availability depends on Claude's workflow feature support. |
| `keybindings.json` | Native [keybindings](https://code.claude.com/docs/en/keybindings), read from the user config directory. |

Top-level `hooks/`, `monitors/`, and `routines/` are excluded. Merely copying a
hook script does not register it. Hooks and monitors **inside plugin bundles**
are copied with their registration files; Claude's own loading restrictions still
apply (plugin monitors are interactive-only). Host settings, credentials, sessions,
marketplaces, and `plugins/` installation/cache state are not copied wholesale.
Claude's separate synced-skills ownership registry, `skills/manifest.json`, is
also unsupported: import stops before transferring content when it is present.
Use a custom source with the personal bundles you want and without sync state.
Unreadable manifests or invalid plugin names likewise stop import rather than
risk dropping a disable preference. Invalid directory encoding is unsupported.

#### Companion preferences and refresh

From host `settings.json`, coop selects only:

- `disableAllHooks` (boolean): applies to all guest Claude hooks, including hooks
  within copied skills/plugins and pre-existing guest hooks.
- `outputStyle` (string): selects the guest output style by name. Claude resolves
  the name; a style supplied by an excluded marketplace installation is not
  installed by this import.
- Boolean `enabledPlugins` entries whose exact IDs belong to copied,
  discoverable skills-directory plugins, including IDs recorded from previous
  copies retained by the overlay. Absent entries leave guest choices or
  Claude's manifest/default enablement in effect; coop never synthesizes `true`.

Explicit imported values override the corresponding guest user preferences.
Unrelated guest settings and plugin entries survive. Coop's managed permissions,
authentication handling, and model/proxy routing retain their existing precedence;
none can be supplied through this narrow host-settings merge. Claude's own
project/managed settings precedence still applies above user settings.

On each successful active-source refresh, removed host overrides restore the
previous guest values, or remove the key if none existed before import. A guest
edit that differs from the last imported value is retained when the host override
is removed. Removing an explicit disable therefore can enable that extension
again, according to the restored guest preference or Claude's default.
An absent host `settings.json` is an empty preference set; malformed settings or
invalid relevant preference types stop bootstrap before any Claude invocation.

Coop retains a narrow `~/.claude/coop-import.json` snapshot for recovery, and
`_coopImportedPreferences` ownership metadata in guest `settings.json` for atomic
restoration of previous values. Imported preferences are applied before transferring staged extensions and before onboarding,
marketplace/plugin installation, or MCP registration, including after the existing
corrupt-settings fallback. The fallback still loses unrelated corrupt guest state,
but reapplies imported disables and style selection. An invalid import snapshot
stops bootstrap rather than treating disabled extensions as enabled.

Files follow an overlay lifecycle: restart overwrites files still present on the
host, but host deletions do not delete previous guest copies. `config_dir = false`
stops copying and retains both previous copies and the last preference snapshot.
A missing default source likewise retains previous imports; custom paths must
exist at config validation time. If a source disappears after validation, copying
is skipped and the snapshot retained. To remove retained content, remove it in the
guest or recreate the VM. Deletion synchronization and broader symlink/copier
hardening are outside this import contract.

### Environment variable forwarding

`env_forward` lists additional environment variable names to forward from the host to the guest via SSH `SendEnv`. These are forwarded on every SSH session, not just during bootstrap.

`ANTHROPIC_API_KEY` and `GITHUB_TOKEN` are handled through their own mechanisms (described above) and do not need to appear here.

```toml
[claude]
env_forward = ["MYORG_KEY", "OPENAI_API_KEY"]
```

Each variable must be set in the host environment at the time of the SSH session. Unset variables are silently skipped.

### Plugin marketplaces

`marketplaces` lists plugin marketplace sources. Each entry is either a remote URL (typically a GitHub repository) or an absolute path to a local directory.

```toml
[claude]
marketplaces = [
  "https://github.com/anthropics/claude-plugins-official",
  "/Users/me/dev/my-marketplace",
]
```

Remote URLs are passed directly to `claude plugin marketplace add --scope user` inside the guest.

Local directories are first copied into the guest at `~/.coop/marketplaces/<dirname>/` via SCP, then registered using the guest-side path. This is useful when developing a marketplace and testing plugins without publishing them to a remote source.

### Plugin installation

`plugins` lists plugins to install from the registered marketplaces. Each entry is passed to `claude plugin install <name> -s user` inside the guest.

```toml
[claude]
plugins = [
  "rust-analyzer-lsp@claude-plugins-official",
  "devcontainer-setup@trailofbits",
]
```

Plugins are installed after marketplaces are registered. If a plugin references a marketplace that hasn't been added, installation fails.

### MCP server registration

`mcp_servers` maps server names to their definitions. Each server is registered via `claude mcp add-json <name> <json> -s user` inside the guest.

Two server types are supported:

**stdio**: a local command that communicates over stdin/stdout:

```toml
[claude.mcp_servers.my-tool]
command = "/usr/local/bin/my-tool"
args = ["--verbose"]
```

**HTTP**: a remote server accessed by URL:

```toml
[claude.mcp_servers.sentry]
type = "http"
url = "https://mcp.sentry.dev/mcp"
```

Server definitions can include an `env` map for environment variable name mappings passed through to the MCP server configuration.

## Bootstrap sequence

When `coop up` creates/restarts a project VM or `coop start` restarts a stopped VM (without `--no-agents`), coop executes the following steps after the VM boots and SSH becomes available:

1. **GitHub auth**: If a `GITHUB_TOKEN` is available, run `gh auth setup-git` in the guest.
2. **User content preparation**: Stage the [allowlisted customizations](#config-directory) from `config_dir` and refresh their narrow companion-preference snapshot.
3. **Managed permissions**: Merge coop's managed permission keys (`permissions.defaultMode: bypassPermissions` and `permissions.skipDangerousModePermissionPrompt: true`) into the guest's `~/.claude/settings.json`, preserving unrelated keys and applying the imported companion preferences described above. The setting must live in user scope — Claude Code ignores `skipDangerousModePermissionPrompt` from project settings. Other keys Claude Code stores in this file (notably `enabledPlugins` and `extraKnownMarketplaces`) are preserved except for the explicitly imported plugin IDs, so unrelated plugin and marketplace state survives a stop/start cycle. Coop also owns the `env` block for local-model routing (see [Local model support](#local-model-support)): it is set when the VM is in local-model mode and removed in remote mode, so any hand-authored `env` entries in this file are not preserved. A file that cannot be parsed is replaced with managed defaults, then the imported companion preferences are reapplied before invoking Claude.
   After settings are written, transfer the staged content into guest `~/.claude/`, then seed onboarding when required.
4. **Marketplaces**: Register each marketplace source (local directories are copied to the guest first). On first boot, coop compares the configured marketplaces against those already baked into the golden image (from `coop setup --profile`) and only installs the ones that are missing.
5. **Plugins**: Install each plugin from the registered marketplaces. Like marketplaces, coop computes the delta against plugins already present in the golden image and skips those that are already installed.
6. **MCP servers**: Register each MCP server definition.

On restart (`coop start` of a stopped instance), coop refreshes GitHub auth (step 1), config directory contents (step 2), and the managed `~/.claude/settings.json` (step 3). Marketplaces, plugins, and MCP servers persist on the guest disk and are not re-installed.

### Skipping bootstrap

To create or restart a VM without any Claude Code configuration:

```bash
coop up . --no-agents
coop start --no-agents
```

This skips the entire bootstrap sequence. The VM boots normally but gets no API key, no GitHub token, no plugins, and no MCP servers. You can still run `coop claude` afterward, and that session forwards `ANTHROPIC_API_KEY` and any `env_forward` variables via SSH. Plugins and MCP servers won't be available unless you configure them manually inside the guest.

## Updating Claude Code

Claude Code auto-updates in the background by default — it checks for a newer version on startup and periodically, and applies the update on the next launch. coop does not disable this and the guest has outbound network access, so Claude Code keeps itself current with no action from you.

To force an update immediately rather than waiting for the background updater:

```bash
coop agent update --claude
```

This runs `claude update` synchronously inside the guest as the guest user. It is a convenience for when you want the newest version right now; for the recurring stale-agent problem, Codex is the one that needs attention (see [Updating Codex](codex-integration.md#updating-codex)). See [`agent update`](commands.md#agent-update).

## Local model support

A VM can route Claude Code at a host-side local model server (Ollama / LM Studio
/ vLLM / llama.cpp) instead of Anthropic's cloud. The endpoint must serve the
Anthropic Messages API. Switch a VM with [`coop model <vm> local`](commands.md#model)
and back with `coop model <vm> remote`; configure the endpoint under
[`[claude.local_model]`](configuration.md#local-model-routing) or interactively
at the `coop model … local` prompt.

The selection is per VM and independent of Codex — Claude can run on a local
model while Codex stays on cloud, or the reverse. The endpoint Claude resolves
is the `[claude.local_model]` config block if present, otherwise an endpoint
saved interactively for the instance, otherwise none (it stays on cloud).
Config takes precedence over the saved endpoint.

In local mode coop writes an `env` block into the managed
`~/.claude/settings.json` (see step 3 of the [bootstrap sequence](#bootstrap-sequence))
pointing `ANTHROPIC_BASE_URL` at the guest-visible endpoint, pinning every model
tier to the configured model, and supplying `ANTHROPIC_AUTH_TOKEN`. coop owns
this `env` block: it is set in local mode and removed in remote mode, so
hand-authored `env` entries are not preserved. Two cache-stability keys
(`CLAUDE_CODE_ATTRIBUTION_HEADER=0`, `CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS=1`)
are set in local mode. They stop Claude Code from mutating the system prompt
per request, which keeps a local inference server's prompt cache warm.

Switching takes effect without a VM restart: coop rewrites `settings.json` live
over SSH on a running VM (or saves the selection to apply on the next start). A
running `claude` reads its config at launch, so relaunch it (`coop claude <vm>`)
to pick up the change.
