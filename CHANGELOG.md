# Changelog

## v0.4.4

### Breaking changes

- **tmux session wrapping removed** (#183) — `coop shell`, `coop claude`,
  and `coop codex` no longer wrap the remote session in tmux, and the
  `--session <name>` / `--no-tmux` flags are gone. `tmux` is no longer
  installed in the guest image either. Users who want detachable sessions
  can run `coop shell` and start tmux themselves, after installing it as
  an extra package (`--extra-packages tmux` on `coop setup`) or via a
  custom post-install script. Claude Code's background-agent daemon
  (`coop claude-agents`) already provides session persistence without a
  terminal multiplexer.

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

- **`--forward-port` / `forward_ports` config** (#125) — Forward guest
  TCP ports to the host for the lifetime of the VM. `coop start
  --forward-port 3000` exposes guest 3000 on host 3000; `3000:18080`
  remaps to a different host port. The flag is repeatable, supported
  by a config-level `forward_ports = [...]` default, persisted across
  `stop`/`start`, and torn down cleanly on `coop stop`. Collision with
  an already-bound host port fails fast before the VM is created.

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
