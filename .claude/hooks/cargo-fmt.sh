#!/usr/bin/env bash
set -euo pipefail

# PostToolUse formatter: run `cargo fmt` after Claude writes or edits a Rust
# file, mirroring the `cargo fmt` pre-commit hook so in-progress edits stay
# formatted and don't surface later as CI `cargo fmt -- --check` failures.
#
# Format only. `cargo clippy --fix` is deliberately NOT run here: autofixing
# mid-edit rewrites code Claude is about to build on, causing an edit/undo
# churn loop. Clippy and the zero-warnings policy are surfaced (and enforced)
# by the pre-commit hook and CI. `cargo fmt` is idempotent and never surprises
# an in-progress edit.
#
# We format the whole crate (like the pre-commit hook) rather than a single
# file, so rustfmt always reads the project edition/config and never drifts.
# The hook must never block the tool, so a formatter failure is surfaced on
# stderr but does not propagate.

INPUT=$(cat)
FILE=$(jq -r '.tool_input.file_path // .tool_response.filePath // empty' <<<"$INPUT" 2>/dev/null || true)

if [ -z "$FILE" ] || [ "${FILE##*.}" != "rs" ]; then
  exit 0
fi

command -v cargo >/dev/null 2>&1 || exit 0
cargo fmt --quiet 2>/dev/null || echo "cargo fmt failed" >&2

exit 0
