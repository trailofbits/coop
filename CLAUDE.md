# coop — Claude / Codex / contributor guide

Isolated VM environment for running Claude Code and Codex — Firecracker on
Linux, Lima on macOS.

## Agent entrypoint

Shared entrypoint for Claude, Codex, and humans. Keep this short and
navigational; durable detail lives in [`docs/`](docs/).

- [`docs/index.md`](docs/index.md) — system-of-record map.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — module map, the two-backend
  design, host→guest data flow, architectural invariants.
- [`docs/trust-model.md`](docs/trust-model.md) — trust boundaries and taint
  sources (the security spec; read before touching secrets, subprocess,
  network, or the guest boundary).
- [`docs/code-style.md`](docs/code-style.md) — Rust authoring idioms + review /
  authoring checklists.
- [`docs/testing.md`](docs/testing.md) — integration, mutation, fuzzing, kani.
- [`docs/platform-notes.md`](docs/platform-notes.md) — CI-kernel workarounds,
  Docker networking, scp `~` caveat, tracing-to-stderr.
- Review tooling: `.claude/agents/review-*.md`, `.claude/commands/`,
  `.claude/skills/`.

## Architecture (one paragraph)

A Rust CLI that orchestrates VM lifecycle (setup → up/start → shell → stop →
destroy → status/logs). Two backends are selected at **compile time** by
`#[cfg]` behind the `backend::VmBackend` trait / `PlatformBackend` alias:
Firecracker microVMs on Linux (KVM), Lima VMs on macOS
(Virtualization.framework). Everything above the trait — SSH, workspace sync,
config/secret injection, agent bootstrap, `commands/` — is backend-shared and
must hold for both. Full detail: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Trust model

**The VM is the isolation boundary.** The guest is deliberately permissive
(passwordless sudo, agents run in bypass mode) because the whole VM is the blast
radius — so the guest is **untrusted from the host's view**, and guest-authored
data must never escalate into host code execution, filesystem escape, or
credential exposure. Authoritative boundaries and the stop-and-confirm checklist:
[`docs/trust-model.md`](docs/trust-model.md). This is the engineering spec;
[`SECURITY.md`](SECURITY.md) remains the vulnerability-disclosure policy.

Stop and confirm before merging code that adds an outbound URL / network
listener / egress rule, forwards a new secret into the guest, runs a subprocess
on tainted bytes, writes a host path from tainted data, logs/traces tainted or
secret content, or softens the `coop update` verification chain.

## Development commands

Runtime: Rust `1.94.0` (see `rust-toolchain.toml`), edition 2024.

```bash
cargo build                                                    # debug build
cargo fmt -- --check                                           # format check
cargo clippy --all-targets --all-features -- -D warnings       # lints (zero-warnings)
cargo test                                                     # unit tests (lib)
cargo deny check                                               # advisories/licenses/bans
taplo format --check                                           # TOML formatting
prek run                                                       # all pre-commit hooks
```

Install pinned local dev tools (prek, taplo, cargo-deny, cargo-mutants,
cargo-fuzz, kani) with `./scripts/install-dev-tools.sh --all`, then `prek
install`. CI pins its own taplo/cargo-deny versions in
`.github/workflows/ci.yml`; keep those in sync with the installer.

## Before committing

Pre-commit hooks (prek) run automatically: `cargo fmt`, `cargo clippy`, `cargo
test`, `taplo format --check`, plus trailing-whitespace / EOF / large-file /
merge-conflict checks. After hooks pass, run the integration suite on **both
platforms** — too slow for hooks:

```bash
./tests/run-integration.sh                       # local (macOS/Lima)
./tests/run-integration.sh --remote user@host    # remote (Linux/Firecracker)
```

The `/integration` command wraps this; [`docs/testing.md`](docs/testing.md) has
the full testing reference (including the `.cargo/mutants.toml` mutation scoping
and the [`mutation-check`](.claude/skills/mutation-check/SKILL.md) skill).

## Code style

Follow the global Rust guidance (clippy lint policy, `thiserror`/`anyhow`,
`tracing`, newtypes, enums over bools) plus coop's own conventions in
[`docs/code-style.md`](docs/code-style.md): parse-don't-validate at boundaries,
smart-constructor newtypes, type-state for lifecycles, absolute imports only,
tracing to **stderr**. Prefer changing a type to make a bug unrepresentable over
adding a runtime check.

## Pull requests

- **One PR = one logical change.** Refactors/renames first, then behavior —
  never mixed. Split if the description needs "and" / unrelated bullets.
- Run the gates before opening: `cargo fmt -- --check`, `cargo clippy … -D
  warnings`, `cargo test`, and the integration suite on both platforms for
  guest-visible or lifecycle changes.
- Keep cross-file infra in sync in the same PR — a new CLI flag/config field ↔
  `config.example.toml` + `docs/`; tool-version pins ↔ CI; a new shell-out/IO
  function in a scoped module ↔ `.cargo/mutants.toml`.
- Before opening, run the [`closeout-review`](.claude/skills/closeout-review/SKILL.md)
  skill on the working diff (a `PreToolUse` hook gates `gh pr create` on it).
  Describe what the code does now — plain, factual language; a bug fix is a bug
  fix.
