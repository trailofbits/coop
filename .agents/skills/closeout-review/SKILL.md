---
name: closeout-review
description: Run the final scope-controlled review before committing, pushing, or opening a PR. Use for closeout review, readiness checks, or self-review of a completed branch.
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

Stop after two non-converging review/fix cycles, if the diff grows beyond about
twice the baseline without approval, or when a local fix has become a redesign.

Report what ran, accepted/fixed findings, rejected findings with a one-line
reason, follow-ups, unrun platform gates, and a `clean` or `blocked` verdict.

On a `clean` verdict, record the reviewed commit for the compatibility PR gate:

```bash
COMMON_DIR="$(git rev-parse --git-common-dir)"
case "$COMMON_DIR" in /*) ;; *) COMMON_DIR="$PWD/$COMMON_DIR" ;; esac
COMMON_DIR="$(cd "$COMMON_DIR" && pwd)"
DIR="$COMMON_DIR/closeout-review"
mkdir -p "$DIR"
git rev-parse HEAD > "$DIR/$(git rev-parse --abbrev-ref HEAD | tr '/' '-')"
```

The marker is valid only for the current commit. Never record it for a blocked
verdict or to bypass review.
