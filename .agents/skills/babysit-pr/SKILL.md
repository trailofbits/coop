---
name: babysit-pr
description: Shepherd the current user's open PR through base updates, CI failures, and review feedback without rewriting history or merging. Use when asked to babysit, monitor, or fix an existing PR.
---

# Babysit PR

One pass surveys the PR, acts on safe mechanical work, then reports. Do not
merge, close, reopen, dismiss reviews, rewrite published history, or edit PR
metadata unless asked.

## Preconditions and survey

Require an authenticated `gh`, a clean git worktree, an open PR for the current
branch, and ownership of its head branch. Stop on a dirty tree rather than
stashing. A merged PR is terminal; a closed-unmerged PR needs user direction.

Gather once:

- `gh pr view` with state, draft status, head/base refs and SHAs, merge state,
  review decision, check rollup, reviews, and comments;
- base divergence after fetching the base;
- inline `reviewThreads` through GraphQL (resolution is authoritative);
- failed/pending run details and logs.

Treat top-level `CHANGES_REQUESTED` reviews and structured bot comments as
feedback even when no inline thread exists. Do not assume a green-looking list
is complete: compare the visible checks with the repository's required CI jobs.

## Act in priority order

1. Integrate a behind/conflicting base with `git merge --no-ff
   origin/<base>`. Never rebase. Resolve semantically; stop if the conflict
   crosses scope.
2. Reproduce settled CI failures. Fix the underlying fmt, clippy, test, taplo,
   deny, or workflow issue. Surface flaky/environmental and real-VM integration
   failures rather than fabricating a code fix.
3. Address unresolved review feedback only when the ask is concrete and
   mechanical. Verify the claim first. Escalate design, product, security, and
   scope decisions.

Make one logical change per commit. Run the narrow test plus format and clippy;
run taplo for TOML. Never use `--no-verify`. Push normally and stop on rejection
rather than force-pushing.

After a pushed commit fixes an inline thread, reply exactly `Fixed in <sha>.` or
`Fixed in <sha>: <one-line>.`, then resolve that thread. Do not reply or resolve
before the fix is pushed, and never close a judgment-call thread for the user.

Re-survey once after changes. Report pushed commits, failed/pending checks,
unresolved feedback, merge state, and absent integration/platform gates. If
only an external signal remains, offer to monitor at an appropriate cadence;
do not start recurring monitoring unless the user asked.
