# coop — agent and contributor guide

Isolated VM environment for running Codex and Claude Code — Firecracker on
Linux, Lima on macOS.

## Agent entrypoint

Shared entrypoint for coding agents and humans. Keep this short and
navigational; durable detail lives in [`docs/`](docs/).

- [`docs/index.md`](docs/index.md) — system-of-record map.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — module map, the two-backend
  design, host→guest data flow, architectural invariants.
- [`docs/trust-model.md`](docs/trust-model.md) — trust boundaries and taint
  sources (read before touching secrets, subprocesses, network, or the guest
  boundary).
- [`docs/code-style.md`](docs/code-style.md) — Rust authoring idioms and review
  checklists.
- [`docs/testing.md`](docs/testing.md) — integration, mutation, fuzzing, kani.
- [`docs/platform-notes.md`](docs/platform-notes.md) — CI-kernel workarounds,
  Docker networking, scp `~` caveat, tracing-to-stderr.
- Agent workflows: [`.agents/skills/`](.agents/skills/).

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
radius. The guest is therefore **untrusted from the host's view**, and
guest-authored data must never escalate into host code execution, filesystem
escape, or credential exposure. Authoritative boundaries and the
stop-and-confirm checklist: [`docs/trust-model.md`](docs/trust-model.md).
[`SECURITY.md`](SECURITY.md) remains the vulnerability-disclosure policy.

Stop and confirm before merging code that adds an outbound URL, network
listener, or egress rule; forwards a new secret into the guest; runs a
subprocess on tainted bytes; writes a host path from tainted data; logs or
traces tainted/secret content; or softens the `coop update` verification chain.

## Development commands

Runtime: Rust `1.94.0` (see `rust-toolchain.toml`), edition 2024.

```bash
cargo build
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check
taplo format --check
prek run
```

Install pinned local dev tools (prek, taplo, cargo-deny, cargo-mutants,
cargo-fuzz, kani) with `./scripts/install-dev-tools.sh --all`, then `prek
install`. CI pins its own taplo/cargo-deny versions in
`.github/workflows/ci.yml`; keep those in sync with the installer.

## Before committing

Pre-commit hooks run automatically: `cargo fmt`, `cargo clippy`, `cargo test`,
`taplo format --check`, plus whitespace, EOF, large-file, and merge-conflict
checks. After hooks pass, run the integration suite on **both platforms** for
guest-visible and lifecycle changes:

```bash
./tests/run-integration.sh                       # local (macOS/Lima)
./tests/run-integration.sh --remote user@host    # remote (Linux/Firecracker)
```

Use the [`integration`](.agents/skills/integration/SKILL.md) skill to run and
interpret it. [`docs/testing.md`](docs/testing.md) has the full testing
reference, including mutation scoping and the
[`mutation-check`](.agents/skills/mutation-check/SKILL.md) skill.

## Code style

Follow the global Rust guidance (clippy lint policy, `thiserror`/`anyhow`,
`tracing`, newtypes, enums over bools) plus coop's conventions in
[`docs/code-style.md`](docs/code-style.md): parse-don't-validate at boundaries,
smart-constructor newtypes, type-state for lifecycles, absolute imports only,
and tracing to **stderr**. Prefer changing a type to make a bug unrepresentable
over adding a runtime check.

## Pull requests

- **One PR = one logical change.** Put refactors/renames before behavior, never
  in the same change. Split if the description needs unrelated bullets.
- Run the gates before opening: format, clippy with zero warnings, unit tests,
  and both integration backends for guest-visible or lifecycle changes.
- Keep cross-file representations in sync: CLI/config ↔ examples and docs;
  tool pins ↔ CI; logic-module shell/IO functions ↔ `.cargo/mutants.toml`.
- Before opening, use the
  [`closeout-review`](.agents/skills/closeout-review/SKILL.md) skill on the
  working diff. Describe what the code does now in plain, factual language.

## Review discipline

The [`review`](.agents/skills/review/SKILL.md) skill is the canonical review
workflow. In addition to lens-specific checks, every review must:

- Treat each finding and each author claim as a hypothesis. Reproduce or inspect
  the real behavior when possible; check the pinned dependency/tool version.
- Mutation-check or deliberately break new tests and tripwires. An assertion
  that still passes after removing the promised behavior is not coverage.
- Distinguish observed facts from inferred causes in errors, docs, and review
  comments. Preserve distinct failure states when later messaging depends on
  them; prefer an enum over discarding the reason and reconstructing it.
- Audit lifecycle symmetry: success, partial failure, timeout, retry, cleanup,
  concurrency, stale state, and mode transitions. A spawned process, secret,
  lock, PID file, or cache must remain bounded on every path.
- Search every representation of a changed contract, including source, tests,
  examples, exhaustive docs, workflow/install scripts, comments, and PR text.
  Re-check the full branch after rebases and review-fix commits.
- Confirm required CI jobs are present, not merely that the visible checks are
  green. State which platform/integration gates were not run.
