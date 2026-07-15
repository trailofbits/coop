---
description: Single-pass PR shepherding — watch CI, fix failures, address review comments, keep the branch up to date with its base.
allowed-tools: Bash(gh pr view:*), Bash(gh pr diff:*), Bash(gh api graphql:*), Bash(gh api repos:*), Bash(gh run list:*), Bash(gh run view:*), Bash(git fetch:*), Bash(git log:*), Bash(git status:*), Bash(git rev-list:*), Bash(git rev-parse:*), Bash(git diff:*), Bash(git add:*), Bash(git commit:*), Bash(git push:*), Bash(git merge:*), Bash(cargo fmt:*), Bash(cargo clippy:*), Bash(cargo test:*), Bash(cargo check:*), Bash(taplo format:*)
---

## What this does

One pass over the PR for the current branch:

1. **Survey** the PR state once (CI checks, review threads, base divergence, mergeability).
2. **Triage** what's actionable now vs. waiting on something external.
3. **Act** in priority order: base merge / conflicts → CI failures → unresolved review comments.
4. **Report** what changed and **suggest a `/loop` cadence** if work remains.

This shepherds your own open PRs. It does **not** merge, close, or reopen the PR. It **never** rewrites published history — base updates land as merge commits so the integration is visible and auditable. It only writes to this PR's own branch (never to `main`/`master` or branches you don't own).

## Preconditions

Stop and tell the user if any fail:

- Current dir is a git working tree: !`git rev-parse --show-toplevel 2>/dev/null || echo "MISSING — not a git repo"`
- A PR exists for the current branch: !`out=$(gh pr view --json number,state,headRefName 2>/dev/null) && echo "$out" | head -3 || echo "MISSING — no PR for current branch"`
- Working tree is clean: !`git status --porcelain | head -5 | grep . || echo "clean"`

If the worktree is dirty, stop and ask how to proceed — do not stash silently.

## Terminal-state short-circuit

Before the survey, peek at the PR's high-level state:

```bash
gh pr view --json state,reviewDecision
```

If `state` is `MERGED`, report `PR is MERGED — nothing to babysit` and exit. In a `/loop`, tell the user to stop the loop. `reviewDecision == APPROVED` is **not** a short-circuit — keep babysitting until the PR actually merges. `state == CLOSED` (without merge) is not auto-terminal — surface to the user instead of exiting.

## Step 1 — Survey the PR state (once)

```bash
gh pr view --json number,state,isDraft,headRefName,baseRefName,headRefOid,url,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup,title,reviews,comments
git fetch origin "$(gh pr view --json baseRefName -q .baseRefName)" --quiet
git log --oneline "origin/$(gh pr view --json baseRefName -q .baseRefName)..HEAD"
git rev-list --left-right --count "HEAD...origin/$(gh pr view --json baseRefName -q .baseRefName)"
```

Also fetch inline review threads (the resolution source of truth):

```bash
gh api graphql -f query='query($owner:String!,$repo:String!,$n:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$n){reviewThreads(first:50){nodes{id isResolved diffSide comments(first:10){nodes{databaseId path line body author{login}}}}}}}}' -F owner=<owner> -F repo=<repo> -F n=<n>
```

Extract: `pr_number`, `pr_url`, `head_sha`, `base_ref`, `is_draft`, `merge_state`
(`CLEAN`/`DIRTY`/`BEHIND`/`BLOCKED`/`UNSTABLE`), failing checks (`statusCheckRollup`
items with `conclusion` in `{FAILURE, CANCELLED, TIMED_OUT, ACTION_REQUIRED}`),
in-flight checks (`status` in `{IN_PROGRESS, QUEUED, PENDING}`), and unresolved
feedback from three streams:

- **A — inline threads** (`reviewThreads`): unresolved iff `isResolved == false`.
- **B — top-level reviews** (`reviews`): a `CHANGES_REQUESTED` body with no inline comments shows up only here.
- **C — PR-level comments** (`comments`): free-form asks, and structured findings from the CI review bot (a `<K> finding(s) posted inline.` summary or `### file:line` sections). Parse each as first-class actionable feedback.

For B/C (no resolution field), treat as needing action unless: it's a bot
greeting/CI summary with no findings, it's from the PR author, or a commit
**after** the comment's `createdAt` references its substance. When unsure, treat
as unaddressed.

Print a one-screen summary before acting:

```
PR #<n> <title>  (<url>)
state: <OPEN|DRAFT>  merge: <CLEAN|DIRTY|BEHIND|...>  review: <APPROVED|CHANGES_REQUESTED|REVIEW_REQUIRED>
base: <base_ref>  ahead/behind: <a>/<b>
checks: <X passed> / <Y failing> / <Z pending>
unresolved comments: <K>
```

## Step 2 — Decide what to act on

Priority order (act on the first applicable, then re-survey):

1. **Merge base / conflicts** — `merge_state` is `DIRTY`, or base is ahead and the PR has stale CI against an old base.
2. **CI failures** — any check failing.
3. **Unresolved review comments** — reviewer comments the author hasn't addressed.
4. **Nothing actionable** — report and exit (Step 4).

