# Architecture

`coop` is a Rust CLI that orchestrates isolated VM environments for running AI
coding agents (Claude Code, Codex). It manages the full VM lifecycle — setup,
start, shell, stop, destroy, status, logs — behind two platform backends:

- **Linux** — Firecracker microVMs on KVM.
- **macOS** — Lima VMs on Apple Virtualization.framework (`limactl`).

This document maps the modules, the two-backend design, the data flow from host
to guest, and the architectural invariants. For the security view of the same
system, see [`trust-model.md`](trust-model.md); for Rust conventions, see
[`code-style.md`](code-style.md).

## Layout

```
coop/
├── src/
│   ├── main.rs             # thin binary shim → coop::run()
│   ├── lib.rs              # clap CLI definition + central command dispatcher
│   ├── backend.rs          # VmBackend trait, PlatformBackend alias, shared guest ops
│   ├── vm.rs               # Firecracker process management (typestate machine)
│   ├── lima.rs             # macOS/Lima backend implementation
│   ├── setup.rs            # Firecracker host setup + golden-image builder
│   ├── network.rs          # Firecracker TAP/bridge/NAT networking
│   ├── config.rs           # config model + loading (the type-safe core)
│   ├── workspace.rs        # workspace sync (rsync/tar) + ~/.ssh/config injection
│   ├── devcontainer.rs     # devcontainer.json parse + translate to coop config
│   ├── devcontainer_oci.rs # devcontainer Features from the GHCR OCI registry
│   ├── git_repo_devcontainer.rs # fetch devcontainer.json from a remote repo
│   ├── guest.rs            # guest profiles, required binaries, baked package lists
│   ├── guest_env_state.rs  # persisted guest env vars
│   ├── model_state.rs      # per-instance local/remote model routing
│   ├── secret_store.rs     # pluggable secret backends (keychain/op/secret-tool/file)
│   ├── github_pat.rs       # `coop github` PAT wizard (setup/rotate/status/forget)
│   ├── github_repo.rs      # RepoSlug + repo-URL parsing
│   ├── github_submodules.rs# submodule discovery for PAT scoping
│   ├── pat_prompt.rs       # interactive pre-start PAT prompt
│   ├── ssh.rs              # interactive SSH launching
│   ├── remote_command.rs   # injection-safe remote shell-command builder
│   ├── cmd.rs              # host std::process::Command wrapper (redaction, sudo)
│   ├── port_forward.rs     # ssh -L port forwards + state
│   ├── signal.rs           # SIGINT/SIGTERM cooperative cancellation
│   ├── paths.rs            # GuestPath / HostPath type split
│   ├── fs_util.rs          # atomic file writes, file locks
│   ├── sha256_hash.rs      # Sha256Hash newtype
│   ├── jsonc.rs            # JSONC → JSON (for devcontainer.json)
│   ├── naming.rs           # safe-name character class
│   ├── completions.rs      # shell completion (static + dynamic candidates)
│   ├── prompt.rs           # TTY prompts
│   ├── update.rs           # `coop update` self-update + background notifier
│   └── commands/           # one module per command domain (see below)
├── scripts/guest/          # guest-image provisioning scripts (embedded at build)
├── guest/init.sh           # guest first-boot init
├── tests/                  # integration test scripts (see AGENTS.md "Before committing")
├── fuzz/                   # cargo-fuzz targets (own workspace)
└── docs/                   # this tree
```

All application logic lives in the **library crate** (`src/lib.rs`); `main.rs`
is a six-line shim calling `coop::run()`. This is why unit and mutation tests
run against `--lib`.

## The two-backend design

The central abstraction is the `backend::VmBackend` trait — every VM operation
(`setup`, `create_and_start`, `start_existing`, `stop`, `destroy_instance`,
`resize_disk`, `commit_disk`, `status`, `stream_logs`, `ssh_target`, …) goes
through it. Two implementations exist:

- `FirecrackerBackend` — `#[cfg(not(target_os = "macos"))]`; delegates to
  `setup`, `vm::FirecrackerVm`, and `network`.
- `LimaBackend` — `#[cfg(target_os = "macos")]`; delegates to `lima`.

