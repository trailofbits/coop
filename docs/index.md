# Documentation index

System-of-record map for `coop`. The root [`CLAUDE.md`](../CLAUDE.md) is the
short navigational entrypoint; durable detail lives here.

## For contributors (engineering)

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, the two-backend design,
  host→guest data flow, architectural invariants.
- [`trust-model.md`](trust-model.md) — trust boundaries, taint sources, secret
  handling, `coop update` verification. The authoritative security spec the
  `review-security` agent reads. (Disclosure policy is [`SECURITY.md`](../SECURITY.md).)
- [`code-style.md`](code-style.md) — Rust authoring idioms and the review /
  authoring checklists.
- [`testing.md`](testing.md) — integration tests, mutation testing (+
  `.cargo/mutants.toml` scoping and baselines), fuzzing, kani.
- [`platform-notes.md`](platform-notes.md) — Firecracker CI-kernel workarounds,
  Docker networking, scp `~` caveat, tracing-to-stderr.

## For users

- [`getting-started.md`](getting-started.md) — install and first VM.
- [`commands.md`](commands.md) — every `coop` subcommand.
- [`configuration.md`](configuration.md) — `config.toml` reference.
- [`backends.md`](backends.md) — Lima (macOS) and Firecracker (Linux) setup.
- [`images-and-profiles.md`](images-and-profiles.md),
  [`workspaces.md`](workspaces.md), [`multi-instance.md`](multi-instance.md),
  [`devcontainer.md`](devcontainer.md), [`vscode.md`](vscode.md),
  [`shell-completion.md`](shell-completion.md).
- [`claude-integration.md`](claude-integration.md),
  [`codex-integration.md`](codex-integration.md) — agent integration.
- [`json-output-design.md`](json-output-design.md) — the `--json` design
  (a good precedent for design-doc style).

## Agent tooling (`.claude/`)

- `agents/review-*.md` — single-lens PR review sub-agents.
- `commands/` — `my-review`, `babysit-pr`, `babysit-my-prs`, `integration`.
- `skills/` — `closeout-review` (pre-PR gate), `mutation-check`.
- `hooks/` + `settings.json` — the closeout gate, `cargo fmt` formatter, the
  no-pipe-test-output guard, and the read-only Bash allowlist.
