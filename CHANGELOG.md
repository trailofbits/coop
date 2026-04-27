# Changelog

## v0.4.0

### New features

- **Codex CLI support** (#44, #49) — `coop codex` launches OpenAI's Codex
  inside the guest, alongside Claude Code. `~/.codex` config and auth are
  staged into the VM, `OPENAI_API_KEY` is forwarded, and MCP servers
  configured under `[codex.mcp_servers]` are merged into the guest's
  `~/.codex/config.toml`. A `codex-yolo` guest alias mirrors the existing
  `claude-yolo` shortcut.

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
