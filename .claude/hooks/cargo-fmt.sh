#!/usr/bin/env bash
set -euo pipefail

# PostToolUse formatter: run rustfmt on the Rust file Claude just wrote or
# edited, mirroring the `cargo fmt` pre-commit hook so in-progress edits stay
# formatted and don't surface later as CI `cargo fmt -- --check` failures.
#
# We format the single edited file with `rustfmt`, NOT whole-crate `cargo fmt`:
# `cargo fmt` discovers its workspace from the hook's cwd, so for an edit to a
# file in a linked worktree it would format the wrong crate (missing the edited
# file entirely) and silently rewrite every other file in that crate. rustfmt
# on the path touches only what changed and reads any `rustfmt.toml` from the
# file's directory upward.
#
# Format only. `cargo clippy --fix` is deliberately NOT run here: autofixing
# mid-edit rewrites code Claude is about to build on. Clippy and the
# zero-warnings policy are enforced by the pre-commit hook and CI. rustfmt is
# idempotent and never surprises an in-progress edit.
#
# The hook must never block the tool, so a formatter failure is surfaced on
# stderr (with the file and rustfmt's own error) but does not propagate.

INPUT=$(cat)
FILE=$(jq -r '.tool_input.file_path // .tool_response.filePath // empty' <<<"$INPUT" 2>/dev/null || true)

if [ -z "$FILE" ] || [ "${FILE##*.}" != "rs" ] || [ ! -f "$FILE" ]; then
  exit 0
fi

command -v rustfmt >/dev/null 2>&1 || exit 0

# Edition must match the crate (coop is edition 2024); rustfmt defaults to 2015
# otherwise. CI's `cargo fmt` remains the authoritative check if this ever
# drifts from a future edition bump.
if ! err=$(rustfmt --edition 2024 "$FILE" 2>&1); then
  printf 'cargo-fmt hook: rustfmt failed on %s\n%s\n' "$FILE" "$err" >&2
fi

exit 0
