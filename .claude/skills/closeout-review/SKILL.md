---
name: closeout-review
description: Use when finishing a change and about to commit, push, or open a PR — to run a final closeout review on the working diff and decide what to fix WITHOUT scope-creeping the change into a refactor, migration, or redesign. Triggers: "review before I commit", "closeout review", "is this ready to ship", "self-review this branch", "what should I fix before the PR", running this on a loop over a branch.
---

# Closeout Review

## Overview

A closeout review is the **final gate** before you commit, push, or open a PR.
It runs coop's review machinery (`/my-review`) against your working diff, then
applies three disciplines an unguided review skips:

1. **Advisory, not authoritative** — every finding is a *hypothesis*. Verify it
   against real code before acting. Reject speculative risks and unreal edges.
2. **Scope governance** — freeze what this change is *supposed* to do first.
   Findings outside that boundary become follow-ups, not edits.
3. **Convergence** — patch-and-re-review converges fast or you stop and report.
   It must not spiral into an open-ended rework.

**Core principle:** the review's job is to find what is *wrong with this diff*,
not to expand the diff. A clean closeout means "this change is correct and in
scope," not "this file is now perfect."

## When to use

- Before `git commit` on a non-trivial change, before pushing, or before a PR.
- When self-reviewing a branch and deciding blocker vs. follow-up.
- On a loop over a branch as you iterate toward "ready."

**When NOT to use:**

- CI / posting inline comments on a GitHub PR → use `/my-review` directly (it
  owns the comment posting and read-only contract). Closeout reuses
  `/my-review`'s engine (§3) but resolves findings locally instead of posting.
- A reviewer-style read of someone else's PR → use `/review`.
- Trivial one-line / docs-typo changes — the gate is overhead.

## Workflow

### 1. Freeze the scope baseline (before reviewing)

Write down, in one place, what this change *is*:

- The original request / intended behavior.
- The **owner boundary** — which module(s) and surface this change belongs to.
- `git diff --stat` against the base: changed files + non-test, non-generated
  LOC (`Cargo.lock` and completions don't count).

Freeze this *before* you see findings, so the review can't silently redefine the
task.

### 2. Determine the target diff

In priority order:

- Uncommitted work → `git diff` (and `git diff --staged`).
- A branch → diff against its base: open PR base (`gh pr view --json
  baseRefName`) if one exists, else `origin/main`.
- A specific commit → `git show <ref>` / `git diff <ref>^..<ref>`.

Do not auto-fetch new history; review what is local. Empty diff → stop.

### 3. Run the review engine

Run `/my-review` against the target diff — don't hand-roll the agent dispatch.
`/my-review` builds the shared context, selects the relevant `review-*`
subagents from the diff's trigger map, runs them in parallel, and
batch-validates. Reuse it instead of re-deciding which agents to run.

`/my-review`'s last step posts findings as inline PR comments — that does
**not** apply to a pre-PR closeout. Take its validated findings as input and
resolve them locally through the scope governance below; post nothing.

### 4. Verify each finding independently

**Treat review output as advisory. Never blindly apply it.** Before accepting a
finding, confirm all of:

- **Diff-introduced & grounded** — the flagged line was added/changed by this
  diff (or this diff made a latent bug reachable), and the claim is backed by
  *quoted* code, not a paraphrase.
- **Not already handled** — a guard, a newtype constructor, or a type doesn't
  already cover it.
- **Dependency contract checked** — for any claim about a crate/API, read the
  actual signature/docs, not memory.
- **Fix stays simple** — the fix doesn't overcomplicate the code or add a
  phantom feature to satisfy a hypothetical.

Drop findings that fail any check. When you reject a real-but-intentional finding
(a deliberate invariant or ownership decision — coop has several, e.g. the
passphrase-less VM key, `bypassPermissions` in the guest), leave a one-line note
so the next reviewer doesn't re-raise it.

### 5. Classify with the Scope Governor

Every surviving finding gets exactly one label:

| Label | Meaning | Action |
|-------|---------|--------|
| **In-scope blocker** | Introduced by this diff, same owner boundary, fixable without reframing the task. | Fix now. |
| **Follow-up** | Real, but a different bug class / adjacent surface / cleanup track. | Note it (TODO, issue, PR description); do **not** fix here. |
| **Stop-and-escalate** | Needs a new contract, protocol/storage/API change, or a design decision outside this change. | Stop. Surface to the human. Don't absorb it. |

When in doubt between blocker and follow-up, it's a follow-up.

### 6. Patch, re-test, re-review (converge)

- Apply only **in-scope blockers**.
- Re-run the focused gate for what you touched — the narrowest relevant
  `cargo test <name>`, plus `cargo clippy --all-targets --all-features -- -D
  warnings` and `cargo fmt -- --check` (and `taplo format --check` if a `.toml`
  changed). If the change is guest-visible or touches the VM lifecycle, note
  that `tests/integration.sh` should run on both backends before merge (see
  [`docs/testing.md`](../../../docs/testing.md)) — you generally can't run it
  here, so flag it as a pre-merge requirement.
- Re-review the new diff. Repeat until no in-scope blockers remain.

## Stop conditions (hard)

Pause and report — do not keep editing — when any hit:

- **Two cycles without convergence.**
- **Diff grows past ~2× the baseline files / LOC** without explicit approval.
- **The narrow change is turning into** an architecture change, a config-schema
  migration, a broad refactor, or a release-process change.
- **The best fix is "define the canonical contract first"** rather than a local
  inference — that's a design decision, not a closeout edit.

## Final report

Always close with a short report:

- **What ran** — the review agents and the gate command(s) you re-ran.
- **Findings** — accepted (and fixed) vs. rejected (one-line reasoning each),
  and any follow-ups / escalations carried out of scope.
- **Verdict** — `clean` (no in-scope blockers; ready) or `blocked` (with the
  reason and the stop condition that fired).

### Record the gate marker (only on a `clean` verdict)

A `PreToolUse` hook (`closeout-review-gate.sh`) blocks `gh pr create` until this
marker exists for the current branch at the current commit. When — and only when
— the verdict is `clean`, record it as the last step. Run this from the worktree
you reviewed; it stores the marker under the shared git common dir so the gate
finds it even though the hook runs from the main checkout:

```bash
COMMON_DIR="$(git rev-parse --git-common-dir)"
case "$COMMON_DIR" in /*) ;; *) COMMON_DIR="$PWD/$COMMON_DIR" ;; esac
COMMON_DIR="$(cd "$COMMON_DIR" && pwd)"
DIR="$COMMON_DIR/closeout-review"
mkdir -p "$DIR"
git rev-parse HEAD > "$DIR/$(git rev-parse --abbrev-ref HEAD | tr '/' '-')"
```

The marker is bound to the reviewed commit. If you commit again afterward, the
gate re-arms and the review must be re-run. Never write the marker to bypass a
`blocked` verdict — that defeats the gate.

## Common mistakes

| Mistake | Fix |
|---------|-----|
| Applying findings verbatim | They're hypotheses — verify against real code first (§4). |
| Letting the review redefine the task | Freeze the scope baseline *before* reviewing (§1). |
| Fixing every nit "while I'm here" | Nits in adjacent code are follow-ups, not blockers (§5). |
| Looping forever on "one more finding" | Two-cycle rule and 2× growth are hard stops. |
| Re-implementing review from scratch | Run `/my-review`'s engine (§3). |
