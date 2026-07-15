---
description: Continuously babysit my open PRs — 7-min loop, auto-discovers new PRs, dispatches one background worker per PR needing work.
allowed-tools: Bash(gh auth status:*), Bash(gh api user:*), Bash(gh repo view:*), Bash(gh search prs:*), Bash(gh pr view:*), Bash(gh api graphql:*), Bash(gh api repos:*), Bash(gh run list:*), Bash(git remote:*), Bash(git rev-parse:*), Bash(git fetch:*), Bash(git worktree:*), Bash(mkdir:*), Bash(ls:*), Bash(stat:*), Bash(date:*), Bash(find:*), Bash(head:*), Bash(echo:*)
---

## What this does

Sets up a **continuous loop** (default 7-min cadence) that:

1. Discovers every open PR authored by the current `gh` user in the current repo's `origin` — picks up new PRs automatically, drops merged/closed ones.
2. Triages each PR (CI failures, base divergence, unresolved review feedback) using `/babysit-pr`'s rules.
3. For each PR needing mechanical work, spawns **one background subagent** that runs a single `/babysit-pr` pass on that PR — in an isolated worktree, with a lock file so the next cron fire doesn't double-dispatch.
4. Keeps the main agent **responsive** — orchestration is fast (survey + dispatch); slow work happens out-of-band.
5. Surfaces every **deferred decision** — judgment/scope/design calls it will NOT auto-act on — in a persistent ledger (`$LOCK_DIR/deferred.md`) re-printed in full every sweep.

Re-running is safe — it checks for an existing orchestrator cron and skips re-creation. Durable crons auto-expire after 7 days; re-run to refresh. Use `/babysit-my-prs stop` to cancel.

This wraps `/babysit-pr` (single PR). It NEVER rewrites published history, never force-pushes, never pushes to `main`/`master`, never touches PRs you don't own.

## Argument handling

`$ARGUMENTS` may be: empty → set up with defaults (current user, 7-min cadence); `stop` → cancel the cron + in-flight tasks; `status` → list workers + cron + ledger without changing anything; anything else → report `Unknown arg: <x>. Use empty, 'stop', or 'status'.`

## Preconditions

- gh is authed: !`gh auth status 2>&1 | head -3`
- in a git repo: !`git rev-parse --show-toplevel 2>/dev/null || echo "MISSING — not a git repo"`
- origin points to GitHub: !`git remote get-url origin 2>/dev/null || echo "MISSING — no origin remote"`

If any fail, stop and tell the user. Derive once and reuse:

```bash
GH_USER=$(gh api user --jq .login)
OWNER_REPO=$(gh repo view --json owner,name -q '"\(.owner.login)/\(.name)"')
LOCK_DIR="${CLAUDE_JOB_DIR:-/tmp/babysit-my-prs-$USER}"
mkdir -p "$LOCK_DIR/locks" "$LOCK_DIR/worktrees"
```

The deferred-decision ledger lives at `$LOCK_DIR/deferred.md` and persists across sweeps.

## `status` mode

1. List active locks: `ls -la "$LOCK_DIR/locks/"` — each `pr-N.lock` (mtime < 30 min) is a worker in flight.
2. List the orchestrator cron via `CronList` (prompt starts with `[babysit-my-prs sweep]`).
3. List in-flight tasks via `TaskList`.
4. Print `$LOCK_DIR/deferred.md` in full if present.
5. One-screen summary. Exit.

## `stop` mode

