# Codex Integration

coop installs Codex into every guest image and gives you a dedicated `coop codex` launcher. This guide covers the `coop codex` command, the configuration that controls what gets injected into the guest, and the bootstrap sequence that runs when a VM starts.

## Launching Codex

```bash
coop codex [instance-name] [-- extra-args...]
```

This SSHes into the guest and runs the `codex` CLI. By default coop passes `--dangerously-bypass-approvals-and-sandbox`, so Codex runs without its sandbox or approval prompts — parity with how `coop claude` runs unrestricted. The VM is the isolation boundary, so Codex's own sandbox is redundant; it also does not work in the guest, which lacks a functioning bubblewrap, so leaving it enabled makes every shell command Codex runs fail.

When `[codex] auth = "chatgpt"` is enabled, `coop codex` launches a small
guest wrapper (`/usr/local/bin/codex-account`) that starts a D-Bus session,
unlocks GNOME Keyring, and then runs the real Codex binary. In keyring mode
each launch gets a fresh private D-Bus session, so the wrapper asks for the
guest keyring password every time before Codex starts.

The in-guest `codex-yolo` shortcut routes through the same wrapper, so it works
in either auth mode. Running the bare `codex` binary from `coop shell` does
not: it has no D-Bus session, and `keyring` credential storage has no
`auth.json` fallback, so Codex will not find its credentials. Inside the guest,
run `codex-account` (or `codex-yolo`) instead of `codex`. The wrapper is a
transparent passthrough unless the guest `~/.codex/config.toml` asks for
keyring storage, so it is safe to use in either mode. It gates on the guest
file rather than on coop's `auth` setting, which is what keeps `codex-yolo`
working from inside the guest; coop keeps that file in step when you switch
modes, rewriting it on the next `coop start` to drop the keyring setting.

To keep Codex's sandbox and approval prompts for a single session, pass `--ask`. coop then launches `codex` with no bypass flag, so Codex applies its normal defaults:

```bash
coop codex --ask
```

Use `--ask` too if you want to supply your own sandbox or approval flags (`--sandbox`, `-a`) as trailing arguments — otherwise coop's bypass flag takes precedence.

Trailing arguments go straight through to the `codex` CLI:

```bash
coop codex -- --model gpt-5
```

## Configuration

Codex-related settings live under the `[codex]` section in `config.toml`, except `github` which is a top-level field:

```toml
github = "auto"

[codex]
auth = "api_key"
api_key = "sk-proj-..."
env_forward = ["MYORG_KEY"]
config_dir = "~/.codex"

[codex.mcp_servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest"]
```

Every field is optional. An empty `[codex]` section (or omitting it entirely) keeps the historical API-key mode and skips all Codex-specific bootstrap steps unless host config, MCP servers, plugins, or local-model routing need to be applied.

### API key forwarding

The default auth mode is `auth = "api_key"`. In this mode, coop forwards
`OPENAI_API_KEY` to the guest via SSH `SendEnv` on every session: `coop codex`,
`coop shell`, and `coop exec` alike. The key is never written to disk inside
the guest.

Resolution order:

1. `codex.api_key` in `config.toml`
2. `OPENAI_API_KEY` environment variable on the host

If neither is set, the guest starts without an API key. You can authenticate interactively the first time you run `codex` inside the VM.

### ChatGPT account auth

Set `auth = "chatgpt"` to use a ChatGPT account or ChatGPT Business workspace
with Codex instead of an OpenAI API key:

```toml
[codex]
auth = "chatgpt"
```

