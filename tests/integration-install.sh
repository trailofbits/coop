#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for install.sh's release provenance path. GitHub transports
# are stubbed, but the real installer performs checksum verification,
# attestation dispatch, extraction, and installation.

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REAL_PATH="$PATH"
command -v jq >/dev/null || { echo "jq is required for these tests" >&2; exit 1; }
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
    if [ "${COOP_TEST_AUTH_FAIL:-0}" = 1 ]; then
        printf partial > "$destination/$pattern"
        exit 1
    fi
    cp "$COOP_TEST_FIXTURE/$pattern" "$destination/$pattern"
    exit 0
fi

if [ "$1 $2" = "attestation verify" ]; then
    [ "${COOP_TEST_GH_VERIFY_FAIL:-0}" != "1" ] || exit 42
    bundle=""
    repo=""
    identity=""
    source_ref=""
    deny_self_hosted=0
    shift 3
    while [ "$#" -gt 0 ]; do
        case "$1" in
            --repo) repo="$2"; shift 2 ;;
            --cert-identity) identity="$2"; shift 2 ;;
            --source-ref) source_ref="$2"; shift 2 ;;
            --deny-self-hosted-runners) deny_self_hosted=1; shift ;;
            --bundle) bundle="$2"; shift 2 ;;
            *) exit 46 ;;
        esac
    done
    [ "$repo" = "trailofbits/coop" ] || exit 47
    [ "$identity" = "https://github.com/trailofbits/coop/.github/workflows/release.yml@refs/tags/$VERSION" ] || exit 48
    [ "$source_ref" = "refs/tags/$VERSION" ] || exit 49
    [ "$deny_self_hosted" = 1 ] || exit 50
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
auth=0
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) destination="$2"; shift 2 ;;
        -H) if [ "$2" = @- ]; then auth=1; cat >/dev/null; fi; shift 2 ;;
        http://*|https://*) url="$1"; shift ;;
        *) shift ;;
    esac
done
if [ "$auth" = 1 ] && [ "${COOP_TEST_AUTH_FAIL:-0}" = 1 ]; then
    [ -z "$destination" ] || printf partial > "$destination"
    exit 22
fi
case "$url" in
    */attestations/sha256:*)
        [ "$auth" = 0 ] || exit 51
        [ "${COOP_TEST_API_FAIL:-0}" != 1 ] || exit 22
        cp "$COOP_TEST_FIXTURE/api.json" "$destination"
        ;;
    */releases/latest) printf '%s\n' '{"tag_name":"v9.9.9"}' ;;
    */SHA256SUMS|*.tar.gz)
        [ "${COOP_TEST_PUBLIC_FAIL:-0}" != 1 ] || exit 22
        cp "$COOP_TEST_FIXTURE/${url##*/}" "$destination"
        ;;
    */attestations.jsonl)
        [ "${COOP_TEST_BUNDLE_FAIL:-0}" != "1" ] || exit 22
        cp "$COOP_TEST_FIXTURE/attestations.jsonl" "$destination"
        ;;
    *) exit 45 ;;
esac
STUB
printf '%s\n' '{"attestations":[{"bundle":{"synthetic":true}}]}' > "$FIXTURE/api.json"
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
        VERSION="${COOP_TEST_VERSION-$VERSION}" \
        INSTALL_DIR="$INSTALL_DIR" \
        GITHUB_TOKEN="restricted-token-must-not-reach-bundle-curl" \
        COOP_TEST_FIXTURE="$FIXTURE" \
        COOP_TEST_GH_LOG="$GH_LOG" \
        COOP_TEST_CURL_LOG="$CURL_LOG" \
        COOP_TEST_GH_REQUIRE_BUNDLE="${COOP_TEST_GH_REQUIRE_BUNDLE:-0}" \
        COOP_TEST_GH_VERIFY_FAIL="${COOP_TEST_GH_VERIFY_FAIL:-0}" \
        COOP_TEST_AUTH_FAIL="${COOP_TEST_AUTH_FAIL:-0}" \
        COOP_TEST_PUBLIC_FAIL="${COOP_TEST_PUBLIC_FAIL:-0}" \
        COOP_TEST_API_FAIL="${COOP_TEST_API_FAIL:-0}" \
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

if grep -q 'attestation verify .* --repo trailofbits/coop .* --bundle ' "$GH_LOG"; then
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
: >"$GH_LOG"
printf '%s\n' 'keep-existing-install' >"$INSTALL_DIR/coop"
printf '%s\n' 'keep-existing-proxy' >"$INSTALL_DIR/coop-proxy"
COOP_TEST_GH_VERIFY_FAIL=1
export COOP_TEST_GH_VERIFY_FAIL
if run_installer >"$TEST_ROOT/t2.log" 2>&1; then
    fail "failed bundle verification aborts installation" "installer exited 0"
elif grep -q "Attestation verification failed" "$TEST_ROOT/t2.log" \
    && [[ "$(cat "$INSTALL_DIR/coop")" == "keep-existing-install" \
       && "$(cat "$INSTALL_DIR/coop-proxy")" == "keep-existing-proxy" ]]; then
    pass "failed bundle verification leaves both installed binaries unchanged"
