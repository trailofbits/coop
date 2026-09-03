# Documentation index

System-of-record map for `coop`. The root [`AGENTS.md`](../AGENTS.md) is the
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
  [`devcontainer.md`](devcontainer.md), [`editor.md`](editor.md),
  [`shell-completion.md`](shell-completion.md).
- [`claude-integration.md`](claude-integration.md),
  [`codex-integration.md`](codex-integration.md) — agent integration.
- [`credential-proxy.md`](credential-proxy.md) — the opt-in `[proxy]`
  credential-injecting proxy (issue #411): keeps the raw API key out of the
  guest.
- [`json-output-design.md`](json-output-design.md) — the `--json` design
  (a good precedent for design-doc style).

## Agent tooling

- [`.agents/skills/`](../.agents/skills/) — shared review, closeout, mutation,
  integration, and PR-shepherding workflows discovered by Codex.
- [`.github/workflows/codex-review.yml`](../.github/workflows/codex-review.yml)
  — trusted-user, on-demand `@codex` review in a read-only sandbox.
- [`.claude/`](../.claude/) — compatibility commands, skill entrypoints, and
  local hooks/settings.
