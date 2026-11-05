#!/usr/bin/env bash
set -euo pipefail

# PreToolUse gate: require a closeout review on the branch the PR is created
# FROM, at its current tip. A draft accepts a completed review with a blocked
# verdict; a ready PR requires a clean review. The closeout-review skill
# records the verdict; this hook only checks it.
#
# Worktree-aware: the branch under review usually lives in a linked worktree
# while this hook runs from the main checkout (CLAUDE_PROJECT_DIR). So we
# (a) resolve the git context from the command's `cd` prefix or the hook-
# provided cwd instead of the hook's own PWD, (b) take the target branch from
# `--head` when it is given (a PR can be opened for a branch you are not on),
# and (c) store markers under the shared git common dir so a marker written
# from a worktree is visible from every other worktree and the main checkout.
#
# Fail-open by design: anything that isn't an identifiable `gh pr create` in a
# git repo is allowed through, so the gate never wedges unrelated work.

INPUT=$(cat)
COMMAND=$(jq -r '.tool_input.command // .tool_input.cmd // .command // .cmd // empty' <<<"$INPUT" 2>/dev/null || true)

[ -z "$COMMAND" ] && exit 0

# Only gate PR creation. `pr create` is contiguous (subcommand follows `pr`).
PR_CREATE_RE='(^|[[:space:]])gh[[:space:]]+pr[[:space:]]+create([[:space:]]|$)'
[[ "$COMMAND" =~ $PR_CREATE_RE ]] || exit 0
PR_ARGS="${COMMAND#*"${BASH_REMATCH[0]}"}"

# Locate the directory the command actually runs git/gh in: an explicit
# `cd <dir> && ...` prefix wins, else the cwd the hook was invoked with.
WORKDIR=""
if [[ "$COMMAND" =~ ^[[:space:]]*cd[[:space:]]+([^[:space:]\&\;\|]+) ]]; then
  WORKDIR="${BASH_REMATCH[1]}"
  WORKDIR="${WORKDIR%[\"\']}"
  WORKDIR="${WORKDIR#[\"\']}"
  # A `cd` target we can't resolve — unexpanded `~`/`$VAR`, or a path truncated
  # at its first space — must NOT suppress the fallbacks below, or the gate
  # fails open on idiomatic `cd ~/wt && gh pr create`. Drop it and let the
  # hook-provided cwd / $PWD decide.
  [ -d "$WORKDIR" ] || WORKDIR=""
fi
[ -z "$WORKDIR" ] && WORKDIR=$(jq -r '.cwd // empty' <<<"$INPUT" 2>/dev/null || true)
[ -z "$WORKDIR" ] && WORKDIR="$PWD"
[ -d "$WORKDIR" ] || exit 0

git -C "$WORKDIR" rev-parse --show-toplevel >/dev/null 2>&1 || exit 0

# Read the value of an actual `--head`/`-H` flag with a quote-aware scan. A
# regex over the raw command takes the leftmost match, so a `--head`/`-H` token
# quoted inside a `--body`/`--title` string would hijack branch selection
# whenever it names a resolvable ref (`main`, `develop`). Tokenizing first keeps
# such a token bound inside its quoted argument, where it is never the flag.
head_flag_value() {
  local s="$1" c i n q="" tok="" want=0
  local -a toks=()
  n=${#s}
  for ((i = 0; i < n; i++)); do
    c="${s:i:1}"
    if [ -n "$q" ]; then
      if [ "$c" = "$q" ]; then q=""; else tok+="$c"; fi
    elif [ "$c" = '"' ] || [ "$c" = "'" ]; then
      q="$c"
    elif [[ "$c" == [[:space:]] ]]; then
      if [ -n "$tok" ]; then
        toks+=("$tok")
        tok=""
      fi
    else
      tok+="$c"
    fi
  done
  [ -n "$tok" ] && toks+=("$tok")
  for tok in "${toks[@]}"; do
    if [ "$want" = 1 ]; then
      printf '%s' "$tok"
      return
    fi
    case "$tok" in
      --head | -H) want=1 ;;
      --head=*)
        printf '%s' "${tok#--head=}"
        return
        ;;
      -H=*)
        printf '%s' "${tok#-H=}"
        return
        ;;
      -H?*)
        printf '%s' "${tok#-H}"
        return
        ;;
    esac
  done
}

