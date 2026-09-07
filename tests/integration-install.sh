#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for install.sh's release provenance path. GitHub transports
# are stubbed, but the real installer performs checksum verification,
# attestation dispatch, extraction, and installation.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REAL_PATH="$PATH"
TEST_ROOT="$(mktemp -d)"
trap 'rm -rf "$TEST_ROOT"' EXIT

case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) TRIPLE="x86_64-unknown-linux-musl" ;;
    Linux-aarch64) TRIPLE="aarch64-unknown-linux-musl" ;;
    Darwin-arm64|Darwin-aarch64) TRIPLE="aarch64-apple-darwin" ;;
    *) echo "Unsupported test platform: $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

VERSION="v9.9.9"
ARCHIVE_DIR="coop-${VERSION}-${TRIPLE}"
TARBALL="${ARCHIVE_DIR}.tar.gz"
FIXTURE="$TEST_ROOT/fixture"
MOCK_BIN="$TEST_ROOT/mock-bin"
INSTALL_DIR="$TEST_ROOT/install"
GH_LOG="$TEST_ROOT/gh.log"
CURL_LOG="$TEST_ROOT/curl.log"
mkdir -p "$FIXTURE/$ARCHIVE_DIR" "$MOCK_BIN" "$INSTALL_DIR"

cat >"$FIXTURE/$ARCHIVE_DIR/coop" <<'EOF'
#!/bin/sh
echo installed-coop
EOF
cat >"$FIXTURE/$ARCHIVE_DIR/coop-proxy" <<'EOF'
#!/bin/sh
echo installed-coop-proxy
EOF
chmod +x "$FIXTURE/$ARCHIVE_DIR/coop" "$FIXTURE/$ARCHIVE_DIR/coop-proxy"
(cd "$FIXTURE" && tar -czf "$TARBALL" "$ARCHIVE_DIR")

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$FIXTURE" && sha256sum "$TARBALL" > SHA256SUMS)
else
    (cd "$FIXTURE" && shasum -a 256 "$TARBALL" > SHA256SUMS)
fi
printf '%s\n' '{"synthetic":"non-empty provenance bundle"}' \
    >"$FIXTURE/attestations.jsonl"

cat >"$MOCK_BIN/gh" <<'STUB'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$COOP_TEST_GH_LOG"

if [ "$1 $2" = "release download" ]; then
    pattern=""
    destination=""
    shift 2
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --pattern) pattern="$2"; shift 2 ;;
            --dir) destination="$2"; shift 2 ;;
            *) shift ;;
        esac
    done
    cp "$COOP_TEST_FIXTURE/$pattern" "$destination/$pattern"
    exit 0
fi

if [ "$1 $2" = "attestation verify" ]; then
    [ "${COOP_TEST_GH_VERIFY_FAIL:-0}" != "1" ] || exit 42
    bundle=""
    while [ "$#" -gt 0 ]; do
        if [ "$1" = "--bundle" ]; then
            bundle="$2"
            break
        fi
        shift
    done
    if [ "${COOP_TEST_GH_REQUIRE_BUNDLE:-0}" = "1" ]; then
        [ -n "$bundle" ] && [ -s "$bundle" ] || exit 43
    fi
    exit 0
fi

exit 44
STUB

cat >"$MOCK_BIN/curl" <<'STUB'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$COOP_TEST_CURL_LOG"
destination=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) destination="$2"; shift 2 ;;
        http://*|https://*) url="$1"; shift ;;
        *) shift ;;
    esac
done
case "$url" in
    */attestations.jsonl)
        [ "${COOP_TEST_BUNDLE_FAIL:-0}" != "1" ] || exit 22
        cp "$COOP_TEST_FIXTURE/attestations.jsonl" "$destination"
        ;;
    *) exit 45 ;;
esac
STUB
chmod +x "$MOCK_BIN/gh" "$MOCK_BIN/curl"

pass_count=0
fail_count=0

pass() {
    pass_count=$((pass_count + 1))
    echo "  PASS  $1"
}

fail() {
    fail_count=$((fail_count + 1))
    echo "  FAIL  $1"
    if [[ -n "${2:-}" ]]; then
        echo "        $2"
    fi
}

