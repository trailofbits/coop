---
name: closeout-review
description: Run the final scope-controlled review before pushing or opening a PR. Use for closeout review, readiness checks, or self-review of a completed branch.
---

# Closeout Review

This is the final gate for a non-trivial change. Read and apply the repository
[`review`](../review/SKILL.md) skill, then resolve its findings locally. Do not
post review comments.

## Freeze scope first

Before reading findings, record the original request, owner/module boundary,
changed files, and non-generated LOC against the base. This is the baseline;
review must not silently redefine the task.

## Classify every validated finding

- **In-scope blocker:** introduced by this diff, same owner boundary, and
  fixable without reframing the task. Fix now.
- **Follow-up:** real but adjacent, optional, or a separate cleanup/bug class.
  Record it; do not absorb it.
- **Stop and escalate:** requires a new contract, protocol, storage/API change,
  security confirmation, or product/design decision. Ask the user.

When uncertain between blocker and follow-up, use follow-up. Findings are
hypotheses: independently verify them before changing code.

## Converge

Apply only in-scope blockers. Run the narrow test for each fix, then format,
clippy with `-D warnings`, and the applicable broader tests. Use the
[`mutation-check`](../mutation-check/SKILL.md) skill for scoped logic and the
[`integration`](../integration/SKILL.md) skill for guest-visible/lifecycle
changes. Re-run the review skill against the complete updated diff, not only the
last fix commit.

For changes to platform `#[cfg]` gates, backend aliases, or platform-only
types/call paths, run check and clippy for every affected OS, natively or with
an available cross target. Verify the ordinary non-test library build as well
as tests: `--all-targets` does not select other operating systems, and
`cfg(test)` can hide unused production variants. Report unavailable targets as
unrun platform gates.

Stop after two non-converging review/fix cycles, if the diff grows beyond about
twice the baseline without approval, or when a local fix has become a redesign.

Report what ran, accepted/fixed findings, rejected findings with a one-line
reason, follow-ups, unrun platform gates, and a `clean` or `blocked` code-review
verdict. Report validation separately as complete, incomplete, or failed. An
unavailable test does not turn an otherwise clean code review into a passed
validation gate. Apply the [user-impact guidance](../../../docs/user-impact.md)
when a material decision remains unresolved.

Iterate on the uncommitted diff as needed. After committing fixes, run one final
full-diff review of the branch tip intended for the PR. Record its verdict in
one marker. After a blocked verdict, open only a **draft** PR to carry unresolved
findings or decisions for discussion. The compatibility hook checks that the
commit was reviewed; it does not enforce PR readiness.
Never write the marker before the final review or for an uncommitted diff:

```bash
VERDICT=clean # or blocked, matching the reported code-review verdict
COMMON_DIR="$(git rev-parse --git-common-dir)"
case "$COMMON_DIR" in /*) ;; *) COMMON_DIR="$PWD/$COMMON_DIR" ;; esac
COMMON_DIR="$(cd "$COMMON_DIR" && pwd)"
DIR="$COMMON_DIR/closeout-review"
mkdir -p "$DIR"
printf '%s %s\n' "$(git rev-parse HEAD)" "$VERDICT" > "$DIR/$(git rev-parse --abbrev-ref HEAD | tr '/' '-')"
```

The marker is valid only for that commit. Writing the latest verdict replaces
any earlier verdict for the same commit. A clean code-review verdict does not
claim that every validation gate ran. Before marking an existing draft PR
ready, repeat the closeout and validation checks; the PR creation hook does
not guard that later transition.
