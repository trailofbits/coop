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
| `github` | string | unset (treated as `"off"`) | GitHub authentication strategy. See [GitHub auth](#github-auth). |

## GitHub auth

The `github` field determines how coop obtains a `GITHUB_TOKEN` for the guest:

| Value | Behavior |
|-------|----------|
| `"auto"` | Checks `$GITHUB_TOKEN` first. Falls back to `gh auth token` if unset. |
| `"env"` | Reads `$GITHUB_TOKEN` from the environment only. Warns if unset. |
| `"off"` | No GitHub token forwarding. |

When a token is present, coop runs `gh auth setup-git` inside the guest to wire up git credential helpers.

## `vm` section

VM resource allocation and boot configuration.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `vcpu_count` | integer | `2` | Number of vCPUs. Must be > 0. Overridable with `--vcpus` on `setup` and `start`. |
| `mem_size_mib` | integer | `4096` | Memory in MiB. Must be >= 128. Overridable with `--mem` on `setup` and `start`. |
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
| `config_dir` | string (path) or `false` | `~/.codex` | Source directory for Codex config files. Copies an allowlist of entries (`AGENTS.md`, `prompts/`, `config.toml`) from this directory to `~/.codex/` in the guest on start. Set to `false` to disable. Supports `~` expansion. |
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

Custom profiles compose with built-in ones (`python`, `node`, `c`, `fuzz`, `rust`, `go`, `full`). Combine them with commas: `coop setup --profile python,node,my-tools`.

## CLI overrides

Several config values accept per-invocation overrides via flags:

| Flag | Applies to | Overrides |
|------|-----------|-----------|
| `--vcpus <N>` | `setup`, `start` | `vm.vcpu_count` |
| `--mem <MiB>` | `setup`, `start` | `vm.mem_size_mib` |
| `--template-size <GiB>` | `setup` | `vm.template_size_gib` |
| `--disk <GiB>` | `start` | Per-instance disk size (grows from template if larger) |
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
```