1. `CronList` → find the orchestrator cron(s) → `CronDelete` each.
2. Leave running workers to finish (don't kill mid-push).
3. Optionally remove stale locks: `find "$LOCK_DIR/locks" -name 'pr-*.lock' -mmin +30 -delete`.
4. Report what was stopped. Exit.

## Default mode — set up the orchestrator

### Step 1. Check for an existing orchestrator

`CronList`. If any cron's prompt starts with `[babysit-my-prs sweep]`, do NOT duplicate — report `Orchestrator already running (cron <id>).`, run the initial sweep anyway (Step 3), then exit.

### Step 2. Schedule the recurring cron

Off-minute 7-cycle: `4-59/7 * * * *`. Use `CronCreate` with `recurring: true`, `durable: true`. The cron prompt is the **sweep payload** below — copy it verbatim, substituting `<GH_USER>` and `<OWNER_REPO>`:

---SWEEP PAYLOAD---

```
[babysit-my-prs sweep] Periodic /babysit-pr sweep across <GH_USER>'s open PRs in <OWNER_REPO>. DO NOT create a new cron — the orchestrator is already running.

LOCK_DIR="${CLAUDE_JOB_DIR:-/tmp/babysit-my-prs-$USER}"
DEFERRED_LEDGER="$LOCK_DIR/deferred.md"   # persists across sweeps

1. **Discover** open PRs:
   gh search prs --repo <OWNER_REPO> --author <GH_USER> --state open --json number,title,isDraft,url,updatedAt --limit 200
   If the count equals the limit (200), prepend `WARNING: >=200 open PRs, some not babysat this sweep.` to the report.

2. **Reconcile TaskList:** for each task matching `PR #N`, if `$LOCK_DIR/locks/pr-N.lock` is gone or >30 min old, TaskUpdate to completed. For any task whose PR is no longer open, complete it.

3. **Skip in-flight PRs:** drop any PR with a lock file <30 min old.

4. **Triage candidates** via ONE foreground general-purpose Agent. Brief it to gather, per PR:
   - CI status via `gh run list --repo <OWNER_REPO> --branch <head> --limit 10 --json databaseId,name,conclusion,status` (failing = conclusion in {failure,cancelled,timed_out,action_required}; in-flight = status in {in_progress,queued,pending}). coop CI jobs: check, deny, taplo, zizmor.
   - Inline review threads via GraphQL `reviewThreads(first:50){nodes{id isResolved comments(first:10){nodes{author{login} body createdAt path line}}}}` — unresolved iff `isResolved=false`.
   - Top-level reviews + PR-level comments via `gh pr view <N> --repo <OWNER_REPO> --json reviews,comments`. Parse the CI review bot's structured findings as first-class feedback; a finding is addressed if a later commit references its substance.
   - Base divergence via `gh api repos/<OWNER_REPO>/compare/<base>...<head> --jq '{ahead,behind,status}'`.

   Sort every PR into exactly ONE of three buckets:
   - **WORK NEEDED** (mechanical / auto-actionable): settled CI failure (not a flake or an integration-test failure needing the maintainer's VM host), DIRTY merge resolvable by `git merge --no-ff`, or an unresolved thread whose fix is mechanical (concrete diff handed over, or a plain factual correction). Drafts get CI fixes, base merges, and existing review-comment fixes — NO unprompted deep review.
   - **DEFERRED** (needs the USER's decision — do NOT dispatch): a judgment/scope call — an optional/"non-blocking" suggestion, a design reconcile, a semantic merge conflict, a conflict the author is iterating on, or feedback where the right answer isn't mechanically obvious. Capture: PR number, head SHA, the one-line decision the USER must make, and why it's not auto-actionable.
   - **CLEAN**: green CI, no conflict, no unresolved actionable feedback (approved-awaiting-merge counts).

   Output THREE buckets: `WORK NEEDED: [(N, reason), ...]`, `DEFERRED: [(N, head_sha, decision, why-not-auto), ...]`, `CLEAN: [N, ...]`. Under 700 words.

5. **Dispatch background workers** — one Agent per PR in WORK NEEDED (NEVER for DEFERRED), run_in_background=true. Each worker prompt MUST contain:

   a) **Lock + worktree setup** (verbatim, substituting <N>):
   ```
   LOCK="$LOCK_DIR/locks/pr-<N>.lock"
   if [ -f "$LOCK" ]; then
     AGE=$(( $(date +%s) - $(stat -c %Y "$LOCK") ))
     [ $AGE -lt 1800 ] && { echo "Worker active on PR #<N> ($AGE s)"; exit 0; }
   fi
   date +%s > "$LOCK"
   trap 'find "$LOCK_DIR/locks" -name "pr-<N>.lock" -delete 2>/dev/null' EXIT

   PARENT=$(git -C "$PWD" rev-parse --show-toplevel 2>/dev/null || echo "<repo-root>")
   BRANCH=$(gh pr view <N> --repo <OWNER_REPO> --json headRefName -q .headRefName)
   WORKTREE="$LOCK_DIR/worktrees/pr-<N>"
   git -C "$PARENT" fetch origin "$BRANCH"
   git -C "$PARENT" worktree add --detach "$WORKTREE" "origin/$BRANCH" 2>/dev/null || {
     git -C "$PARENT" worktree remove --force "$WORKTREE" 2>/dev/null
     git -C "$PARENT" worktree prune
     git -C "$PARENT" worktree add --detach "$WORKTREE" "origin/$BRANCH"
   }
   cd "$WORKTREE"
   ```

   b) **Task description** — the specific reason from triage. Be concrete: file paths, line numbers, the substance of the ask. Restate the work.

   c) **`/babysit-pr` guardrails** (copy into the worker prompt — never relax):
   - Integrate base via `git merge --no-ff origin/<base>` ONLY; NEVER rebase.
   - Never `--force` / `--force-with-lease` / `--no-verify`.
   - One logical change per commit.
   - Never push to `main`/`master` or a branch you don't own.
   - The worktree is detached — push with `git push origin HEAD:refs/heads/$BRANCH` so the local branch ref is never touched.
   - Local gate before each push: `cargo fmt -- --check`, `cargo clippy --all-targets --all-features -- -D warnings`, and the narrowest `cargo test`; `taplo format --check` if a .toml changed. Fix only mechanical issues; surface anything semantic.
   - For inline threads (A): when a pushed commit addresses the ask, reply via GraphQL `addPullRequestReviewThreadReply` with body EXACTLY `Fixed in <sha>.` or `Fixed in <sha>: <one-line>.`, then `resolveReviewThread`. NEVER reply on the user's behalf to judgment-call findings.
   - For reviews (B) / PR-level comments (C): the commit message restating the substance is the close-out; optionally one PR-level `Addressed in <sha1>, <sha2>` after the batch.
   - On push rejection: STOP, surface — never force-push. On non-mechanical conflicts or design-judgment findings: STOP, surface.

   d) **Report format** — single line: `Pushed <sha> — <msg>` or `No-op: <reason>` or `Aborted: <reason>`. Under 250 words.

