#!/usr/bin/env bash
set -euo pipefail

INPUT=$(cat)
COMMAND=$(jq -r '.tool_input.command // .tool_input.cmd // .command // .cmd // empty' <<<"$INPUT")

if [ -z "$COMMAND" ]; then
  exit 0
fi

# Expensive commands in this project: the cargo quality gates, mutation and
# fuzzing sweeps, and the integration-test scripts. \b handles subcommand
# suffixes like `cargo test --lib` and `./tests/run-integration.sh --remote`.
EXPENSIVE='\b(cargo (test|clippy|mutants|build)|cargo \+nightly fuzz|cargo kani|tests/(run-)?integration([a-z-]*)\.sh)\b'

# Filters that indicate the command output is being searched rather than
# inspected as a whole.
FILTERS='\|\s*(grep|rg|head|tail|awk|sed)\b'

if echo "$COMMAND" | grep -qE "$EXPENSIVE" \
  && echo "$COMMAND" | grep -qE "$FILTERS"; then
  tmpdir=${TMPDIR:-/tmp}
  OUTFILE=$(mktemp "${tmpdir%/}/test-output.XXXXXX")

  cat >&2 <<EOF
Do not pipe this output through grep/rg/head/tail/awk/sed — it forces the
command to run again every time you want to examine different parts of the
output. This applies to cargo test/clippy/build, cargo mutants, cargo fuzz,
cargo kani, and the tests/*integration*.sh scripts.

Instead:
Redirect output to a file:  <command> > ${OUTFILE} 2>&1
Read the file:              Read tool or cat ${OUTFILE}
Search the file:            grep "pattern" ${OUTFILE}

This way the command runs once and you can inspect the results as many times
as needed. Do not forget to delete the file once you are done using it.
EOF
  exit 2
fi

exit 0