Skip and report (don't act) when: PR is `DRAFT` with no review requested; a
failing check is external/flaky the user owns separately; a failing check has no
fetchable logs; or a comment needs product/design judgment the user hasn't given.

## Step 3 — Act

Each round: make the smallest change that addresses one issue, run the relevant
local gate, commit, push, then return to Step 1 if budget remains.

Local gate (run only what the change touches — not the full suite every time):

```bash
cargo fmt -- --check          # or `cargo fmt` to apply
cargo clippy --all-targets --all-features -- -D warnings
cargo test                    # narrow with `cargo test <name>` when possible
taplo format --check          # only if a .toml changed
```

### 3a. Merge base / resolve conflicts

Integrate `<base_ref>` with a merge commit — never a rebase:

```bash
git fetch origin <base_ref>
git merge --no-ff origin/<base_ref>
```

Resolve conflicts by reading both sides; prefer the PR's semantic intent; never
`--theirs`/`--ours` blindly. Re-run the gate after resolving. If a conflict
touches code outside the PR's scope, stop and surface. Then `git push origin
<head_ref>`. If the push is rejected (someone pushed concurrently), stop and
surface — do **not** force-push.

### 3b. CI failures

For each failing check (coop CI jobs: `check` = fmt/clippy/test + integration-update/uninstall; `deny`; `taplo`; `zizmor`):

1. `gh run view --log-failed <run-id>` (resolve `run-id` from the check's `detailsUrl` or `gh run list --branch <head_ref> --limit 5 --json databaseId,name,conclusion`).
2. Categorize:
   - **fmt** → `cargo fmt`; commit as `style: cargo fmt`.
   - **clippy** → fix the warning properly (coop is zero-warnings); never `#[allow]` to silence unless it's a justified, commented exception.
   - **test** → reproduce with `cargo test <name>`; fix the underlying bug. Do NOT `#[ignore]` or delete a test to make CI pass.
   - **taplo** → `taplo format` the TOML.
   - **deny** → a new/updated dependency tripped an advisory/license/ban/source rule; fix `deny.toml` or the dependency, regenerate `Cargo.lock`.
   - **zizmor** → a workflow-security finding; fix the workflow (pin SHA, `persist-credentials: false`, least-privilege permissions).
   - **integration** → these run real VMs; a failure usually needs the maintainer's environment. Surface with the run URL rather than faking a fix.
   - **flaky / environmental** → do NOT push a fake fix. Surface with the URL and stop.
3. Commit the fix as a focused commit (never mix refactor with behavior change).
4. `git push origin <head_ref>`.

### 3c. Unresolved review comments

For each finding (inline thread, review body, or one parsed section of a structured comment), in file→line order:

1. Read it in context; open the file at the cited line.
2. If it's a clear, mechanical ask (typo, rename, dead-code removal, missing test case, broken doc example): apply it.
3. If it needs a design/judgment call: surface to the user, don't guess.
4. If already addressed by a later commit: leave it.
5. Commit with a message referencing the finding's substance (not the comment ID).

**3c.i — inline threads (Stream A).** After the fix is pushed, reply and resolve. Reply body exactly `Fixed in <sha>.` or `Fixed in <sha>: <one-line>.` — no other prose. Both are GraphQL mutations on the thread node `id`:

- Reply: `gh api graphql -f query='mutation { addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:"<thread_id>", body:"Fixed in <sha>."}) { comment { databaseId } } }'`
- Resolve: `gh api graphql -f query='mutation { resolveReviewThread(input:{threadId:"<thread_id>"}) { thread { isResolved } } }'`

Only reply+resolve when a pushed commit actually addresses the ask. Threads escalated to the user stay unanswered.

**3c.ii — reviews (B) / PR-level comments (C).** No thread to resolve. The commit message restating the finding's substance is the close-out; optionally post one PR-level `Addressed in <sha1>, <sha2>` after the batch.

## Step 4 — Report and suggest a cadence

```
Babysat PR #<n>:
  • <K> change(s) pushed: <one-line per commit, with sha>
  • <Y> failing check(s) remaining
  • <Z> unresolved comment(s) remaining
  • merge state: <state>
```

Then suggest a `/loop` cadence only if there's a concrete signal worth waking for:

| Situation | Suggested cadence |
|---|---|
| CI running, no other work | `/loop 5m /babysit-pr` |
| Awaiting reviewer | `/loop 30m /babysit-pr` |
| Blocked on user judgment | none — wait for the user |
| Clean, approved, mergeable | none — tell the user it's ready to merge |
| Draft with nothing pending | none |

Phrase it as a question — don't start a loop unsolicited.

## Guardrails (do not violate)

- One logical change per commit. Don't bundle a base merge, a fmt fix, and a review-comment fix into one commit.
- Never rewrite published history. No `git rebase` on the PR branch, no `--force`/`--force-with-lease`. Integrate the base via `git merge --no-ff`.
- Never `--no-verify`, never skip hooks.
- Never push to `main`/`master` or a branch you don't own.
- Never close, reopen, or merge the PR. Never dismiss reviews. Never edit the PR title/body unless asked.
- Replies to inline threads must be `Fixed in <sha>.`/`Fixed in <sha>: <one-line>.` and only when a pushed commit addresses the ask.
- PR-level structured findings from the CI review bot are first-class feedback — apply mechanical ones, surface judgment calls.
- If a step needs destructive recovery (`git reset --hard`, deleting unfamiliar files), stop and ask first.
