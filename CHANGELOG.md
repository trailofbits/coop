# Changelog

## Unreleased

### Fixes

- **Guest-memory floor enforced by construction** (#404) — `coop up --mem 16`,
  `coop setup --mem 16`, a `config.toml` with `mem_size_mib = 16`, and a
  cloned repo whose `devcontainer.json` sets `hostRequirements.memory` below
  the 128 MiB minimum are now all rejected up front, instead of booting an
  unbootable VM that never comes up on SSH. The floor lives in a new
  `VmMemory` type whose constructor every entry point routes through (CLI,
  `config.toml`, `coop resize`, the devcontainer translator), so no lifecycle
  path can hold a sub-floor value. The stale `Validated` witness — which
  vouched for a config that lifecycle commands mutated afterward, and which
  the actual VM boot (`create_and_start`) never even consumed — is removed;
  the environmental (path-existence) checks it stood in for now run at the
  backend boot choke point on the freshest filesystem state, so a new
  lifecycle path cannot skip them. The start-time mount set gains a
  `ValidatedMounts` constructor that enforces guest-path uniqueness on every
  path, closing a gap where `coop quickstart` skipped the check.

- **Firecracker CI artifact listing fetched over HTTPS** (#401) — the S3
  `ListObjectsV2` request that discovers kernel/rootfs versions during
  setup used plain HTTP: the bucket name contains dots, which breaks TLS
  certificate matching for the virtual-hosted URL. It now uses the same
  path-style HTTPS endpoint as the downloads themselves, so a network
  attacker can no longer steer version selection.

### Documentation

- **Docs describe the public repository** (#401) — removed the
  transitional "while `trailofbits/coop` is private" wording from the
  README and `docs/commands.md`. `coop update` and `install.sh` work
  anonymously and use `gh`/`GITHUB_TOKEN` opportunistically when present.

### Internal

- **`repository` metadata added to Cargo.toml** (#401).

## v0.5.3

### New features

- **`coop resize` changes memory and vCPUs in place** (#397) —
  `coop resize` now accepts `--mem <MiB>` and `--vcpus <N>` alongside the
  existing `--size`, so a stopped instance's RAM and vCPU count can be
  changed without destroy-and-recreate. At least one of the three is
  required; `--start` boots the instance after applying the change
  instead of leaving it stopped. The backend config artifact is now the
  source of truth for mem/vcpu (mirroring how disk already works): on
  Firecracker the per-instance JSON is read back and only the infra
  fields are regenerated on restart, so a change to the global `[vm]`
  config no longer silently alters RAM for every existing instance; on
  Lima the `cpus`/`memory` keys in `lima.yaml` are rewritten atomically.
  `coop status` reads mem/vcpu from the artifact. A failed `--start`
  boot rolls the values back so the instance stays bootable.

- **`--json` output on read/query commands** (#386) — An opt-in `--json`
  flag on the highest-value read commands emits machine-readable output
  for reliable parsing; the human-readable default is unchanged. Covered:
  `status`, `list`/`ls`, `images`, `devcontainer check`, `github status`,
  `profiles list`, and `up`/`start --dry-run`. Each command builds one
  `Serialize` view model and `--json` switches only the final render
  step, so the human and JSON outputs cannot drift; absence is
  `null`/`[]` rather than sentinel strings. For `devcontainer check` and
  `up`/`start --dry-run` the JSON payload goes to stdout while the human
  report stays on stderr, so `… --json | jq` stays clean. See
  `docs/json-output-design.md`.

### Fixes

- **AppleDouble sidecars no longer copied on macOS hosts** (#376) —
  macOS creates `._`-prefixed AppleDouble sidecar files when archiving
  through the system `tar`. coop now sets `COPYFILE_DISABLE=1` when
  creating workspace archives on macOS so these sidecars are not written
  into the guest transfer.

### Internal

- **`.editorconfig`** (#395) — declares the repo's indentation and
  whitespace rules so editors match the project's formatting.

- **License field in `Cargo.toml`** (#387).

- **Release workflow sets the release title** (#382).

### Documentation

- **`CONTRIBUTING.md`** (#393) and **`SECURITY.md`** (#394) added.

- **Pronunciation guide in `README`** (#383).

### Dependencies

- `anyhow` 1.0.102 → 1.0.103 (#377)
- `clap_complete` 4.6.5 → 4.6.7 (#398)
- `actions/attest-build-provenance` 4.1.0 → 4.1.1 (#399)
- `actions/cache` v5.0.5 → v6.1.0, `cargo-bins/cargo-binstall` v1.20.0 →
  v1.20.1, `zizmorcore/zizmor-action` v0.5.6 → v0.5.7 (#384)

## v0.5.2

### New features

- **`coop model` — route a VM at a host-side local model** (#352) —
  Points Claude Code and Codex at a local model server (Ollama, LM
  Studio, vLLM, llama.cpp) running on the host instead of the cloud,
  configured per tool under `[claude.local_model]` /
  `[codex.local_model]` in `config.toml`. `coop model <vm>` reports the
  per-tool mode and resolved endpoint; `coop model <vm> local` routes
  the VM at the local model(s) and `coop model <vm> remote` restores
  the cloud defaults. The selection is a persisted per-VM setting (not
  a per-launch flag), so it applies to `coop shell`, `coop claude`,
  `coop codex`, and VS Code alike; switching writes guest config over
  SSH with no rebuild and is re-applied on the next boot. A
  `localhost`/loopback `host_url` is rewritten to the backend's
  guest-visible host automatically.

- **`coop codex` bypasses Codex's sandbox by default** (#353) — `coop codex`
  now launches Codex with `--dangerously-bypass-approvals-and-sandbox`, so it
  runs unrestricted like `coop claude`. The VM is the isolation boundary, and
  Codex's Linux sandbox does not work in the guest (no functioning bubblewrap),
  which previously made every shell command Codex ran fail on sandbox setup.
  Pass `coop codex --ask` to keep Codex's sandbox and approval prompts.

- **`coop commit` + `coop restore` — checkpoint and roll back an instance** (#289) —
  `coop commit <name> --image <image>` saves a stopped instance's
  filesystem as a reusable image, a `docker container commit`-style
  backup (files, not live memory). The committed image is an ordinary
  coop image: `coop images` lists it and `coop up --image` launches new
  instances from it. `coop restore <name> --image <image>` rolls a
  stopped instance back to an image's filesystem in place, keeping the
  instance's name, index, IP, and workspace association. Both require a
  stopped instance; `commit --force` overwrites an existing image name.

### Fixes

- **OAuth-token users no longer land in Claude Code's onboarding wizard**
  (#87) — Running `coop claude` with subscription auth
  (`CLAUDE_CODE_OAUTH_TOKEN` forwarded into the guest) dropped the user
  into the first-run theme/login wizard, whose login step ignores the
  token. Claude Code gates the wizard on `hasCompletedOnboarding` in
  `~/.claude.json`, which coop never staged. coop now seeds that flag on
  boot when an OAuth token is forwarded and the flag is not already set.
  Idempotent and a no-op for API-key and no-credential users. Reverts
  when anthropics/claude-code#8938 is fixed upstream.

- **Installed Claude plugins survive a stop/start cycle** (#350) —
  `write_managed_claude_settings` rewrote `~/.claude/settings.json` from
  scratch on every boot with only a `permissions` block, deleting the
  `enabledPlugins` and `extraKnownMarketplaces` keys Claude Code uses to
  track installed plugins and marketplaces. Plugins installed on first
  boot then showed as uninstalled in `/plugins` after a restart. coop now
  merges its managed `permissions` into the existing file, preserving
  those keys.

- **`~` is expanded in every `config.toml` path field** (#349, #351) — A
  leading `~` was only expanded for `claude.config_dir`,
  `codex.config_dir`, and `claude.marketplaces`; other host-path fields
  (`data_dir`, `firecracker_bin`, `vm.kernel_path`, per-profile
  `marketplaces`) kept a literal `~`, so e.g. `data_dir = "~/coop-data"`
  resolved to a literal `./~/coop-data` directory. Expansion now happens
  at deserialization via a `ConfigPath` newtype, so every path field —
  including ones added later — is expanded on every load path.

## v0.5.1

### New features

- **`coop ssh-config` — install a `coop-<name>` SSH alias** (#294) —
  Writes the same `~/.ssh/config` block `coop vscode` produces, but
  without launching an editor, so plain `ssh`, `scp`, and `rsync` reach
  the guest by alias. `--clean` removes the block. The alias now
  survives `coop stop` and is refreshed on `coop start` (the Lima SSH
  port changes per boot), so it stays valid across restarts;
  `coop destroy` and `--clean` remove it.

- **`coop setup --builder-timeout <duration>`** (#315) — bounds how long
  setup waits for image-build commands before timing out. Accepts bare
  seconds or an `s`/`m`/`h` suffix (e.g. `60m`, `2h`). Applies across
  both backends.

### Removed

- **`scripts/build-rootfs.sh`** (#293) — the standalone `debootstrap`
  rootfs builder is removed. Image creation runs through `coop setup`,
  which downloads a Firecracker CI squashfs and provisions it with
  `scripts/guest/guest-config.sh`; the standalone script duplicated that
  provisioning, omitted the CI-kernel workarounds (iptables-legacy,
  static `resolv.conf`, Docker's `DOCKER_INSECURE_NO_IPTABLES_RAW`), and
  produced an image the current flow does not consume. The rootfs
  discovery error no longer points at it.

## v0.5.0

### Breaking changes

- **`coop build` removed; creation flags moved off `coop start`** (#269,
  #281) — `coop start` no longer accepts `--profile`, `--git-repo`,
  `--vcpus`, `--mem`, `--disk`, `--mount`, `--image`, or `--exclude-git`;
  those flags now live on `coop up`. clap reports them as unexpected
  arguments instead of accepting them and bailing at runtime. The
  standalone `coop build` command and the `scripts/fetch-kernel.sh`
  helper are removed — `coop setup` and `coop up --profile` cover image
  creation end-to-end. `scripts/build-rootfs.sh` stays as a documented
  manual fallback. Restart still works against existing instances; the
  flags above only take effect at create time (via `coop up`).

- **`--git-repo` moved from `coop start` to `coop up`** (#264) —
  `coop up --git-repo <url>` clones a remote repository into
  `/workspace` and is idempotent across re-runs, matching the rest of
  `up`: an instance already associated with the URL is reused if
  running, restarted if stopped, and created otherwise. The instance
  name is derived from the repo basename when `--name` is not given.

- **tmux session wrapping removed** (#183, #184) — `coop shell`,
  `coop claude`, and `coop codex` no longer wrap the remote session in
  tmux, and the `--session <name>` / `--no-tmux` flags are gone. The
  `tmux` package is also dropped from the guest base image. Claude
  Code's `claude agents` daemon (`coop claude-agents`) already
  provides session persistence without a terminal multiplexer; users
  who still want detachable terminals can install tmux themselves via
  `coop setup --extra-packages tmux` and start it manually inside
  `coop shell`.

- **Devcontainer auto-discovery prompts on `up` / `setup --workspace`**
  (#129, #130, #242) — When the workspace contains
  `.devcontainer/devcontainer.json` and no escape hatch flag is set,
  coop prompts before applying it. Non-TTY callers must pass
  `--devcontainer <path>`, `--no-devcontainer`, or `--dry-run`; the
  prompt never silently chooses. Precedence is
  `CLI > devcontainer.json > defaults`, and the report column makes
  overrides explicit.

### New features

- **`coop up` — project-oriented environment command** (#230, #264) —
  `coop up [DIR]` ensures an environment for a project directory
  exists and is running, idempotently. `DIR` defaults to the current
  directory and is canonicalized for affinity; a matching instance is
  reconnected if running, restarted if stopped, or created otherwise.
  `--git-repo <url>` uses a remote repository as the workspace instead
  of a local directory. Creation-time flags (`--vcpus`, `--mem`,
  `--disk`, `--image`, `--profile`, `--extra-mount`,
  `--devcontainer`, `--exclude-git`) only take effect when the
  instance is created and are rejected against existing instances with
  a `coop destroy` hint; runtime flags (`--forward-port`,
  `--post-start`, `--env`) take effect on create or restart but are
  ignored when reconnecting to a running VM.

- **`coop quickstart` — one-shot setup + start + claude** (#74, #213)
  — Chains `ensure-image → ensure-instance → launch-claude`,
  short-circuiting any step that's already done. Setup runs only when
  the `default` template image is missing; workspace affinity
  reconnects to a running instance for the current directory (or
  restarts a stopped one) instead of allocating fresh. `--no-workspace`
  opts out of the cwd mount; sensitive directories (`$HOME`, `/`)
  prompt default-no on a TTY and bail non-interactively.

- **`devcontainer.json` support** (#129, #130, #131, #134) — A new
  translator maps recognised keys to coop's existing primitives:
  `hostRequirements` → `--vcpus`/`--mem`/`--disk`, `containerEnv` →
  guest env, `forwardPorts` → `--forward-port`, `postStartCommand` →
  `post_start`, `features` → built-in profiles, `mounts` →
  `--mount`, `remoteUser` → `--guest-user`. New flags on
  `setup` / `up`: `--devcontainer PATH` (opt in to a specific file),
  `--no-devcontainer` (opt out entirely), and `--dry-run` (print the
  per-key report and exit before any side effects). JSONC (`//`,
  `/* */` comments, trailing commas) parses cleanly. CLI `--env` and
  devcontainer `containerEnv` are persisted to `guest_env.json` so
  `coop shell` / `coop exec` see the same values without re-parsing
  config.

- **OCI devcontainer features** (#245) — Supported public
  `ghcr.io/devcontainers/features/*` entries declared in
  `devcontainer.json` are resolved at `coop setup` and baked into the
  image alongside the existing built-in profiles. `coop up --profile`
  continues to compose the built-in profile set.

- **`coop devcontainer` subcommands** (#234, #247, #287) —
  `coop devcontainer check <path>` translates a `devcontainer.json`
  and prints the report without running any setup or VM work
  (`--stage setup|start|both`). `coop devcontainer ignore <project>`
  persistently opts a project out of discovery; `... status` lists
  current opt-outs; `... clear` removes one. Symlinked project
  prefixes are resolved when clearing, so a directory that was moved
  or unlinked doesn't leave a stale opt-out behind.

- **`coop setup --guest-user` and `remoteUser` handling** (#217, #218)
  — Bake a configurable guest username (default `ubuntu`, validated
  against POSIX rules and not `root`) into the image at setup time.
  `start`/`shell`/`exec` read the value from the image's
  `template_config.json`. A `devcontainer.json` that declares a
  different `remoteUser` is honored when its value matches the
  persisted setup user; mismatches surface an actionable diagnostic
  instead of silently writing a wrong home path. Backward-compatible
  for pre-existing images.

- **`--forward-port` / `forward_ports` config** (#125, #128) — Forward
  guest TCP ports to the host for the lifetime of the VM.
  `coop up --forward-port 3000` exposes guest 3000 on host 3000;
  `3000:18080` remaps to a different host port. The flag is
  repeatable, supported by a config-level `forward_ports = [...]`
  default, persisted across `stop`/`start`, and torn down cleanly on
  `coop stop`. Collision with an already-bound host port fails fast
  before the VM is created.

- **`post_start` hook** (#123, #126) — `post_start` in `config.toml`
  (or `--post-start <cmd>` on `coop up` / `coop start`) runs a shell
  command in the guest after SSH is ready and before any interactive
  shell or agent launch. Maps to `postStartCommand` in
  `devcontainer.json`. Failure is logged at WARN and does not fail
  the start, so a transient hook problem can't strand the VM.

- **`[guest_env]` config block + `--env KEY=VALUE`** (#127, #131,
  #134) — Set literal env vars inside the guest without forwarding
  from the host. CLI overrides win over config and devcontainer
  values; `--env` is persisted to `guest_env.json` so
  `coop shell` / `coop exec` see the same values without rerunning
  `start`. `coop start --env` and `--devcontainer`-derived
  `containerEnv` survive `stop`/`start` cycles.

- **Build profile images on demand from `coop up`** (#235) —
  `coop up --profile <list>` derives an image name from the sorted
  profile list, runs the same stale-image check as `coop setup`, and
  builds or rebuilds that image if needed. Explicit named images
  (`--image`) are unchanged.

- **Submodule discovery in `coop github setup-pat`** (#219, #221,
  #223) — The wizard pre-discovers submodule repositories via the
  local `gh` install (no network round-trip per submodule) and offers
  to set up a PAT for each one.

- **Guest PATH set via `/etc/environment`** (#248, #249) —
  `~/.local/bin` is now reliably on `PATH` for non-interactive SSH
  sessions, including `coop claude` and Claude Code's Bash-tool
  subshells. Previously these sessions skipped `~/.profile` and
  `~/.bashrc`'s PATH export, hiding `uv`/`pipx`/user tools and
  triggering `claude doctor` warnings. `pam_env` reads
  `/etc/environment` for every PAM session regardless of shell mode,
  so the prepend reaches remote commands, their subshells, and
  VS Code remote sessions.

- **Docker buildx in guest images** (#288) — `docker buildx` is
  installed in every coop image so multi-arch and `buildkit`-driven
  builds work without manual setup.

- **Secrets redacted from `Debug` output** (#79, #121, #262) — A new
  `Secret<T>` newtype with redacting `Debug`, transparent serde, and
  an explicit `expose()` accessor wraps `ClaudeConfig.api_key`,
  `CodexConfig.api_key`, and `PatEntry.token`. MCP header secrets
  and forwarded env values render as `<redacted>` in error chains,
  tracing events, `dbg!`, and panics.

- **Hint at `--workspace`/`--mount` when `start` name looks like a
  path** (#226) — Running `coop start ./my-project` now surfaces a
  hint to use `coop up` (which interprets `DIR` correctly) instead of
  failing with an opaque "instance not found".

- **Warn when `--mount` source is a live git repo** (#102, #122) —
  Live mounts share filesystem state with the host, so in-guest
  changes affect the host checkout immediately. coop now warns when
  the mount source looks like a working tree.

### Fixes

- **Survive mid-session SSH exit 255 in interactive sessions** (#224,
  #227) — A network blip that produced SSH exit 255 mid-session no
  longer kills the wrapping coop process; the session reattaches.

- **Interactive SSH escape path hardened** (#232) — Closes a regression
  in escape-sequence handling on the interactive path.

- **`workspace.json` parse errors surfaced** (#225) — Three call sites
  previously swallowed parse errors; all now report them with context.

- **Bare `coop start --mount` allocates a fresh instance** (#214,
  #215) — Earlier this collided with the auto-resolve path and
  produced a confusing error.

- **`coop up` workspace sync no longer drops `--extra-mount` on
  Firecracker** (#264) — The workspace-sync block now always syncs
  extra mounts; only mount-only instances record the workspace
  identity. Also fixes a tar-pipe fallback that extracted to
  `/workspace` regardless of the mount's guest path.

- **`coop start` devcontainer lifecycle ordering** (#241) — Devcontainer
  translation now runs before the SSH-ready probe, so per-key
  reporting reflects what will actually be applied.

- **Skip redundant `is_running()` in stop / stream-logs** (#195,
  #205) — These were probing the running state twice on the
  SSH-readiness path; collapsed to a single probe.

- **Direct-probe `resolve_instance` fast path for by-name lookups**
  (#202, #203, #211, #212) — Defers `CoopConfig::validate` until
  commands that touch probed paths actually need it, and skips the
  generic search when an instance name is supplied.

- **Lima SSH-readiness probe via `ssh.config` instead of
  `limactl list`** (#201, #210) — Cuts a 0.4–1.5 s `limactl`
  invocation off the resolve fast path on macOS.

- **Firecracker CI kernel fallback** (#290) — When the latest
  Firecracker CI minor has no assets published yet (transient
  release-pipeline gap), setup falls back to the previous minor
  instead of failing.

- **Integration tests** — Forward-port test uses a `python3` listener
  instead of `nc` (Firecracker CI rootfs ships no netcat) (#132).
  `test_list_empty` tolerates pre-existing instances on a shared
  host (#133). `--forward-port` collision test reads `$HARNESS_ERR`
  instead of an outer redirect (#135). Devcontainer opt-out test
  assertions match the actual message text (#286). Stop idempotency
  test reuses a stopped instance (#240). Bare `coop start --mount`
  `pipefail`+`grep -q` race fixed (#220).

### Internal

- **VM lifecycle type-state** (#153, #180, #275) — `RunningInstance`
  and `StoppedInstance` carry lifecycle state in the type, eliminating
  runtime state checks on operations like `resize_disk`, `status`,
  and `stop`. The `RunningInstance` proof extends across both
  backends.

- **Newtype / enum survey across module boundaries** —
  `InstanceName` (#192, #199), `RepoSlug` (#149, #171),
  `ImageName` (#157, #181), `Hostname` and `SshUser` (#154, #187),
  `GuestUser` (#217, #218), `ToolName` and `AccountName` (#165,
  #190), `EnvVarName` (#151, #177, #196, #206), `MemorySpec` (#163,
  #188), `MiB`/`GiB` (#194, #207), `Sha256Hash` (#161, #189),
  `Architecture` (#169, #173), `PortForward` and `Mount` (#179),
  `HostInterface` (#159, #186), `SubnetMask(u8)` (#150, #176),
  `InstanceIndex` 0..=252 bound (#162, #175), `TmuxSessionName` +
  `RemoteCommand` (#166, #182, later removed with #183),
  `GuestPath` round-trips dropped (#193, #204). Profile names
  resolved at the config boundary (#156, #172).
  `--mount`/`--env`/`--forward-port` and disk size / devcontainer
  path / repo slug parsed by clap value-parsers (#179, #285).
  `redacted_args: Vec<usize>` replaced with typed `Arg` variants
  (#160, #185). `McpServerDef` replaced with a transport-tagged
  enum (#148, #178). `WorkspaceState` optionals collapsed into
  `WorkspaceSource` variants (#170). `restart: bool` and
  `follow: bool` replaced with two-variant enums (#174).
  Correlated `bool` fields replaced with enums (#266, #280).
  `cmd:` string reverse-parsing replaced with a structured
  `CmdToken` (#277). Guest-env precedence merge unified (#276).
  PAT repo probe typed with a `ProbeError` enum (#282). Dead
  wrappers removed and over-broad visibility tightened (#283).
  Near-identical function pairs extracted into shared helpers
  (#284). Newtypes threaded through boundaries instead of decaying
  to `String` (#279). A four-PR survey of smaller newtype / struct
  refactors (#191).

- **Mutation-testing baseline on `config.rs`** (#136, #137, #138,
  #139, #140, #141, #143, #144, #145, #146) — Adds the
  mutation-testing guidance now in `CLAUDE.md`, pins
  `CoopConfig::validate`, `Instance::is_running`,
  `is_firecracker_process`, and `MiB::as_gib_f64` with tests, and
  annotates equivalent mutants (`fmt::Display` impls, default-value
  getters, serde `Visitor` methods) with `#[mutants::skip]` and a
  one-line justification.

- **Port-forward integration test covers restart re-establishment**
  (#260).

- **`[guest_env]` config block integration phase** (#263).

- **OCI devcontainer feature install integration phase** (#261).

### Documentation

- **`coop up`, `coop quickstart`, and the devcontainer workflow**
  documented in `docs/commands.md`, `docs/getting-started.md`, and
  `docs/devcontainer.md` (#213, #230, #234, #257).

- **Guest user, guest PATH, and startup flag pointers** (#259).

- **Submodule discovery in setup-pat wizard** (#258).

- **Mutation-testing guidance** (#136) added to `CLAUDE.md`.

### Dependencies

- `serde_json` 1.0.149 → 1.0.150 (#228)
- `zizmorcore/zizmor-action` 0.5.3 → 0.5.5 → 0.5.6 (#198, #229)

## v0.4.4

### Breaking changes

- **`coop push` / `coop pull` / `coop exec` take the instance name as a
  positional argument** (#90) — these three commands previously accepted
  `--name <name>` while the other eleven subcommands took `name`
  positionally. The flag is removed; pass the name positionally instead.
  Because `push` and `pull` already had `[DIR]` as a positional, the
  directory is now a `--dir` flag. Because `exec` had `COMMAND...` as a
  positional, the command must follow `--`. Examples:
  `coop push my-vm --dir ./src --force`,
  `coop pull my-vm --dir ./out --force`,
  `coop exec my-vm -- ls -la`.

### New features

- **`coop ca` / `coop claude-agents` shortcut** (#80, #82, #99, #100, #101) —
  Runs `claude agents` inside the VM in one command. Claude Code's daemon
  manages background-session lifetime itself, so closing the terminal
  does not interrupt running agents. The guest is now bootstrapped with
  a managed `~/.claude/settings.json` that pre-accepts
  `bypassPermissions`, so dispatched sessions no longer prompt for
  tool permissions; `coop claude --ask` explicitly opts back into the
  prompting default.

- **`coop github setup-pat` wizard** (#85, #88) — Walks the user
  through creating a fine-grained personal access token scoped to one
  repo, stores it in the user's preferred secret store (Keychain,
  Secret Service, 1Password, or file), and forwards it to the guest as
  `GITHUB_TOKEN` keyed off the resolved repo slug. Adds a new
  `github = "pat"` config mode.

- **`coop list` / `coop ls`** (#89, #94) — Local-only enumeration of
  instance name + state. Reads on-disk metadata and `be.is_running`
  without SSH probes so it stays fast even when VMs are unreachable.
  `coop status` keeps its richer per-instance and resource-usage
  output.

- **`coop uninstall`** (#93, #96) — Reverses what `install.sh` does:
  removes the running coop binary and, with confirmation, the data
  directory (`~/.coop`) and XDG update-check state. Flags
  `--yes` / `--keep-data` / `--purge`. Refuses to delete
  `target/{debug,release}/coop` and surfaces EPERM with a
  `sudo coop uninstall` hint. Bails on non-TTY stdin without `--yes`
  so CI misuse fails loud.

- **Shell completion** (#92, #98) — `coop completions <shell>` prints
  a static completion script for bash, zsh, fish, powershell, and
  elvish. Adding `source <(COMPLETE=<shell> coop)` to a shell rc
  additionally fills in live values via clap_complete's dynamic
  engine — instance, image, and profile names are read from `~/.coop`
  on each TAB.

- **`--git-repo` clones authenticate against private GitHub repos**
  (#78, #119) — On the host, resolve a token in order: a configured
  `[github.pat."<slug>"]` entry for the repo, then `gh auth token`,
  then `GITHUB_TOKEN`. Forward it to git in the guest via stdin and
  a one-shot `credential.helper`. The token never appears on argv,
  stays out of `/proc/<pid>/cmdline` and the ssh debug log, and is
  not persisted in the cloned repo's `.git/config`. Opportunistic:
  GitHub HTTPS URLs only; non-GitHub and SSH-style URLs pass through
  untouched. A misconfigured PAT entry fails at start time rather
  than silently substituting a broader identity.

- **`.git/` included in workspace transfers by default** (#95) —
  `coop start --workspace`, `coop push`, and `coop pull` previously
  hardcoded `.git/` into the default exclusion list, breaking
  in-guest git history and rendering `check_guest_dirty` a no-op.
  Now transferred by default, with an `--exclude-git` opt-out on
  `start` / `push` / `pull` for repos large enough that the transfer
  cost dominates. `check_guest_dirty` also now detects unpushed
  commits (`@{u}..HEAD`) so in-guest commits aren't silently
  destroyed by a host push.

### Fixes

- **`integration-uninstall.sh` state path on macOS** (#106) —
  `dirs::state_dir()` returns `None` on macOS, so `state_path()` in
  `src/update.rs` falls back to `~/Library/Application Support/coop/`.
  The test seeded `$XDG_STATE_HOME/coop/update-check.json` but the
  binary never wrote there, so the `--purge` assertion failed. The
  test now uses the same platform branching the binary does.

- **SIGPIPE flake in bash completion integration check** (#105) —
  `echo "$HARNESS_OUT" | grep -q "coop,$sub"` against the ~48 KB
  completion script flaked under `set -o pipefail`: when `grep -q`
  matched early it closed the pipe, bash's `echo` builtin exited 141
  (SIGPIPE), and the pipeline status masked `grep`'s success (~60%
  of trials on Linux 6.17 / bash 5.2.21). Replaced the pipe with a
  here-string.

- **`destroy --all` integration phase gated behind
  `COOP_TEST_DESTRUCTIVE=1`** (#104) — `coop destroy --all` removes
  every coop-managed instance on the host, not just the ones the
  test created. The phase is now skipped by default; remote mode
  forwards the opt-in env var explicitly.

### Internal

- **`open_ssh_session` helper extracted** (#81, #83) — The five-line
  `resolve_running` + `prepare_env_forwarding` + `SshSession` setup
  repeated across Claude, ClaudeAgents, Codex, plus `cmd_shell` and
  `cmd_exec`, collapses to a single `open_ssh_session` call.
  `SshSession` is now owned rather than borrowing target/env;
  removing the lifetime parameter drops `SshSession<'_>` from nine
  downstream signatures. Incidental behavior change: with
  `--no-agents`, a misconfigured `cmd:` secret source no longer
  fails the boot path.

### Documentation

- **Rust authoring + review guidance in CLAUDE.md** (#84) —
  Project-specific patterns for using the type system to eliminate
  error states: parse-don't-validate, smart constructors, type-state
  for the VM lifecycle, newtypes that earn their keep, and error
  design. Includes prioritized review and authoring checklists.

### Dependencies

- `cargo-bins/cargo-binstall` 1.18.1 → 1.19.1 (#86)

## v0.4.3

### Fixes

- **`coop update` works on the private/internal `trailofbits/coop` repo**
  (#70, #71) — Previously the in-binary update shelled out to bare
  `curl`, which the GitHub API answers with 404 for unauthenticated
  requests against private repos, surfacing as `curl exited with exit
  status: 22`. The update path now authenticates the same way
  `install.sh` already did: prefer `gh` (when installed and authenticated
  against `github.com`), fall back to `GITHUB_TOKEN`, then bare curl
  (which works once the repo is public).

- **GitHub token no longer appears on argv** (#71) — Both `src/update.rs`
  and `install.sh` now feed the `Authorization` header to curl on stdin
  (`curl -H @-`) instead of as a command-line argument, so
  `$GITHUB_TOKEN` is no longer visible in `/proc/<pid>/cmdline` or in
  `coop -v update`'s tracing debug log.

### Documentation

- README and `docs/commands.md` note that `coop update` requires `gh` or
  `GITHUB_TOKEN` while the repository is private (#71).

## v0.4.2

### Fixes

- **SSH connections respect `IdentitiesOnly`** (#68) — When `ssh-agent`
  holds many keys, ssh offered all of them before the explicit `-i` key,
  hitting sshd's default `MaxAuthTries=6` and producing "SSH not ready"
  on `coop start`. SSH/SCP/rsync invocations and the generated
  `~/.ssh/config` block now set `IdentitiesOnly=yes`, matching Lima's
  own probes.

- **Workspace tar-pipe transfer** (#66, #67) — Surface SSH stderr (with
  a `coop start --disk` hint when the message mentions "no space left
  on device") instead of a generic "tar archive truncated" error. Peak
  guest disk usage during transfer is now the extracted tree, not 2× —
  the temp-file/SHA-256 dance was redundant since SSH already MACs the
  channel. Dedicated background threads drain remote and local tar
  stderr to prevent deadlocks when warnings fill the 64K pipe buffer
  during extraction.

- **Integration test no longer pollutes user state** (#63, #64) —
  `tests/integration-update.sh` redirects `$HOME` and XDG vars to a
  tempdir before invoking coop, so the synthetic `v9.9.9` release
  served by the test fixture no longer lands in
  `~/.local/state/coop/update-check.json` and surfaces as a bogus
  update notification on later runs.

### Dependencies

- `libc` 0.2.185 → 0.2.186, `semver` 1.0.27 → 1.0.28 (#65)

## v0.4.1

Re-release of v0.4.0. The v0.4.0 tag did not produce release artifacts
because `tests/integration-update.sh` Test 4 ("dev build refusal") fails
whenever CI runs on a commit tagged `v{cargo_version}` — `build.rs`
correctly bakes `COOP_BUILD_KIND=release` for that commit, so the test's
unset-override path produced a release binary instead of a dev one. Test 4
now sets `COOP_FORCE_BUILD_KIND=dev` explicitly, mirroring Test 1's
`=release` override. No functional changes to `coop` itself since v0.4.0.

## v0.4.0

### New features

- **Codex CLI support** (#44, #49) — `coop codex` launches OpenAI's Codex
  inside the guest, alongside Claude Code. `~/.codex` config and auth are
  staged into the VM, `OPENAI_API_KEY` is forwarded, and MCP servers
  configured under `[codex.mcp_servers]` are merged into the guest's
  `~/.codex/config.toml`. A `codex-yolo` guest alias mirrors the existing
  `claude-yolo` shortcut. Thanks to Artem Dinaburg for contributing the
  initial Codex integration.

- **`coop update`** (#34, #55) — Self-updates the coop binary from GitHub
  Releases. Downloads the tarball matching the host triple, verifies
  SHA-256 against the release's `SHA256SUMS`, and (when `gh` is installed)
  verifies the build-provenance attestation before atomically replacing
  the running binary. Flags: `--check` (probe only), `--force` (reinstall
  same version), `--version <tag>` (pin), and `-y`/`--yes` (skip
  confirmation). Dev builds refuse to self-update; re-run `install.sh`
  to replace them.

- **Background update-check notifications** (#55) — On every command,
  coop reads the persisted state in `$XDG_STATE_HOME/coop/update-check.json`;
  if a newer release is known, coop prints a one-line notice to stderr.
  The refresh runs in a detached thread and never blocks the command.
  Disable globally with `updates.mode = "off"` in `config.toml`, or
  per-invocation with `COOP_NO_UPDATE_CHECK=1`. The check stays silent
  when `CI=true` or when stdin is not a TTY.

- **`install.sh` verifies build-provenance attestations** (#56) — When
  `gh` is installed, the installer runs `gh attestation verify` after
  the SHA-256 check, matching `coop update`. Without `gh`, both paths
  fall back to checksum verification and print a note describing what
  the checksum covers and what attestation verification would add.
  README documents the manual `gh attestation verify` one-liner.

- **`coop --version` includes git metadata** (#55) — Release builds
  display the short commit sha (e.g. `coop 0.3.1 (a1b2c3d)`); dev builds
  add `-dev` and a `+dirty` suffix when the working tree has
  uncommitted changes.

### Deprecations

- **`--no-claude` is deprecated** (#49) — Use `--no-agents` instead.
  The old flag still works as a hidden alias and emits a runtime
  warning. A future release will remove it.

### Dependencies

- New: `semver` 1
- Cargo dependency updates (`clap`, `indexmap`, `libc`)
- GitHub Actions bumps (`actions/cache`, `actions/upload-artifact`,
  `cargo-bins/cargo-binstall`, `zizmorcore/zizmor-action`)

## v0.3.1

Re-release of v0.3.0. The v0.3.0 release artifacts failed to publish because
the release workflow conflicted with a pre-created GitHub release. Release
notes are now sourced from `CHANGELOG.md` so the workflow owns the full
release end-to-end.

No functional changes since v0.3.0.

## v0.3.0

### Breaking changes

- **`ssh` subcommand renamed to `shell`** (#25) — `coop ssh` is now `coop shell`.
  A hidden `ssh` alias exists for backward compatibility, but scripts and docs
  should migrate to `shell`.

- **`full` meta-profile removed** (#31) — `--profile full` no longer exists.
  Use `--profile python,node,c,fuzz,rust,go` explicitly.

- **Instance names derived from workspace path** (#33) — `coop start --workspace
  <path>` without `--name` now derives the instance name from the directory
  basename (e.g. `~/projects/myapp` → `myapp`). Existing stopped instances
  created under the old numeric naming scheme won't match by workspace affinity.
  Destroy and recreate them, or reference by their old name explicitly.

- **`start` rejects creation-time flags on restart** (#24) — Passing `--mount`,
  `--workspace`, `--git-repo`, or `--disk` when restarting a stopped instance
  now errors instead of silently dropping those flags. Destroy and recreate the
  instance to change these settings.

### New features

- **`coop profiles list` / `coop profiles show`** (#31) — Discover builtin and
  custom profiles without reading source or config. Bare `coop profiles`
  defaults to `list`.

- **`--session <name>` flag for `shell` and `claude`** (#25) — Named tmux
  sessions enable parallel interactive sessions against the same VM.

- **Workspace affinity** (#33) — `coop start --workspace <path>` finds and
  restarts a stopped instance that previously used that workspace instead of
  creating a duplicate.

- **`cmd:` prefix for secret manager integration** (#35) — Config values for
  `claude.api_key` and MCP server headers support `cmd:<command>` syntax. The
  command runs at VM start time (10s timeout) and stdout becomes the resolved
  value. Works with 1Password, `aws secretsmanager`, etc.

- **`push`/`pull` without prior `--workspace`** (#26) — `coop push` and
  `coop pull` now work even if the instance wasn't started with `--workspace`,
  syncing the current directory.

### Fixes

- **Lima v2.1.0 support** (#38) — Handle the `diffdisk` → `disk` rename in
  Lima v2.1.0. Falls back to `diffdisk` for older versions.

- **HTTPS for chroot apt sources** (#39) — Guest package installation uses
  HTTPS mirrors.

### Dependencies

- `sha2` 0.10.9 → 0.11.0
- Cargo dependency updates (`clap`, `serde`)
- `actions/upload-artifact` bump in release workflow

## v0.2.5

Initial public release.