else
    fail "failed bundle verification leaves both installed binaries unchanged" \
        "$(tail -10 "$TEST_ROOT/t2.log")"
fi
unset COOP_TEST_GH_VERIFY_FAIL
if [[ "$(grep -c '^attestation verify ' "$GH_LOG")" == 1 ]] \
    && grep -q -- '--bundle' "$GH_LOG"; then
    pass "failed bundle verification never retries through the API"
else
    fail "failed bundle verification never retries through the API" "gh calls: $(cat "$GH_LOG")"
fi

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

if grep -q -- '--bundle .*api-attestations.jsonl' "$GH_LOG" \
    && grep -q '/attestations/sha256:' "$CURL_LOG"; then
    pass "legacy fallback fetches anonymously and verifies a local bundle"
else
    fail "legacy fallback fetches anonymously and verifies a local bundle" "gh calls: $(cat "$GH_LOG")"
fi

echo "==> Test 3b: API verification failure is fail-closed"
printf '%s\n' 'keep-existing-install' >"$INSTALL_DIR/coop"
printf '%s\n' 'keep-existing-proxy' >"$INSTALL_DIR/coop-proxy"
COOP_TEST_BUNDLE_FAIL=1
COOP_TEST_GH_VERIFY_FAIL=1
export COOP_TEST_BUNDLE_FAIL COOP_TEST_GH_VERIFY_FAIL
if run_installer >"$TEST_ROOT/t3b.log" 2>&1; then
    fail "failed API verification aborts installation" "installer exited 0"
elif grep -q "Attestation verification failed" "$TEST_ROOT/t3b.log" \
    && [[ "$(cat "$INSTALL_DIR/coop")" == "keep-existing-install" \
       && "$(cat "$INSTALL_DIR/coop-proxy")" == "keep-existing-proxy" ]]; then
    pass "failed API verification leaves both installed binaries unchanged"
else
    fail "failed API verification leaves both installed binaries unchanged" \
        "$(tail -10 "$TEST_ROOT/t3b.log")"
fi
unset COOP_TEST_BUNDLE_FAIL COOP_TEST_GH_VERIFY_FAIL

echo "==> Anonymous transfer retries and unusable API responses"
COOP_TEST_AUTH_FAIL=1
export COOP_TEST_AUTH_FAIL
if run_installer >"$TEST_ROOT/retry.log" 2>&1; then
    pass "rejected gh and token downloads retry anonymously, replacing partial files"
else
    fail "rejected downloads retry anonymously" "$(tail -10 "$TEST_ROOT/retry.log")"
fi
COOP_TEST_VERSION=""
export COOP_TEST_VERSION
if run_installer >"$TEST_ROOT/metadata.log" 2>&1; then
    pass "latest metadata retries anonymously after gh and token failures"
else
    fail "latest metadata retries anonymously" "$(tail -10 "$TEST_ROOT/metadata.log")"
fi
unset COOP_TEST_VERSION
printf '%s\n' keep-existing-install >"$INSTALL_DIR/coop"
COOP_TEST_PUBLIC_FAIL=1
export COOP_TEST_PUBLIC_FAIL
if run_installer >"$TEST_ROOT/both-fail.log" 2>&1; then
    fail "both transports failing aborts install"
elif [[ "$(cat "$INSTALL_DIR/coop")" == keep-existing-install ]]; then
    pass "both transports failing preserves installation"
else
    fail "both transports failing preserves installation"
fi
unset COOP_TEST_PUBLIC_FAIL COOP_TEST_AUTH_FAIL
COOP_TEST_BUNDLE_FAIL=1
export COOP_TEST_BUNDLE_FAIL
for response in '{}' '{"attestations":[]}' '{"attestations":[{"bundle":null}]}' '{"attestations":[{"bundle":{}}]}' 'invalid-json'; do
    printf '%s\n' "$response" > "$FIXTURE/api.json"
    : > "$GH_LOG"
    if run_installer >"$TEST_ROOT/bad-api.log" 2>&1; then
        fail "unusable API response aborts install: $response"
    elif grep -q '^attestation verify ' "$GH_LOG"; then
        fail "unusable API response must not reach verifier: $response"
    else
        pass "unusable API response rejected before verification: $response"
    fi
done
printf '%s\n' '{"attestations":[{"bundle":{"synthetic":true}}]}' > "$FIXTURE/api.json"
unset COOP_TEST_BUNDLE_FAIL

echo "==> API transport failure and missing-jq compatibility"
COOP_TEST_BUNDLE_FAIL=1
COOP_TEST_API_FAIL=1
export COOP_TEST_BUNDLE_FAIL COOP_TEST_API_FAIL
: > "$GH_LOG"
if run_installer >"$TEST_ROOT/api-fail.log" 2>&1 \
    && grep -q '^attestation verify ' "$GH_LOG" \
    && ! grep -q -- '--bundle' "$GH_LOG"; then
    pass "anonymous API transport failure retains authenticated verification"
