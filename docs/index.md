# Documentation index

System-of-record map for `coop`. The root [`AGENTS.md`](../AGENTS.md) is the
short navigational entrypoint; durable detail lives here.

## For contributors (engineering)

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, the two-backend design,
  host→guest data flow, architectural invariants.
- [`trust-model.md`](trust-model.md) — trust boundaries, taint sources, secret
  handling, and `coop update` verification. Disclosure policy:
  [`SECURITY.md`](../SECURITY.md).
- [`code-style.md`](code-style.md) — Rust authoring idioms and the review /
  authoring checklists.
- [`testing.md`](testing.md) — integration tests, mutation testing (+
  `.cargo/mutants.toml` scoping and baselines), fuzzing, kani.
- [`user-impact.md`](user-impact.md) — assess user tasks and security or
  reliability trade-offs during implementation and review.
- [`platform-notes.md`](platform-notes.md) — Firecracker CI-kernel workarounds,
  Docker networking, scp `~` caveat, tracing-to-stderr.

## For users

- [`getting-started.md`](getting-started.md) — install and first VM.
- [`commands.md`](commands.md) — every `coop` subcommand.
- [`configuration.md`](configuration.md) — `config.toml` reference.
- [`backends.md`](backends.md) — Lima (macOS) and Firecracker (Linux) setup.
- [`images-and-profiles.md`](images-and-profiles.md),
  [`workspaces.md`](workspaces.md), [`multi-instance.md`](multi-instance.md),
  [`editor.md`](editor.md),
  [`shell-completion.md`](shell-completion.md).
- [`claude-integration.md`](claude-integration.md),
  [`codex-integration.md`](codex-integration.md),
  [`grok-integration.md`](grok-integration.md) — agent integration.
- [`credential-proxy.md`](credential-proxy.md) — opt-in host-side credential
  proxy for Anthropic and OpenAI API keys, Claude setup tokens, and Codex
  workspace tokens.
- [`firecracker-network-modes.md`](firecracker-network-modes.md) — restart-gated
  open and no-NIC isolated networking on Linux/Firecracker.

## Agent tooling

- [`.agents/skills/`](../.agents/skills/) — shared review, closeout, mutation,
  integration, implementation-to-draft-PR, and PR-shepherding workflows.
- [`.claude/`](../.claude/) — compatibility commands, skill entrypoints, and
  local hooks/settings.
