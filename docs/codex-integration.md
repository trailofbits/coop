# Codex Integration

coop installs Codex into every guest image and gives you a dedicated `coop codex` launcher. This guide covers the `coop codex` command, the configuration that controls what gets injected into the guest, and the bootstrap sequence that runs when a VM starts.

## Launching Codex

```bash
coop codex [instance-name] [-- extra-args...]
```

This SSHes into the guest and runs the `codex` CLI. By default coop passes `--dangerously-bypass-approvals-and-sandbox`, so Codex runs without its sandbox or approval prompts — parity with how `coop claude` runs unrestricted. The VM is the isolation boundary, so Codex's own sandbox is redundant; it also does not work in the guest, which lacks a functioning bubblewrap, so leaving it enabled makes every shell command Codex runs fail.

With `[codex] auth = "chatgpt"`, the guest wrapper (`codex-account`) verifies
and unlocks the guest user's shared GNOME Keyring service before launching
Codex. The systemd user bus and keyring survive closing the terminal; a VM
restart locks the keyring again. Nested launches reuse the unlocked service.

The wrapper supplies `-c 'cli_auth_credentials_store="keyring"'` to keep the
terminal app-server independent of the desktop app-server (#481). Both read
one guest credential store. Caller arguments retain their precedence,
including an explicitly selected remote app-server. API-key mode passes through.

Use `codex-account` or `codex-yolo` inside the guest for automatic readiness
checks. After unlock, ordinary SSH sessions can also run bare `codex` against
the standard user bus. The wrapper gates on the managed guest configuration;
coop updates that configuration when you change authentication modes and start
the VM again.

### Desktop over SSH

The desktop app connects to Codex in the guest over SSH. Terminal and desktop
sessions share the guest's encrypted GNOME Keyring, while each uses its own
Codex app-server. Unlock the keyring once per VM boot before connecting.

```bash
coop ssh-config my-project
ssh coop-my-project codex --version
coop codex-unlock my-project
```

Then enable the SSH host in the desktop app's Settings → Connections, select
`/workspace`, and complete Codex login inside the guest. Desktop sign-in, SSH
access, the keyring password and guest account login are separate steps. Close
the unlock terminal and reconnect to verify that credentials remain available.

On an existing VM, the first `codex-unlock` installs shared service support in
place. Stop and start that VM before running unlock again. This retires old
private keyring daemons and updaters together. Installation preserves the Codex
home and encrypted credentials. Multiple running keyring daemons must be
resolved before migration so their cached credential histories are not silently
selected at reboot. Conflicting keyring files, unsupported formats
and plaintext stores require explicit migration; coop never selects a history
or replaces them automatically. Back up the files inside the guest before
resolving conflicts or reauthenticating.

`codex-unlock` also upgrades guests missing the SSH session support required
by older Firecracker images, then requests the same stop/start cycle. See
[Firecracker SSH user sessions](platform-notes.md#firecracker-ssh-user-sessions)
for the PAM and persistent-linger details.

After a keyring crash or locked-to-unlocked transition, rerun `codex-unlock`
and reconnect the desktop. Recovery retires the desktop server through Codex's
native `daemon stop`; the desktop owns its next startup and updater. Closing or
locking the keyring cannot erase credentials already cached in running clients.
Concurrent OAuth refresh and logout across clients require real-account testing.
Desktop SSH does not use coop's API-key/proxy secret forwarding.

#### Desktop execution permissions

Select **Full access** in the desktop thread's permissions control when using
the VM as the isolation boundary. An explicit desktop selection overrides
guest defaults, including when continuing an existing thread. Auto mode uses
the Linux workspace sandbox and may fail to initialize on guests without
working bubblewrap/user-namespace support. Changing authentication or unlocking
the keyring does not change a thread's permissions.

Image provisioning and agent bootstrap install `/etc/codex/config.toml` when
that file does not already exist, in both authentication modes:

```toml
approval_policy = "never"
default_permissions = ":danger-full-access"
```

These are system defaults for Codex 0.154.0, below user/project configuration
and explicit thread selections. Existing system configuration is preserved.
After upgrading an existing VM, restart it with agent bootstrap enabled to
install the defaults, then reconnect the desktop. `--no-agents` skips this
installation on existing images. The defaults do not disable separate app/MCP
approval policies or organization requirements.

To restore workspace sandboxing and on-request approval prompts for a single
session, pass `--ask`. coop explicitly overrides the unrestricted guest defaults:

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
requested again after a VM restart or keyring lock; it is unrelated to your ChatGPT or host
credentials. Because it is per-guest, `coop destroy` discards it along with the
cached login.

Unlocking needs a terminal, supplied by `coop codex` and `coop codex-unlock`.
Noninteractive launches work after successful unlock in the current service
generation; otherwise they fail with guidance to run an interactive unlock.

Security and billing guardrails in this mode:

- `OPENAI_API_KEY` is not forwarded, even if it is configured, present in the
  host environment, listed in `env_forward`, or persisted from `coop start
  --env`.
- `auth.json` from the host Codex config directory is not copied into the
  guest. Account tokens are stored in the guest OS credential store instead.
- `CODEX_HOME` cannot redirect Codex around keyring storage in this mode. When
  coop's managed config selects the keyring, the guest wrapper refuses any
  explicitly set `CODEX_HOME`, preventing Codex from writing account
  credentials to an unmanaged `auth.json`; unset `CODEX_HOME` when using
  ChatGPT account auth.
- `[proxy.openai]` is rejected with `auth = "chatgpt"`, because the proxy path
  uses an OpenAI API key and would switch Codex back to API billing.

Because coop must keep `cli_auth_credentials_store` in the guest
`~/.codex/config.toml`, this mode rewrites that file on every start. Codex's
own state in it — installed marketplaces and plugins, and the
`[projects.*]` workspace-trust records — is read back and preserved across the
rewrite, so you are not re-approving workspace trust after each restart.

For existing guests, use `coop codex-unlock <vm>` to install shared keyring
support in place, then stop and start the VM. To include support in future
VMs, rebuild the golden image with `coop setup --rebuild`. Rebuilding an image
does not change an existing guest disk.

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

**NOTE**: MCP server commands must be installed in the guest. For example, to make `npx` available when creating a new instance, use `coop up --profile node`. If your image already includes the required tools, no additional profile flag is needed. Profiles do not add tools to an existing instance; see [Images and Profiles](images-and-profiles.md) for image setup options.

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

`coop setup` uses [OpenAI's native installer](https://developers.openai.com/codex/cli/)
to install the full Codex package, including bundled tools, as the configured
guest user. The installer manages its package under the user's home directory
and exposes `~/.local/bin/codex`. coop retains `/usr/local/bin/codex` as a
compatibility link for existing wrappers and scripts.

To update directly inside the VM, run `codex update` as the guest user; sudo
is not required. To update from the host:

```bash
coop agent update --codex          # update Codex to the latest release
coop agent update --check          # report installed vs. latest, change nothing
```

`coop agent update --codex` re-runs the native installer as the guest user and
refreshes the compatibility link. It also migrates older direct-binary
installations without rebuilding the VM or replacing the user's Codex config.
A profile-provided `/usr/local/bin/codex` is preserved during image setup;
an explicit update replaces it with the native installation.

Updates affect that VM. To refresh the golden image for new VMs, run
`coop setup --rebuild`. See [`agent update`](commands.md#agent-update).

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
