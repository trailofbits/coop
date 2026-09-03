---
name: babysit-my-prs
description: Triage and shepherd all open PRs owned by the current GitHub user, isolating each writable worker in its own worktree. Use when asked to babysit or monitor the user's PR fleet.
---

# Babysit My PRs

Authenticate `gh`, derive the current user and origin repository, then discover
all open PRs authored by that user. Never touch another author's branch.

For each PR, collect CI state, required-job presence, base divergence, merge
state, inline review threads, top-level reviews, and PR comments. Classify it as:

- **Work needed:** a mechanical base merge, settled CI fix, or concrete review
  correction.
- **Deferred:** a semantic conflict, optional suggestion, integration-host
  failure, scope/security/design judgment, or ambiguous feedback.
- **Clean:** no actionable feedback, required checks present and green, and no
  conflict.

When parallel agents are available and the user requested action, delegate one
worker per work-needed PR. Each worker must use a separate detached worktree,
acquire a per-PR lock, and follow [`babysit-pr`](../babysit-pr/SKILL.md). Push
with `git push origin HEAD:refs/heads/<branch>`; never update the parent
checkout's branch ref. Do not dispatch deferred items.

If recurring monitoring is available and requested, schedule a sweep with a
non-overlapping cadence, keep lock/deferred state outside the repository, and
run an initial sweep immediately. If the environment has no scheduler, perform
one sweep and state that limitation; do not emulate a daemon with a blocking
sleep loop.

Every sweep reports open, in-flight, dispatched, deferred, and clean counts and
prints the full deferred-decision ledger. Stop recurring monitoring only when
asked. Running workers may finish, but no new workers should be dispatched
after stop.
