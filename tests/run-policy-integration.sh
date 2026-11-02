#!/usr/bin/env bash
set -euo pipefail

# Shared build for CI and release preflight; each consumer receives immutable
# snapshots and makes disposable copies for update/uninstall operations.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
POLICY_TMP="$(mktemp -d)"
trap 'rm -rf "$POLICY_TMP"' EXIT
exec </dev/null

"$SCRIPT_DIR/build-policy-binaries.sh" "$POLICY_TMP/artifacts" \
    test-source homebrew official dev release-smoke

# Keep an optimized smoke test in addition to the development-profile matrix.
# Pin the source-policy rejection so this never contacts the release service.
echo "==> Optimized release smoke test"
RELEASE_BIN="$POLICY_TMP/artifacts/coop-release-smoke"
version="$("$RELEASE_BIN" --version)"
[[ "$version" == "coop "* && "$version" != *-dev* ]]
"$RELEASE_BIN" --help > /dev/null
if COOP_UPDATE_API_BASE_URL=http://127.0.0.1:1 \
    "$RELEASE_BIN" update --check > "$POLICY_TMP/release.log" 2>&1; then
    echo "FAIL: optimized release accepted a runtime update source" >&2
    exit 1
fi
grep -q 'update source policy rejected COOP_UPDATE_API_BASE_URL' "$POLICY_TMP/release.log"
echo "  PASS  optimized release version, CLI, and update-source policy"

# Run both suites even if one fails, preserving preflight's failure reporting.
status=0
"$SCRIPT_DIR/integration-update.sh" "$POLICY_TMP/artifacts" || status=1
"$SCRIPT_DIR/integration-uninstall.sh" "$POLICY_TMP/artifacts/coop-dev" || status=1
exit "$status"
