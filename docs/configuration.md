# Configuration Reference

coop reads configuration from `~/.coop/config.toml` by default. Pass `--config <path>` to use a different file. Files with a `.json` extension are parsed as JSON for backward compatibility.

If the file does not exist, coop falls back to built-in defaults. A valid minimal config is an empty file.

Run `coop validate` to surface errors and warnings before anything touches a VM.

## Top-level fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `data_dir` | string (path) | `~/.coop` | Directory for VM artifacts: images, instances, keys, kernel, Firecracker binary. |
| `ssh_port` | integer | `22` | SSH port on the guest VM. Must be > 0. |
| `firecracker_bin` | string (path) | `~/.coop/firecracker` | Path to the Firecracker binary. Linux only; ignored on macOS (Lima backend). |
| `github` | string or table | unset (treated as `"off"`) | GitHub authentication strategy. See [GitHub auth](#github-auth). |
| `post_start` | string | unset | Shell command run in the guest after every successful boot, before any interactive `shell` / agent launch. Failure is logged at `WARN` and does not fail startup. Override per invocation with `coop up --post-start <cmd>` or `coop start --post-start <cmd>`. |

## GitHub auth

The `github` field determines how coop obtains a `GITHUB_TOKEN` for the guest:

| Value | Behavior |
|-------|----------|
| `"auto"` | Checks `$GITHUB_TOKEN` first. Falls back to `gh auth token` if unset. |
| `"env"` | Reads `$GITHUB_TOKEN` from the environment only. Warns if unset. |
| `"off"` | No GitHub token forwarding. |
| `"pat"` | Uses a per-repo fine-grained PAT recorded under `[github.pat]`. Scope is **server-enforced** to one repository. |

When a token is present, coop runs `gh auth setup-git` inside the guest to wire up git credential helpers.

### Fine-grained PAT (`github = "pat"`)

In pat mode coop forwards a *per-repo* fine-grained personal access token: the resolved `owner/repo` at VM startup selects the matching entry in `[github.pat]`. Compared with `"auto"` / `"env"`, the effective reach of a leaked token is bounded by the repos and permissions GitHub recorded when it was created — GitHub rejects out-of-scope operations (REST and GraphQL) server-side, not in coop.

Configure via the wizard:

```sh
coop github setup-pat --repo trailofbits/coop
```

The wizard opens the PAT-creation form in your browser, validates the token via `/user` and `/repos/<repo>`, stores the token in a secret manager you choose (macOS Keychain, Linux Secret Service, 1Password, or a `0600` file under `~/.coop/state/github-pat/`), and writes a `[github.pat."owner/repo"]` entry. The token itself is stored only in the chosen secret manager — the config file holds a `cmd:` invocation that retrieves it.

Multi-repo example:

```toml
[github]
mode = "pat"

[github.pat."trailofbits/coop"]
token = "cmd:security find-generic-password -s coop-github-pat -a trailofbits-coop -w"

[github.pat."trailofbits/coop-plugins"]
token = "cmd:security find-generic-password -s coop-github-pat -a trailofbits-coop-plugins -w"
```

Bring-your-own-token (no wizard, useful for CI/Terraform). Any `cmd:` invocation that prints the token on stdout works. Examples:

```toml
[github]
mode = "pat"

# Vault
[github.pat."trailofbits/coop"]
token = "cmd:vault read -field=token secret/coop/github/trailofbits-coop"

# 1Password (matches what the wizard emits)
[github.pat."trailofbits/coop-plugins"]
token = "cmd:op item get 'coop-github-pat (trailofbits-coop-plugins)' --fields password --reveal"
```

Other subcommands:

| Command | Effect |
|---------|--------|
| `coop github status` | List configured entries, storage backend, and whether each token still resolves. Never prints token material. |
| `coop github rotate-pat --repo X/Y` | Re-run the wizard against an existing entry (PATs expire — max 1 year). |
| `coop github forget-pat --repo X/Y` | Remove the stored secret and the `[github.pat."X/Y"]` entry. Does **not** add a skip marker; the token may still be live on GitHub. |
| `coop validate --probe` | Resolves each entry and probes `GET /user` against api.github.com. May trigger Keychain authorization or a 1Password Touch-ID prompt the first time per session. |

#### Auto-prompt at VM startup

When `coop up` or `coop start` runs with a resolvable repo (usually the synced workspace's `origin`) and `github` is `"off"` (or `"pat"` with no matching entry), coop offers to run the wizard inline: `[y/N/never]`. Three answers:

- `y` — run the wizard, then continue the start.
- `N` (default) — start unauthenticated, ask again next time.
- `never` — record a skip marker under `[github.skip]` so coop won't ask again for this repo.

Non-interactive contexts (`CI` is set, stdin is not a TTY) skip the prompt and log a one-line tip pointing at `coop github setup-pat`. The `--no-prompt` flag skips the prompt silently. Set `[setup] prompt_for_pat = false` in `config.toml` to disable the prompt globally.

#### Skip markers

```toml
[github]
mode = "pat"
skip = ["trailofbits/big-repo"]
```

`coop github setup-pat --repo X/Y` removes any skip marker for `X/Y` when it adds a new entry.

## `vm` section

VM resource allocation and boot configuration.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `vcpu_count` | integer | `2` | Number of vCPUs. Must be > 0. Overridable with `--vcpus` on `setup` and `up`. |
| `mem_size_mib` | integer | `4096` | Memory in MiB. Must be >= 128. Overridable with `--mem` on `setup` and `up`. |
| `template_size_gib` | integer | `8` | Template rootfs disk size in GiB. Must be > 0. Overridable with `--template-size` on `setup`. |
| `kernel_path` | string (path) | `~/.coop/vmlinux` | Path to the vmlinux kernel image. Linux/Firecracker only. |
| `boot_args` | string | `console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw` | Kernel boot arguments. Linux/Firecracker only. |

## `network` section

Firecracker TAP networking. These fields apply to Linux only. The Lima backend on macOS manages networking independently.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host_ip` | string (IPv4) | `172.16.0.1` | Host-side IP address on TAP interfaces. |
| `subnet_mask` | string (CIDR) | `/24` | Subnet mask in CIDR notation. Must be `/0` through `/32`. |
| `host_iface` | string | `auto` | Host network interface for NAT (e.g., `eth0`, `ens5`). `auto` detects it at runtime. |

## Guest user

The guest VM runs as an unprivileged account, `ubuntu` (uid 1000) by default. Override the username at setup time with `coop setup --guest-user <name>`:

```sh
coop setup --guest-user vscode
```

The name is validated against the POSIX-portable pattern `[a-z_][a-z0-9_-]{0,31}`; `root` is rejected because coop assumes an unprivileged uid-1000 account.

The guest user is **baked into the image at setup time and immutable for the image's lifetime** — it is persisted in the image's `template_config.json`, and `up` / `start` / `shell` / `exec` read it back from there. To change it, destroy and recreate the image: `coop destroy && coop setup --guest-user <name>`.

### devcontainer `remoteUser`

When a workspace's `devcontainer.json` declares a `remoteUser` (e.g. `vscode` for the Microsoft devcontainer base images), the handling depends on the stage:

- **At `coop setup`**, a valid `remoteUser` becomes the image's guest user unless `--guest-user` already pins one (the CLI flag wins, and the override is reported).
- **At `coop up` / `coop start`**, the guest user is already baked in. If the file's `remoteUser` matches the image's persisted user, it is applied; if it differs, coop reports the mismatch, skips forwarding `containerEnv` (its values often reference a `/home/<remoteUser>/...` path that doesn't exist on disk), and points you at `coop destroy && coop setup --guest-user <remoteUser>` to switch.

See [Devcontainer support](devcontainer.md) for the full translation table.

## Guest PATH

Every guest SSH session — login, non-login, and `exec` — has the guest user's `~/.local/bin` on `PATH`. coop prepends it to `PATH` in `/etc/environment`, which `pam_env` applies to all sessions. This is where the Claude Code installer places its per-user `claude` binary.

## `guest_env` section

Literal environment variables to set inside the guest, independent of the host process environment. Use this when you want a value that isn't (or shouldn't be) on the host — `env_forward` covers the inherit-from-host case.

```toml
[guest_env]
RUST_LOG = "info"
MY_FLAG = "1"
```

Keys are env var names; values are the literals to inject. Entries here **override** any value resolved through other mechanisms for the same name (forwarded host env, `claude.api_key`, etc.), and the override is logged at `WARN`.

**Secrets:** values land in the guest's process environment in plain text and may be visible via `ps`/`/proc` to guest users. For credentials, prefer `env_forward` (host process env stays the source of truth) or one of the `cmd:` integrations on the structured fields (`claude.api_key`, etc.).

Override or extend per-invocation with `coop up --env KEY=VALUE` or `coop start --env KEY=VALUE` (repeatable).

## `claude` section

Claude Code configuration injected into the guest VM at start time. Every field is optional.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api_key` | string | unset (reads `$ANTHROPIC_API_KEY` from environment) | Anthropic API key. Forwarded to the guest via SSH `SendEnv`. Never written to disk inside the VM. |
| `config_dir` | string (path) or `false` | `~/.claude` | Source directory for Claude config files. Copies an allowlist of entries (`CLAUDE.md`, `rules/`, `commands/`) from this directory to `~/.claude/` in the guest on start. Set to `false` to disable. Supports `~` expansion. |
| `env_forward` | array of strings | `[]` | Extra environment variable names to forward from host to guest via SSH `SendEnv`. `ANTHROPIC_API_KEY` and `GITHUB_TOKEN` are forwarded automatically when set; list additional variables here. |
| `marketplaces` | array of strings | `[]` | Plugin marketplace sources. Each entry is a GitHub repo URL or an absolute local directory path. Local directories are copied into the guest before registration. |
| `plugins` | array of strings | `[]` | Plugins to install from registered marketplaces. Format: `plugin-name@marketplace-name`. |
| `mcp_servers` | table | `{}` | MCP servers to register in the guest. Keys are server names; values are server definitions. See [MCP servers](#mcp-servers). |

### MCP servers

Each key in `mcp_servers` maps a server name to its definition. Two transport types are supported.

**Stdio server** (spawns a process):

```toml
[claude.mcp_servers.my-server]
command = "/usr/bin/my-mcp-server"
args = ["--flag", "value"]
env = { SERVER_API_KEY = "MY_HOST_ENV_VAR" }
```

**HTTP server** (connects to a remote endpoint):

```toml
[claude.mcp_servers.remote-server]
type = "http"
url = "https://mcp.example.com/v1"
headers = { Authorization = "Bearer token" }
```

Definition fields:

| Field | Type | Description |
|-------|------|-------------|
| `command` | string | Command to run (stdio servers). |
| `args` | array of strings | Arguments for the command (stdio servers). Default: `[]`. |
| `type` | string | Server type: `"http"` or `"sse"` (HTTP servers). Omit for stdio. |
| `url` | string | Server URL (HTTP servers). |
| `env` | table | Environment variable mappings. Keys are the names the server expects; values are the host env var names to read. Default: `{}`. |
| `headers` | table | HTTP headers to send (HTTP servers). Default: `{}`. |

Servers are registered with `claude mcp add-json` at user scope.

## `codex` section

Codex configuration injected into the guest VM at start time. Every field is optional.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api_key` | string | unset (reads `$OPENAI_API_KEY` from environment) | OpenAI API key. Forwarded to the guest via SSH `SendEnv`. Never written to disk inside the VM. |
| `config_dir` | string (path) or `false` | `~/.codex` | Source directory for Codex config files. Copies an allowlist of entries (`AGENTS.md`, `prompts/`, `config.toml`, `auth.json`) from this directory to `~/.codex/` in the guest on start. Set to `false` to disable. Supports `~` expansion. |
| `env_forward` | array of strings | `[]` | Extra environment variable names to forward from host to guest via SSH `SendEnv`. `OPENAI_API_KEY` and `GITHUB_TOKEN` are forwarded automatically when set; list additional variables here. |
| `mcp_servers` | table | `{}` | MCP servers to merge into the guest `~/.codex/config.toml`. Keys are server names; values are server definitions. See [MCP servers](#mcp-servers). |

coop preserves any other settings already present in the staged `config.toml`, but the `mcp_servers` table is owned by coop when `codex.mcp_servers` is configured.

## `profiles` section

Custom installation profiles for `coop setup --profile <name>`. Each profile declares packages and scripts that run during rootfs template creation.

```toml
[profiles.my-tools]
apt_packages = ["ripgrep", "fd-find", "jq"]
pre_install = "curl -fsSL https://example.com/setup.sh | bash"
post_install = "echo 'done'"
marketplaces = ["https://github.com/anthropics/claude-plugins-official"]
plugins = ["rust-analyzer-lsp@claude-plugins-official"]
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `apt_packages` | array of strings | `[]` | Apt packages to install in the guest template. |
| `pre_install` | string | unset | Shell commands to run before `apt-get install` (e.g., adding PPAs or GPG keys). |
| `post_install` | string | unset | Shell commands to run after `apt-get install`. |
| `marketplaces` | array of strings | `[]` | Plugin marketplace sources for this profile. Same format as `claude.marketplaces`. |
| `plugins` | array of strings | `[]` | Plugins to install for this profile. Same format as `claude.plugins`. |

Custom profiles compose with built-in ones (`python`, `node`, `c`, `fuzz`, `rust`, `go`). Combine them with commas: `coop setup --profile python,node,my-tools`.

## `forward_ports` field

Default host-to-guest TCP port forwards applied to every VM startup. Forwards are established as SSH `-L` tunnels after the VM is ready and torn down on `coop stop`.

Each entry accepts a bare port (host and guest match), a `"GUEST:HOST"` string, or a table.

```toml
# Bare port: host 3000 ⇒ guest 3000
forward_ports = [3000]

# String form: host 18080 ⇒ guest 8080
forward_ports = ["8080:18080"]

# Table form (also supports an optional `label` for the user's own bookkeeping)
[[forward_ports]]
guest = 5432
host = 15432
label = "postgres"
```

`--forward-port` on `coop up` or `coop start` appends to (or overrides on guest-port collision) the entries from config; later entries win. Each instance remembers its forward set across `coop stop` / `coop start`, so a restart without `--forward-port` re-establishes the same tunnels.

Collision with an in-use host port fails fast before the VM is created. The error names the offending port and suggests a `GUEST:HOST` override.

## `updates` section

Background update-check behavior for `coop update`. Defaults are safe; most users do not need to set anything here.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mode` | `"notify"` or `"off"` | `"notify"` | `"notify"` runs a background check at most once per `check_interval_hours` and prints a one-line stderr notice when a newer release is known. `"off"` disables both the check and the notice. |
| `check_interval_hours` | integer | `24` | Minimum hours between background release-metadata fetches. |

The background check is also silent when `COOP_NO_UPDATE_CHECK=1`, when `CI=true`, or when stdin is not a TTY. Dev builds (untagged or dirty trees) never run the check or notice.

```toml
[updates]
mode = "off"
```

## CLI overrides

Several config values accept per-invocation overrides via flags:

| Flag | Applies to | Overrides |
|------|-----------|-----------|
| `--vcpus <N>` | `setup`, `up` | `vm.vcpu_count` |
| `--mem <MiB>` | `setup`, `up` | `vm.mem_size_mib` |
| `--template-size <GiB>` | `setup` | `vm.template_size_gib` |
| `--disk <GiB>` | `up` | Per-instance disk size (grows from template if larger) |
| `--env KEY=VALUE` | `up`, `start` | Adds or overrides a `guest_env` entry (repeatable) |
| `--config <path>` | all commands | Config file path (default: `~/.coop/config.toml`) |

## Examples

### Minimal config

An empty file gives you all defaults (2 vCPUs, 4 GiB RAM, 8 GiB disk).

### Full config

```toml
data_dir = "~/.coop"
ssh_port = 22
firecracker_bin = "~/.coop/firecracker"
github = "auto"  # must be set explicitly; default is off

[vm]
vcpu_count = 4
mem_size_mib = 8192
template_size_gib = 20
kernel_path = "~/.coop/vmlinux"
boot_args = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw"

[network]
host_ip = "172.16.0.1"
subnet_mask = "/24"
host_iface = "auto"

[guest_env]
RUST_LOG = "info"

[claude]
config_dir = "~/.claude"
env_forward = ["CUSTOM_TOKEN"]
marketplaces = [
  "https://github.com/anthropics/claude-plugins-official",
  "/home/user/local-marketplace",
]
plugins = ["rust-analyzer-lsp@claude-plugins-official"]

[claude.mcp_servers.my-server]
command = "/usr/bin/my-mcp-server"
args = ["--verbose"]
env = { API_KEY = "MY_API_KEY" }

[codex]
config_dir = "~/.codex"
env_forward = ["CUSTOM_TOKEN"]

[codex.mcp_servers.playwright]
command = "npx"
args = ["-y", "@playwright/mcp@latest"]

[profiles.my-tools]
apt_packages = ["ripgrep", "fd-find"]
post_install = "cargo install ast-grep"

forward_ports = [3000, "8080:18080"]
```
