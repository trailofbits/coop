# Codex Integration

coop installs Codex into every guest image and gives you a dedicated `coop codex` launcher. This guide covers the `coop codex` command, the configuration that controls what gets injected into the guest, and the bootstrap sequence that runs at `coop start`.

## Launching Codex

```bash
coop codex [instance-name] [-- extra-args...]
```

This SSHes into the guest and runs the `codex` CLI.

Trailing arguments go straight through to the `codex` CLI:

```bash
coop codex -- --model gpt-5
```

### tmux session persistence

`coop codex` runs inside a tmux session named `codex` by default. If the SSH connection drops, the Codex process survives in the guest. Running `coop codex` again reattaches to the existing session rather than starting a new one.

To bypass tmux and get a raw SSH session:

```bash
coop codex --no-tmux
```

## Configuration

Codex-related settings live under the `[codex]` section in `config.toml`, except `github` which is a top-level field:

```toml
github = "auto"

[codex]
api_key = "sk-proj-..."
env_forward = ["MYORG_KEY"]
config_dir = "~/.codex"

[codex.mcp_servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

Every field is optional. An empty `[codex]` section (or omitting it entirely) skips all Codex-specific bootstrap steps.

### API key forwarding

coop forwards `OPENAI_API_KEY` to the guest via SSH `SendEnv` on every session: `coop codex`, `coop shell`, and `coop exec` alike. The key is never written to disk inside the guest.

Resolution order:

1. `codex.api_key` in `config.toml`
2. `OPENAI_API_KEY` environment variable on the host

If neither is set, the guest starts without an API key. You can authenticate interactively the first time you run `codex` inside the VM.

### GitHub auth

The `github` field controls how coop obtains a `GITHUB_TOKEN` for the guest. This token enables private repo cloning and `gh` CLI usage inside the VM.

| Value    | Behavior |
|----------|----------|
| `"auto"` | Check the `GITHUB_TOKEN` env var first. If unset, run `gh auth token` on the host to extract a token from the GitHub CLI. |
| `"env"`  | Require `GITHUB_TOKEN` in the host environment. Warns if missing. |
| `"off"`  | Skip GitHub token forwarding entirely. This is the default when `github` is unset. |

When a token is available, coop runs `gh auth setup-git` in the guest during bootstrap.

### Config directory

`config_dir` specifies a host directory from which coop copies an allowlist of entries (`AGENTS.md`, `prompts/`, `config.toml`, `auth.json`) into `~/.codex/` in the guest. This provides Codex's global instructions, prompt files, baseline user configuration, and local Codex authentication state.

```toml
[codex]
config_dir = "~/.codex"
```

The default is `~/.codex`. Set to `false` to disable config file copying entirely.

### Environment variable forwarding

`env_forward` lists additional environment variable names to forward from the host to the guest via SSH `SendEnv`. These are forwarded on every SSH session, not just during bootstrap.

`OPENAI_API_KEY` and `GITHUB_TOKEN` are handled through their own mechanisms and do not need to appear here.

### MCP server registration

`mcp_servers` maps server names to their definitions. coop merges these definitions into the guest `~/.codex/config.toml` under `mcp_servers`.

Definitions use the same schema as Claude integration:

```toml
[codex.mcp_servers.my-tool]
command = "npx"
args = ["-y", "@example/mcp-server"]
```

```toml
[codex.mcp_servers.sentry]
type = "http"
url = "https://mcp.sentry.dev/mcp"
```

If `config_dir` also provides a `config.toml`, coop preserves its other settings but replaces the `mcp_servers` table with the one derived from `codex.mcp_servers`.

## Bootstrap sequence

When `coop start` runs (without `--no-agents`), it executes the following steps after the VM boots and SSH becomes available:

1. **GitHub auth**: If a `GITHUB_TOKEN` is available, run `gh auth setup-git` in the guest.
2. **User content**: Copy the allowlisted Codex entries (`AGENTS.md`, `prompts/`, `config.toml`, `auth.json`) from `config_dir` to `~/.codex/` in the guest.
3. **MCP servers**: Merge configured MCP server definitions into `~/.codex/config.toml`.

On restart (`coop start` of a stopped instance), the same Codex config files are refreshed so host-side updates are reflected in the guest.

### Skipping bootstrap

To start a VM without any Claude Code or Codex configuration:

```bash
coop start --no-agents
```

This skips the guest bootstrap sequence entirely. The VM still includes both CLIs because they are baked into the image during `coop setup`.