else
    fail "anonymous API failure retains authenticated verification"
fi
COOP_TEST_GH_VERIFY_FAIL=1
export COOP_TEST_GH_VERIFY_FAIL
if run_installer >"$TEST_ROOT/api-both-fail.log" 2>&1; then
    fail "anonymous retrieval and authenticated verification failure abort installation"
else
    pass "anonymous retrieval and authenticated verification failure abort installation"
fi
unset COOP_TEST_GH_VERIFY_FAIL
unset COOP_TEST_API_FAIL
# Source only the helpers, then hide jq from has() while retaining all other
# host tools. This exercises the compatibility branch without changing PATH.
sed '/^# --- main /,$d' "$PROJECT_DIR/install.sh" > "$TEST_ROOT/helpers.sh"
: > "$GH_LOG"
cat > "$TEST_ROOT/no-jq.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
source "$1"
TMPDIR="$2"
has() { [ "$1" != jq ] && command -v "$1" >/dev/null 2>&1; }
verify_attestation "$3" "$4"
EOF
if env PATH="$MOCK_BIN:$REAL_PATH" VERSION="$VERSION" \
    COOP_TEST_GH_LOG="$GH_LOG" COOP_TEST_CURL_LOG="$CURL_LOG" \
    COOP_TEST_FIXTURE="$FIXTURE" COOP_TEST_BUNDLE_FAIL=1 \
    bash "$TEST_ROOT/no-jq.sh" "$TEST_ROOT/helpers.sh" "$TEST_ROOT" "$FIXTURE/$TARBALL" \
    "$(cut -d' ' -f1 "$FIXTURE/SHA256SUMS")" >"$TEST_ROOT/no-jq.log" 2>&1 \
    && grep -q 'Install jq' "$TEST_ROOT/no-jq.log" \
    && grep -q '^attestation verify ' "$GH_LOG" \
    && ! grep -q -- '--bundle' "$GH_LOG"; then
    pass "missing jq retains authenticated gh verification with guidance"
else
    fail "missing jq retains authenticated verification" "$(tail -10 "$TEST_ROOT/no-jq.log")"
fi
unset COOP_TEST_BUNDLE_FAIL

echo "==> Test 4: checksum rejection preserves both binaries"
printf '%s\n' 'keep-existing-install' >"$INSTALL_DIR/coop"
printf '%s\n' 'keep-existing-proxy' >"$INSTALL_DIR/coop-proxy"
printf '%064d  %s\n' 0 "$TARBALL" >"$FIXTURE/SHA256SUMS"
if run_installer >"$TEST_ROOT/t4.log" 2>&1; then
    fail "checksum mismatch aborts installation" "installer exited 0"
elif grep -q "Checksum mismatch" "$TEST_ROOT/t4.log" \
    && [[ "$(cat "$INSTALL_DIR/coop")" == "keep-existing-install" \
       && "$(cat "$INSTALL_DIR/coop-proxy")" == "keep-existing-proxy" ]]; then
    pass "checksum mismatch leaves both installed binaries unchanged"
else
    fail "checksum mismatch leaves both installed binaries unchanged" \
        "$(tail -10 "$TEST_ROOT/t4.log")"
fi

if command -v sha256sum >/dev/null 2>&1; then
    (cd "$FIXTURE" && sha256sum "$TARBALL" > SHA256SUMS)
else
    (cd "$FIXTURE" && shasum -a 256 "$TARBALL" > SHA256SUMS)
fi

echo "==> Test 5: reinstall replaces both existing binaries"
if run_installer >"$TEST_ROOT/t5.log" 2>&1 \
    && [[ "$("$INSTALL_DIR/coop")" == "installed-coop" \
       && "$("$INSTALL_DIR/coop-proxy")" == "installed-coop-proxy" ]]; then
    pass "reinstall replaces both binaries with verified release contents"
else
    fail "reinstall replaces both binaries with verified release contents" \
        "$(tail -10 "$TEST_ROOT/t5.log")"
fi

echo "==> Test 6: legacy package without companion remains installable"
rm "$FIXTURE/$ARCHIVE_DIR/coop-proxy"
(cd "$FIXTURE" && tar -czf "$TARBALL" "$ARCHIVE_DIR")
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$FIXTURE" && sha256sum "$TARBALL" > SHA256SUMS)
else
    (cd "$FIXTURE" && shasum -a 256 "$TARBALL" > SHA256SUMS)
fi
printf '%s\n' 'old-coop' >"$INSTALL_DIR/coop"
if run_installer >"$TEST_ROOT/t6.log" 2>&1 \
    && [[ "$("$INSTALL_DIR/coop")" == "installed-coop" \
       && "$("$INSTALL_DIR/coop-proxy")" == "installed-coop-proxy" ]]; then
    pass "legacy install replaces coop and preserves the existing companion"
else
    fail "legacy install replaces coop and preserves the existing companion" \
        "$(tail -10 "$TEST_ROOT/t6.log")"
fi

echo
echo "  $pass_count passed, $fail_count failed"
[[ $fail_count -eq 0 ]]
