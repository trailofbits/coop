---
name: mutation-check
description: Use when a change touches a logic-dense coop module (config, workspace, devcontainer, github_*, secret_store, fs_util, commands/*, model_state, jsonc) to run cargo-mutants correctly AND verify .cargo/mutants.toml was updated in the same PR. Triggers: "run mutation testing", "check mutants", "did I break the mutation baseline", before merging a change to a scoped module, before refactoring one.
---

# Mutation Check

## Overview

coop uses [`cargo-mutants`](https://mutants.rs/) as a manual quality gate: it
mutates the code and checks whether the unit tests notice. A **surviving mutant**
(`missed`) is a unit test that would still pass with the code broken — a real
coverage gap. This skill runs the sweep correctly and, critically, catches the
**documented failure mode**: adding a new shell-out / IO / backend / TTY function
without updating `.cargo/mutants.toml`, which silently breaks the "0 missed"
baseline (this is exactly what happened in #352, surfaced late in #373).

Read [`docs/testing.md`](../../../docs/testing.md) for the full scoping model
and baselines — this skill is the operational checklist, that doc is the
reference.

## When to use

- A change adds or edits logic in a scoped module: `config.rs`, `workspace.rs`,
  `devcontainer.rs`, `guest_env_state.rs`, `github_repo.rs`, `github_pat.rs`,
  `secret_store.rs`, `fs_util.rs`, `src/commands/*`, `model_state.rs`, `jsonc.rs`.
- Before refactoring one of those (capture survivors first to know what behavior
  isn't pinned down).

**When NOT to use:** changes only to `backend.rs`, `lima.rs`, `setup.rs`,
`update.rs`, `ssh.rs`, `vm.rs`, `network.rs`, `port_forward.rs`, `cmd.rs`,
`prompt.rs`, `main.rs`, or the `cmd_*`/`&PlatformBackend`/TTY handlers — these
are scoped out because a `--lib` test structurally can't reach them; the
integration suite covers them. Don't burn minutes mutating them.

## Workflow

### 1. Sync the scope config in the SAME PR (do this first)

Before running anything, diff the change and ask: does it add a function that
**shells out, drives a `&PlatformBackend`, reads a TTY, or writes stdout** inside
a logic module?

- **Yes** → the function must be scoped out in `.cargo/mutants.toml` in this same
  PR — either an `exclude_re` line (`\b`-anchored to the function name or
  `Type::method`) or, for a `..Default::default()` field-deletion mutant that
  `exclude_re` can't reach, an in-source `#[mutants::skip]` with a back-reference
  to the note in `.cargo/mutants.toml`. **And** extract any pure logic the new
  function contains into a separate, kept, unit-tested helper (the pattern the
  #321–#327 fixes established). Do this before merging — it is not a follow-up.
- **No / pure logic** → leave it in scope; it needs a unit test that kills its
  mutants.

If you edited `.cargo/mutants.toml`, sanity-check the list resolves:

```bash
cargo mutants --list -f <touched file>
```

### 2. Run the sweep (scoped, `--lib`)

Always scope with `-f` and pass `-- --lib` (all logic is in the library crate;
`-- --bins` runs zero tests and reports every mutant as missed):

```bash
# The touched files
cargo mutants -f src/<file>.rs -- --lib

# PR-scoped alternative: only lines changed vs main
cargo mutants --in-diff <(git diff origin/main -- 'src/*.rs') -- --lib
```

**Prefer the full-file `-f` sweep over `--in-diff` for the touched files** — the
in-diff sweep only mutates changed lines, so it misses pre-existing same-class
survivors in a file you touched. Redirect output to a file (a `PreToolUse` hook
enforces this for cargo-mutants); the run takes minutes.

### 3. Read `mutants.out/` and handle survivors

- `caught.txt` — killed (good). `unviable.txt` — broke the build (ignore).
  `timeout.txt` — hung (rare; `jsonc.rs` scanner-index mutants live here, not a
  gap). `missed.txt` — the survivors to triage.

For each line in `missed.txt`, classify (see `docs/testing.md` for detail):

1. **Real test gap** → add a test that asserts the actual value (not "it didn't
   panic") so it distinguishes the mutant. Re-run the file to confirm the kill.
2. **Equivalent mutant** → behavior no caller can observe (`Display` returning a
   default, a getter whose default matches the real value). `#[mutants::skip]`
   with a one-line reason.
3. **Dead code** → delete it (per "replace, don't deprecate").

A kill rate of 70–80% on viable mutants is healthy. But for the scoped logic
modules the documented baseline is **0 missed** — treat any new survivor there
as a coverage regression, not noise. First confirm it isn't a shell-out/IO
function that belongs in `.cargo/mutants.toml`, then add the test.

### 4. Report

Close with: which files were swept, `missed` count before/after, what you did
with each survivor (test added / skipped-equivalent / deleted), and whether
`.cargo/mutants.toml` needed (and got) an update in this PR.
