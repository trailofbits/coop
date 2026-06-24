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

The agent view has no sign-in flow of its own. If you haven't signed Claude in yet with `coop claude` (and aren't forwarding an `ANTHROPIC_API_KEY`), run `/login` at the start of your `coop ca` session to authenticate before using the agent view.

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

`config_dir` specifies a host directory from which coop copies an allowlist of entries (`CLAUDE.md`, `rules/`, `commands/`) into `~/.claude/` in the guest. This provides Claude Code's global instructions and rules.

```toml
[claude]
config_dir = "~/.claude"
```

The default is `~/.claude`. Set to `false` to disable config file copying entirely.

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
2. **User content**: Copy the allowlisted entries (`CLAUDE.md`, `rules/`, `commands/`) from `config_dir` to `~/.claude/` in the guest.
3. **Managed permissions**: Write a coop-managed `~/.claude/settings.json` in the guest containing `permissions.defaultMode: bypassPermissions` and `permissions.skipDangerousModePermissionPrompt: true`. The setting must live in user scope — Claude Code ignores `skipDangerousModePermissionPrompt` from project settings. This file is overwritten on every VM startup; per-VM customization belongs in coop's config, not in the guest file.
4. **Marketplaces**: Register each marketplace source (local directories are copied to the guest first). On first boot, coop compares the configured marketplaces against those already baked into the golden image (from `coop setup --profile`) and only installs the ones that are missing.
5. **Plugins**: Install each plugin from the registered marketplaces. Like marketplaces, coop computes the delta against plugins already present in the golden image and skips those that are already installed.
6. **MCP servers**: Register each MCP server definition.

On restart (`coop start` of a stopped instance), only ephemeral state is refreshed: GitHub auth (step 1), config directory contents (step 2), and the managed `~/.claude/settings.json` (step 3). Marketplaces, plugins, and MCP servers persist on the guest disk and are not re-installed.

### Skipping bootstrap

To create or restart a VM without any Claude Code configuration:

```bash
coop up . --no-agents
coop start --no-agents
```

This skips the entire bootstrap sequence. The VM boots normally but gets no API key, no GitHub token, no plugins, and no MCP servers. You can still run `coop claude` afterward, and that session forwards `ANTHROPIC_API_KEY` and any `env_forward` variables via SSH. Plugins and MCP servers won't be available unless you configure them manually inside the guest.