6. **Create tasks** — for each dispatched worker, `TaskCreate(subject="PR #<N> — <one-line goal>", description="<triage reason>: <pr_url>")`.

7. **Update the deferred ledger** at `$DEFERRED_LEDGER`:
   - Read current (absent → empty).
   - PRUNE entries now resolved (PR no longer open, head SHA advanced past the recorded short-sha, or the PR is WORK NEEDED/CLEAN this sweep); collect them as "resolved since last sweep."
   - ADD each DEFERRED item as one line: `- #<N> @<short-sha> — <decision> — WHY NOT AUTO: <reason> — since <YYYY-MM-DDThh:mmZ>`. Keep the original `since` if the line exists and head SHA is unchanged.
   - Rewrite with exactly the still-open deferred set.

8. **Report** — ALWAYS emit both parts every sweep:
   - Status line: `Sweep @ <time>: <X> open, <Y> in flight, dispatched <Z>, deferred <D>, clean <W>.`
   - Deferred block — re-print the FULL current ledger every iteration:
     ```
     ⚠ AWAITING YOUR DECISION (<D>) — I will NOT act on these without your go-ahead:
       • #<N> — <decision> — why I skipped it: <reason>   [since <when>]
     ```
     If empty, print `No deferred decisions.` If anything was pruned, append `Resolved since last sweep: #<N> (<merged | author acted | now clean>).`
```

---END SWEEP PAYLOAD---

### Step 3. Run the initial sweep immediately

Execute the sweep payload now (discovery → triage subagent → worker dispatches → TaskCreates → ledger → report) so the user sees dispatch happen now, not in 7 minutes.

### Step 4. Final report

```
/babysit-my-prs orchestrator running (cron <id>, every 7 min: 4-59/7 * * * *).
Initial sweep: <X> open, <Z> dispatched, <D> deferred, <W> clean, <K> already in flight.
Deferred decisions re-printed every sweep; see $LOCK_DIR/deferred.md.
Auto-expires after 7 days; re-run to refresh. Stop with: /babysit-my-prs stop
```

## Constraints (do not violate)

- Only orchestrate the current `gh` user's own PRs.
- Drafts get CI/rebase + existing review-comment fixes; never unsolicited deep review.
- All worker writes go through `/babysit-pr`'s guardrails. The orchestrator MUST NOT bypass them.
- Workers operate in `$LOCK_DIR/worktrees/pr-<N>` only — never on the parent repo's checked-out branch.
- Locks prevent double-dispatch. Don't dispatch if `pr-N.lock` is <30 min old.
- Don't `CronDelete` the orchestrator unless `$ARGUMENTS == "stop"`.
- Skip terminal PRs (`MERGED`); surface `CLOSED` without dispatching.
- Never modify other people's branches. Never auto-act on a DEFERRED item — record it and re-print the full ledger every sweep.
