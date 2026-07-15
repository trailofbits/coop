---
description: Review a pull request with self-validated findings, posted as inline comments.
---

Review the diff between a PR's base branch and its head, dispatching the
`review-*` sub-agents and posting validated findings as inline review comments.

This is a **read-and-report review only**. Do NOT commit, push, merge, or modify
the PR's code.

When run from CI (the `claude-review` workflow), the calling prompt provides
`REPO` and `PR NUMBER`. When run locally, resolve the current branch's PR with
`gh pr view`.

Design goals:

- Build the review context **once** and share it with every agent, so agents
  don't each re-discover the diff, changed files, CLAUDE.md, and prior feedback.
- **Conditionally skip** agents whose domain the diff doesn't touch.
- **Batch validation by file.**

## 0. Determine the diff

Resolve the base ref from `gh pr view <n> --json baseRefName` and `git fetch
origin <base>` if its tip isn't local. The diff range is `<base>...HEAD`.

```bash
gh pr view <n> --json number,baseRefName,headRefOid,title,url
git fetch origin <base> --quiet
git diff --stat <base>...HEAD
```

If the diff is empty, stop.

## 1. Build the review context packet (once)

`coop` has no packet-builder script — assemble the context inline, once, and
pass it verbatim to every agent. Gather:

- **Diff**: `git diff <base>...HEAD` (trim generated files — `Cargo.lock`,
  `completions/*`, snapshots — to a name + line count; note them as omitted).
- **Changed files** and, for each changed `src/*.rs`, the touched function
  bodies (read the post-change file, not just the hunk).
- **Guidance**: root `CLAUDE.md`, and the relevant `docs/` — `code-style.md`
  (conventions/design), `trust-model.md` (security), `testing.md` +
  `.cargo/mutants.toml` (tests), `ARCHITECTURE.md` (structure).
- **Prior PR feedback**: `gh pr view <n> --json reviews,comments` and the inline
  review threads via GraphQL `reviewThreads` (id, isResolved, path, line, body).
- **Trigger map**: from the changed paths and diff content, note which domains
  are touched — security-relevant surfaces (secrets, subprocess, network,
  filesystem writes, SSH, update), external crates (from `use`/`Cargo.toml`
  changes), tests, docs, comments.

This packet is the sole input to every agent below. Agents may read more files
if needed, but must treat packet content as context, not proof — final
validation re-opens the post-change files.

## 2. Decide which agents to run

Agent definitions live in `.claude/agents/review-*.md`. Each embeds its own
lens, output format, and framing (fresh eyes, packet-as-truth, diff-introduced
findings only, prior-feedback cross-reference). This section governs only which
to invoke.

Always run: `review-correctness`, `review-design`, `review-conventions`.

Conditionally run:

- `review-security` — if the trigger map hits secrets, subprocess/`Command`,
  network/binds, filesystem writes, SSH/scp, guest↔host data flow, `coop update`,
  or `cmd:` config evaluation.
- `review-api-usage` — if the diff changes `use` statements, `Cargo.toml`/
  `Cargo.lock`, or external-crate call sites.
- `review-tests` — unless the packet marks the diff docs-only or config-only
  with no behavior change.
- `review-docs` — if docs changed, doc-comments changed, `--help`/clap text
  changed, or a touched symbol is named in `docs/`/README.
- `review-comments` — if the diff adds or changes inline `//` comments, or
  changes code adjacent to existing comments. Skip on docs-only diffs.

If the diff is docs-only, run only `review-conventions` and `review-docs`.

Track which agents were skipped and why — this becomes the coverage line.

## 3. Parallel review

Launch the selected sub-agents **in parallel** (single message, multiple Agent
tool calls with `subagent_type` set to the agent's `name`). Pass the review
context packet verbatim as each agent's input.

Each returns a JSON array of findings: `{file, line, side, severity (P1/P2/P3),
category, finding, evidence}`.

## 4. Batched validation

Collect all findings, group **by file**, and validate each file's findings in
one pass:

1. Open the post-change file once; re-read the hunks plus surrounding context.
2. Walk every finding against that context, checking:
   - **Diff-introduced and grounded.** The flagged line was added/modified by
     the diff (or the diff made a pre-existing issue reachable). The claim must
     be backed by *quoted* code from the file, not a paraphrase. Drop it if it
     can't be quoted.
   - **Nuance** — surrounding code (a guard, a type, a newtype constructor)
     doesn't already handle the concern.
   - **Convention** — the project's established practice, if relevant.
   - **Hunk membership** — `(line, side)` lands within a diff hunk; snap to the
     nearest changed line if slightly off, drop if no fit.
3. **Deduplicate across agents** — keep the more specific description; note
   consensus.
4. **Cross-reference prior feedback** — drop findings duplicating resolved
   comments; add unresolved comments agents missed.
5. For `review-api-usage` claims, confirm the cited doc URL is current.

Separate surviving findings into substantive vs. `category: "Diff noise"`.

## 5. Post findings to the PR

Substantive findings go inline; diff-noise and the coverage line go in one
summary comment at the end.

### Inline review comments (substantive findings)

When the `mcp__github_inline_comment__create_inline_comment` tool is available
(CI under `claude-code-action`), post each finding with it — pass `path`, `line`
(and `startLine` for multi-line), `body`, and `confirmed: true`. The MCP server
reads owner/repo/PR from the environment; don't pass them. `side` defaults to
`RIGHT` — only set it for LEFT-side comments.

Locally (no MCP tool), fall back to `gh api
repos/{owner}/{repo}/pulls/{n}/comments -f body=... -f commit_id=<head_sha> -f
path=<file> -F line=<line> -f side=<RIGHT|LEFT>`. Resolve `<head_sha>` once via
`gh pr view <n> --json headRefOid -q .headRefOid`.

For each substantive finding:

- Body is the `finding` text, `evidence` folded in where useful. Drop the
  category tag unless it adds something a reviewer wouldn't infer.
- **No severity markers** (P1/P2/P3) in the body — internal triage only.
- Group by severity (P1 first) when ordering posts.

### Summary comment (always posted)

After the inline comments, post one top-level comment via `gh pr comment <n>`:

1. **Header** — `<K> finding(s) posted inline.` (or `No substantive findings.`).
2. **Diff noise** (optional) — a short list, one row per file with line ranges.
3. **Coverage line** — which agents ran and which were skipped (and why).