This mode follows Codex's ChatGPT sign-in path, so usage is tied to the
selected ChatGPT workspace rather than standard API billing. coop writes
`cli_auth_credentials_store = "keyring"` into the guest `~/.codex/config.toml`
so Codex uses Linux Secret Service storage, and it launches Codex through
`/usr/local/bin/codex-account` so a headless guest has a D-Bus session and an
unlocked GNOME Keyring. See OpenAI's
[authentication documentation](https://learn.chatgpt.com/docs/auth#credential-storage)
for the Codex credential-store setting and device-code login flow.

The first login should use device-code auth from inside the guest:

```bash
coop codex -- login --device-auth
```

Then open the shown URL in your browser, sign in to the intended ChatGPT
workspace, and enter the one-time code. Later `coop codex` launches reuse the
cached account credentials from the guest keyring. (`coop codex` launches
`login` and `logout` without the sandbox-bypass flag — they never start an
agent session, so there is nothing to sandbox — and no `--ask` is needed.)

#### The guest keyring password

A fresh VM has no keyring, so the first prompt is *choosing* a password, not
entering one. The wrapper says so and asks for confirmation. That password
encrypts the Codex account credentials at rest inside the guest and is
requested again on later launches; it is unrelated to your ChatGPT or host
credentials. Because it is per-guest, `coop destroy` discards it along with the
cached login.

The prompt needs a terminal. `coop codex` provides one; a non-interactive
`coop exec` does not, and the wrapper fails with a clear message rather than
hanging.

Security and billing guardrails in this mode:

- `OPENAI_API_KEY` is not forwarded, even if it is configured, present in the
  host environment, listed in `env_forward`, or persisted from `coop start
  --env`.
- `auth.json` from the host Codex config directory is not copied into the
  guest. Account tokens are stored in the guest OS credential store instead.
- `[proxy.openai]` is rejected with `auth = "chatgpt"`, because the proxy path
  uses an OpenAI API key and would switch Codex back to API billing.

Images built before this support existed need a rebuild:

```bash
coop setup --rebuild
```

A rebuild only changes the golden image. An existing VM keeps its own guest
disk across `coop stop` / `coop start`, so it will not pick up the new guest
packages. Swap the rebuilt image in without losing the instance:

```bash
coop stop my-project
coop restore my-project --image default
coop start my-project
```

[`restore`](commands.md#restore) keeps the instance's name, index, IP, and
workspace association. Destroying and recreating the VM also works, but
discards its guest disk.

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

When `auth = "chatgpt"` or `[proxy.openai]` is active, `auth.json` is excluded
from the copy. In ChatGPT account mode, coop stores cached account credentials
through the guest keyring instead.

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

If `config_dir` also provides a `config.toml`, coop preserves its other settings but replaces the `mcp_servers` table with the one derived from `codex.mcp_servers`. When the VM is in [local-model mode](#local-model-support), coop also owns the `model` and `model_provider` keys and a `[model_providers.coop_local]` block; these are written on a switch to local and removed on a switch back to remote, so they are not preserved across a mode change.

### Plugin marketplaces

`marketplaces` and `plugins` declare Codex [plugin marketplaces](https://learn.chatgpt.com/docs/plugins) and the plugins to install from them, mirroring the same fields under `[claude]`:

```toml
[codex]
marketplaces = ["trailofbits/codex-plugins"]  # owner/repo, owner/repo@ref, git URL, or local path
plugins = ["my-lsp@codex-plugins"]             # plugin@marketplace
```

Each marketplace source is registered with `codex plugin marketplace add` and each plugin installed with `codex plugin add`. A source that is an absolute local directory is copied into the guest first; a `owner/repo`, `owner/repo@ref`, or git URL is passed through unchanged.

These are **baked into the golden image** during `coop setup` (on the Lima/macOS backend) and recorded in the image's template config. On a VM's first boot coop installs only the delta not already baked in; on the Firecracker/Linux backend, where nothing is baked, the full set installs on first boot. Like Claude plugins, they are installed on **first boot only** — they persist on the guest disk across stop/start.

Codex stores marketplace registrations under `[marketplaces.*]` and per-plugin enabled/disabled state under `[plugins.*]` in `~/.codex/config.toml`. Because coop rewrites that file on every boot, it reads the guest's current tables back first and preserves them across the rewrite (dropping any that came from the host's own `config.toml`), so installed plugins — and any manual enable/disable toggles you make with `/plugins` — survive a restart.

## Bootstrap sequence

When `coop up` creates/restarts a project VM or `coop start` restarts a stopped VM (without `--no-agents`), coop executes the following steps after the VM boots and SSH becomes available:

1. **GitHub auth**: If a `GITHUB_TOKEN` is available, run `gh auth setup-git` in the guest.
2. **User content**: Copy the allowlisted Codex entries (`AGENTS.md`,
   `prompts/`, `config.toml`, `auth.json`) from `config_dir` to `~/.codex/` in
   the guest, preserving the guest's installed `[marketplaces.*]`/`[plugins.*]`
   tables. `auth.json` is omitted when ChatGPT account auth or proxy mode is
   active.
3. **Auth storage**: In ChatGPT account mode, write
   `cli_auth_credentials_store = "keyring"` into `~/.codex/config.toml`.
4. **MCP servers**: Merge configured MCP server definitions into `~/.codex/config.toml`.
5. **Marketplaces & plugins** (first boot only): Install the configured `marketplaces`/`plugins` not already baked into the golden image.

On restart (`coop start` of a stopped instance), the same Codex config files are refreshed so host-side updates are reflected in the guest; marketplaces and plugins are not reinstalled, but the guest's installed plugin state is preserved.

### Skipping bootstrap

To create or restart a VM without any Claude Code or Codex configuration:

```bash
coop up . --no-agents
coop start --no-agents
```

This skips the guest bootstrap sequence entirely. The VM still includes both CLIs because they are baked into the image during `coop setup`.

## Updating Codex

Codex is installed "latest at build time" during `coop setup` and has no background updater, so it stays at that version until the image is rebuilt. Unlike Claude Code, it does not refresh itself. To update Codex in a running VM without rebuilding the image:

```bash
coop agent update --codex          # update Codex to the latest release
coop agent update --check          # report installed vs. latest, change nothing
```

This re-runs coop's own Codex installer inside the guest as root, overwriting `/usr/local/bin/codex` with the current release. To refresh the golden image so new VMs ship the latest Codex, rebuild it with `coop setup --rebuild`. See [`agent update`](commands.md#agent-update).

## Local model support

A VM can route Codex at a host-side local model server (Ollama / LM Studio /
vLLM / llama.cpp) instead of OpenAI's cloud. The endpoint must serve the
Responses API — the only wire API Codex currently supports. Switch a VM with
[`coop model <vm> local`](commands.md#model) and back with
`coop model <vm> remote`; configure the endpoint under
[`[codex.local_model]`](configuration.md#local-model-routing) or interactively
at the `coop model … local` prompt.

The selection is per VM and independent of Claude — Codex can run on a local
model while Claude stays on cloud, or the reverse. The endpoint Codex resolves
is the `[codex.local_model]` config block if present, otherwise an endpoint
saved interactively for the instance, otherwise none (it stays on cloud).
Config takes precedence over the saved endpoint.

In local mode coop injects three coop-owned keys into `~/.codex/config.toml`:
`model` (the configured model), `model_provider` (`coop_local`), and a
`[model_providers.coop_local]` block pointing `base_url` at the guest-visible
endpoint with `wire_api = "responses"`. The provider reads its API key from the
`COOP_LOCAL_API_KEY` env var, which coop forwards with the configured (or dummy)
token. These keys are coop-owned: they are written on a switch to local and
removed on a switch back to remote, so they are not preserved across a mode
change.

Switching takes effect without a VM restart: coop rewrites `config.toml` live
over SSH on a running VM (or saves the selection to apply on the next start). A
running `codex` reads its config at launch, so relaunch it (`coop codex <vm>`)
to pick up the change.