# For a blocked review, accept only `--draft` as the first PR argument. This
# avoids mistaking an option value such as `--title --draft` for the draft flag.
# Reject later shell operators so the same command cannot also mark it ready.
has_draft_flag() {
  local s="$1" c i n q="" tok="" quoted=0 found=0 first=1
  n=${#s}
  for ((i = 0; i < n; i++)); do
    c="${s:i:1}"
    if [ -n "$q" ]; then
      if [ "$c" = "$q" ]; then q=""; else tok+="$c"; fi
    elif [ "$c" = '"' ] || [ "$c" = "'" ]; then
      q="$c"
      quoted=1
    elif [ "$c" = '#' ] && [ -z "$tok" ]; then
      return 1
    elif [ "$c" = ';' ] || [ "$c" = '&' ] || [ "$c" = '|' ] ||
      [ "$c" = '$' ] || [ "$c" = '`' ] || [ "$c" = $'\n' ]; then
      return 1
    elif [[ "$c" == [[:space:]] ]]; then
      if [ -n "$tok" ] && [ "$first" = 1 ]; then
        [ "$tok" = "--draft" ] && [ "$quoted" = 0 ] || return 1
        found=1
        first=0
      fi
      tok=""
      quoted=0
    else
      tok+="$c"
    fi
  done
  if [ "$first" = 1 ]; then
    [ "$tok" = "--draft" ] && [ "$quoted" = 0 ] && found=1
  fi
  [ "$found" = 1 ]
}

# Target branch: the `--head`/`-H` value (strip any `owner:` fork prefix) when it
# names a real ref, else the branch currently checked out in WORKDIR. A head
# that does not resolve to a commit falls back to the current branch, so the
# gate still evaluates a real branch.
BRANCH=""
candidate=$(head_flag_value "$COMMAND")
candidate="${candidate##*:}"
if [ -n "$candidate" ] &&
  git -C "$WORKDIR" rev-parse --verify --quiet "${candidate}^{commit}" >/dev/null 2>&1; then
  BRANCH="$candidate"
fi
if [ -z "$BRANCH" ]; then
  BRANCH=$(git -C "$WORKDIR" rev-parse --abbrev-ref HEAD 2>/dev/null) || exit 0
fi

HEAD_SHA=$(git -C "$WORKDIR" rev-parse --verify "${BRANCH}^{commit}" 2>/dev/null) || exit 0

# Markers live under the shared git common dir so one written from any linked
# worktree is found from every other worktree (and the main checkout).
COMMON_DIR=$(git -C "$WORKDIR" rev-parse --git-common-dir 2>/dev/null) || exit 0
case "$COMMON_DIR" in
  /*) ;;
  *) COMMON_DIR="$WORKDIR/$COMMON_DIR" ;;
esac
COMMON_DIR=$(cd "$COMMON_DIR" 2>/dev/null && pwd) || exit 0

MARKER="${COMMON_DIR}/closeout-review/$(echo "$BRANCH" | tr '/' '-')"
if [ -f "$MARKER" ]; then
  verdict=$(cat "$MARKER")
  # Accept an older clean marker containing only the SHA.
  if [ "$verdict" = "$HEAD_SHA clean" ] || [ "$verdict" = "$HEAD_SHA" ]; then
    exit 0
  fi
  if [ "$verdict" = "$HEAD_SHA blocked" ] && has_draft_flag "$PR_ARGS"; then
    exit 0
  fi
fi

cat >&2 <<EOF
Closeout gate: this branch lacks the review marker required for this PR at its
current commit.

Branch: ${BRANCH}
HEAD:   ${HEAD_SHA}

Run the closeout-review skill now (invoke it via the Skill tool). A draft PR
requires a completed full-diff review; a ready PR requires a clean code-review
verdict. The skill records the verdict for this exact commit. After a blocked
review, put --draft immediately after gh pr create.

If you commit again after the review, re-run it: the marker is bound to the
reviewed commit, so new commits re-arm the gate.
EOF
exit 2