run_installer() {
    env PATH="$MOCK_BIN:$REAL_PATH" \
        VERSION="$VERSION" \
        INSTALL_DIR="$INSTALL_DIR" \
        GITHUB_TOKEN="restricted-token-must-not-reach-bundle-curl" \
        COOP_TEST_FIXTURE="$FIXTURE" \
        COOP_TEST_GH_LOG="$GH_LOG" \
        COOP_TEST_CURL_LOG="$CURL_LOG" \
        COOP_TEST_GH_REQUIRE_BUNDLE="${COOP_TEST_GH_REQUIRE_BUNDLE:-0}" \
        COOP_TEST_GH_VERIFY_FAIL="${COOP_TEST_GH_VERIFY_FAIL:-0}" \
        COOP_TEST_BUNDLE_FAIL="${COOP_TEST_BUNDLE_FAIL:-0}" \
        bash "$PROJECT_DIR/install.sh"
}

echo "==> Test 1: published bundle is downloaded anonymously and verified"
: >"$GH_LOG"
: >"$CURL_LOG"
COOP_TEST_GH_REQUIRE_BUNDLE=1
export COOP_TEST_GH_REQUIRE_BUNDLE
if run_installer >"$TEST_ROOT/t1.log" 2>&1; then
    pass "install succeeds with a published attestation bundle"
else
    fail "install succeeds with a published attestation bundle" \
        "$(tail -10 "$TEST_ROOT/t1.log")"
fi
unset COOP_TEST_GH_REQUIRE_BUNDLE

if "$INSTALL_DIR/coop" | grep -q '^installed-coop$' \
    && "$INSTALL_DIR/coop-proxy" | grep -q '^installed-coop-proxy$'; then
    pass "installer extracts coop and coop-proxy"
else
    fail "installer extracts coop and coop-proxy"
fi

if grep -q 'attestation verify .* --repo trailofbits/coop --bundle ' "$GH_LOG"; then
    pass "gh verifies the tarball with the downloaded bundle"
else
    fail "gh verifies the tarball with the downloaded bundle" "gh calls: $(cat "$GH_LOG")"
fi

if grep -Eq -- '-H|--header|restricted-token' "$CURL_LOG"; then
    fail "bundle download carries no GitHub credential" "curl args: $(cat "$CURL_LOG")"
else
    pass "bundle download carries no GitHub credential"
fi

echo "==> Test 2: bundle verification failure is fail-closed"
printf '%s\n' 'keep-existing-install' >"$INSTALL_DIR/coop"
COOP_TEST_GH_VERIFY_FAIL=1
export COOP_TEST_GH_VERIFY_FAIL
if run_installer >"$TEST_ROOT/t2.log" 2>&1; then
    fail "failed bundle verification aborts installation" "installer exited 0"
elif [[ "$(cat "$INSTALL_DIR/coop")" == "keep-existing-install" ]]; then
    pass "failed bundle verification leaves the installed binary unchanged"
else
    fail "failed bundle verification leaves the installed binary unchanged"
fi
unset COOP_TEST_GH_VERIFY_FAIL

echo "==> Test 3: releases without a usable bundle retain API fallback"
: >"$GH_LOG"
COOP_TEST_BUNDLE_FAIL=1
export COOP_TEST_BUNDLE_FAIL
if run_installer >"$TEST_ROOT/t3.log" 2>&1; then
    pass "installer falls back for a pre-bundle release"
else
    fail "installer falls back for a pre-bundle release" \
        "$(tail -10 "$TEST_ROOT/t3.log")"
fi
unset COOP_TEST_BUNDLE_FAIL

if grep -q 'attestation verify .* --repo trailofbits/coop$' "$GH_LOG" \
    && ! grep -q -- '--bundle' "$GH_LOG"; then
    pass "legacy fallback verifies through the attestations API"
else
    fail "legacy fallback verifies through the attestations API" "gh calls: $(cat "$GH_LOG")"
fi

echo
echo "  $pass_count passed, $fail_count failed"
[[ $fail_count -eq 0 ]]