**Backend selection is compile-time, not runtime.** `backend::PlatformBackend`
is a type alias resolved by `#[cfg]` — `LimaBackend` on macOS, `FirecrackerBackend`
elsewhere. There is no runtime backend enum and no dispatch cost. (Note: the
`Backend` enum in `secret_store.rs` is unrelated — it names *secret-storage*
backends.)

Everything above the trait is **backend-shared**: the entire "shared guest
operations" surface in `backend.rs` (env/secret forwarding, agent bootstrap,
Claude/Codex config injection, git-repo cloning), plus `workspace.rs`,
`ssh.rs`, `config.rs`, and the `commands/` handlers. When you touch shared
code, it must hold for **both** backends. Known intentional divergences:

| Aspect | Firecracker | Lima |
|--------|-------------|------|
| `guest_host_address` | TAP gateway (`network.host_ip`) | `host.lima.internal` |
| `mounts_are_live` | `false` (one-time rsync/tar sync) | `true` (virtiofs) |
| `ssh_target` | built from `guest_ip` + `ssh_port` | queried from `limactl` (per-boot forwarded port) |
| workspace/mounts | rsync or tar-pipe over SSH | live mounts |

### Typestate invariants

Two lifecycle machines are encoded in the type system rather than in runtime
flags — this is a load-bearing design choice (see
[`code-style.md`](code-style.md#type-state-for-lifecycles)):

- **`RunningInstance` / `StoppedInstance`** (`backend.rs`) are unforgeable
  liveness proofs with private fields, minted only by the probes `as_running`
  / `as_stopped`. Operations that require a live (or stopped) VM take the proof
  by value, so the precondition is checked once and then witnessed by the type.
- **`FirecrackerVm<Configured>` / `FirecrackerVm<Running>`** (`vm.rs`) gate
  `start()`/`stop()` transitions at compile time.
- **`boot_preflight(cfg)`** (`backend.rs`) is the single choke point every boot
  path calls first; it runs `cfg.validate()` so no VM starts on an invalid
  config.

## Command dispatch

`lib.rs:run()` is the entrypoint. Order matters:

1. Emit dynamic shell completions (before arg parsing) if requested.
2. Parse `Cli` (clap derive), init tracing (→ **stderr**).
3. Handle commands that must work **without** a loaded config —
   `Completions`, `Init`, `Update`, `Uninstall`, `Devcontainer check` — first.
4. `config::CoopConfig::load`, then fire the update-notifier check.
5. Construct `backend::PlatformBackend::new()`.
6. `match` each `Commands` variant to a `commands::cmd_*` handler. Lifecycle
   handlers call `cfg.validate_and_warn()?` before VM work; query commands
   (`list`/`status`/`logs`) skip it.

The `commands/` submodules own the domains: `lifecycle.rs` (up/start/shell/
exec/stop/destroy/status/list/resize/commit/restore), `quickstart.rs`,
`devcontainer.rs`, `profiles.rs` (+ images), `agent.rs` (`coop agent update`),
`model.rs` (`coop model`), `github.rs`, `admin.rs` (init/validate/uninstall),
and `json.rs` (machine-readable `--json` output types). `commands/mod.rs`
re-exports the dispatch surface and holds cross-domain helpers
(`merge_runtime_guest_env`, `purge_all_data`).

## Data flow: host → guest

The lifecycle is **setup → up/start → shell → stop → destroy**. A first boot
(`lifecycle.rs:start_instance`, `BootMode::FirstBoot`) runs roughly:

1. **Resolve** the target repo/workspace and optionally prompt for a GitHub PAT
   (`pat_prompt::maybe_prompt`).
2. **Ports** — merge forward-port specs and fail fast on host-port collisions
   (`port_forward::check_host_port_collisions`).
3. **Boot** the VM (`be.create_and_start`), then `wait_until_ready` (SSH probe
   with backoff).
4. **Forwards + state** — spawn `ssh -L` forwards, persist `ForwardsState`,
   `GuestEnvState`, `DevcontainerState` as JSON sidecars in the instance dir.
5. **Bootstrap** (`backend.rs:bootstrap_agents`): if a `GITHUB_TOKEN` is present,
   `gh auth setup-git`; then `bootstrap_claude` / `bootstrap_codex` inject the
   allowlisted config files and a managed `settings.json`, and on first boot
   install the *delta* of marketplaces/plugins/MCP servers not already baked
   into the golden image.
6. **Workspace** — `--workspace` copies via tar-pipe; `--git-repo` clones inside
   the guest; mounts are live on Lima and rsync'd on Firecracker. Persist
   `WorkspaceState`.
7. **`postStartCommand`** hook (warned, not fatal).

Config, secrets, and workspace all cross the host→guest boundary here; the
security-relevant details of each crossing are in
[`trust-model.md`](trust-model.md).

### Config model

`config::CoopConfig` is the type-safe core. Loading reads TOML (or JSON by
extension) or defaults, then expands user paths. Value bounds are enforced by
**newtype constructors** (`VmMemory` has a 128-MiB floor, `InstanceIndex` is
`0..=252`, `SubnetMask` is `0..=32`, `Hostname`/`SshUser`/`EnvVarName` validate
their character classes), so `validate()` only checks *environmental* facts
(paths exist, binaries are present). Secrets are stored indirected as `cmd:`
retrieval commands and resolved at VM-start by `resolve_cmd_value`.

Per-instance runtime state is a set of JSON sidecar files under the instance
dir: `instance.json`, `vm_config.json`, `workspace.json`, `forwards.json`,
`guest_env.json`, `model.json`, `devcontainer_state.json`, plus the Firecracker
`.pid`/`.socket`/`.log`/vsock files.

## `coop update`

`update.rs` self-updates the binary: fetch release metadata from the pinned
`trailofbits/coop` repo, download the platform tarball + `SHA256SUMS` +
`attestations.jsonl`, verify the checksum (mandatory), verify the Sigstore
attestation via `gh` against that bundle, falling back to the attestations API
when the release publishes no usable one (best-effort),
extract with path-escape-safe `tar` flags, and atomically `rename` the new
binary over the running one. A background notifier checks for new versions on a
24-hour interval (disabled in dev/CI/non-TTY). The full verification chain and
its trust properties are documented in [`trust-model.md`](trust-model.md#coop-update-trust-chain).

## Guest image

The guest runs from a golden image built once and reused (reflink-cloned per
instance on Firecracker). `setup.rs` builds it via a chroot, running the
embedded provisioning scripts in `scripts/guest/` (`preamble.sh`,
`guest-config.sh`, `cleanup.sh`) plus the profile/package installers referenced
from `guest.rs`. Firecracker uses a minimal CI kernel that is missing several
netfilter modules; the resulting workarounds (iptables-legacy, static
`resolv.conf`, `DOCKER_INSECURE_NO_IPTABLES_RAW=1`) are applied in
`guest-config.sh` and explained in [`AGENTS.md`](../AGENTS.md) and
[`trust-model.md`](trust-model.md#documented-accepted-trade-offs).

## Architectural invariants

Hold these when changing the code; the review agents check for their violation:

1. **The VM is the isolation boundary.** The guest is untrusted from the host's
   view; guest-authored data must not escalate into host code execution or
   filesystem escape. See [`trust-model.md`](trust-model.md).
2. **Backend selection is compile-time.** Don't add a runtime backend enum;
   keep shared code correct for both Firecracker and Lima.
3. **Liveness is a type, not a flag.** Route VM operations through
   `RunningInstance`/`StoppedInstance` and `boot_preflight`, not ad-hoc
   `if is_running` checks.
4. **Value invariants live in constructors.** Parse into a newtype at the
   boundary; don't re-validate primitives downstream.
5. **Secrets never touch argv or logs.** Env/`SendEnv`/stdin only; redact in
   traces.
6. **Remote commands are built with `RemoteCommand::arg`, host commands with
   `Cmd::arg`** — no shell-string interpolation of tainted input.
7. **State is transparent JSON sidecars** written atomically
   (`fs_util::atomic_write_*`) without relaxing permissions.
