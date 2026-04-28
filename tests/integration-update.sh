#!/usr/bin/env bash
set -euo pipefail

# End-to-end test for `coop update`.
#
# Serves a synthetic GitHub-shaped fixture from a local HTTP server and
# verifies that the update flow downloads, checksums, and atomically
# replaces the running binary. Also verifies checksum rollback,
# `--check` behaviour, and dev-build refusal.
#
# Run manually:   ./tests/integration-update.sh
# Run in CI:      same (fast — no VM, no external network)

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Platform detection (matches install.sh / update::target_triple) ──────────

detect_triple() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "${os}-${arch}" in
        Linux-x86_64)   echo "x86_64-unknown-linux-musl" ;;
        Linux-aarch64)  echo "aarch64-unknown-linux-musl" ;;
        Darwin-arm64)   echo "aarch64-apple-darwin" ;;
        Darwin-aarch64) echo "aarch64-apple-darwin" ;;
        *)
            echo "Unsupported platform: ${os}-${arch}" >&2
            exit 1
            ;;
    esac
}

TARGET_TRIPLE="$(detect_triple)"
FAKE_TAG="v9.9.9"
FAKE_DIR="coop-${FAKE_TAG}-${TARGET_TRIPLE}"
FAKE_TARBALL="${FAKE_DIR}.tar.gz"

# ── Temp workspace + server lifecycle ────────────────────────────────────────

TMPDIR="$(mktemp -d)"
SERVER_PID=""

cleanup() {
    if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2> /dev/null; then
        kill "$SERVER_PID" 2> /dev/null || true
        wait "$SERVER_PID" 2> /dev/null || true
    fi
    rm -rf "$TMPDIR"
}
trap cleanup EXIT

FIXTURE="$TMPDIR/fixture"
mkdir -p "$FIXTURE/repos/trailofbits/coop/releases/tags"
mkdir -p "$TMPDIR/bin" "$TMPDIR/build/${FAKE_DIR}"

# ── Helpers ──────────────────────────────────────────────────────────────────

pass_count=0
fail_count=0

pass() {
    pass_count=$((pass_count + 1))
    echo "  PASS  $1"
}

fail() {
    fail_count=$((fail_count + 1))
    echo "  FAIL  $1"
    [[ -n "${2:-}" ]] && echo "        $2"
}

if command -v sha256sum > /dev/null 2>&1; then
    SHA256_CMD=(sha256sum)
elif command -v shasum > /dev/null 2>&1; then
    SHA256_CMD=(shasum -a 256)
else
    echo "Neither sha256sum nor shasum is available" >&2
    exit 1
fi

sha_of() {
    "${SHA256_CMD[@]}" "$1" | cut -d' ' -f1
}

sha256sums_line() {
    "${SHA256_CMD[@]}" "$1"
}

write_release_json() {
    local path="$1" tag="$2" assets_block="$3"
    cat > "$path" << JSON
{
  "tag_name": "${tag}",
  "assets": ${assets_block}
}
JSON
}

full_assets_block() {
    cat << JSON
[
  {"name": "${FAKE_TARBALL}", "browser_download_url": "${BASE_URL}/${FAKE_TARBALL}"},
  {"name": "SHA256SUMS", "browser_download_url": "${BASE_URL}/SHA256SUMS"}
]
JSON
}

# ── Build the real coop binary (release kind, for success tests) ─────────────

echo "==> Building release binary..."
(
    cd "$PROJECT_DIR"
    COOP_FORCE_BUILD_KIND=release cargo build --release --quiet
)
RELEASE_BIN="$PROJECT_DIR/target/release/coop"
cp "$RELEASE_BIN" "$TMPDIR/bin/coop"
export COOP_BIN="$TMPDIR/bin/coop"

# ── Fabricate a "newer" release tarball ──────────────────────────────────────

cat > "$TMPDIR/build/${FAKE_DIR}/coop" << 'EOF'
#!/bin/sh
echo "MARKER: fake-replacement-binary"
EOF
chmod +x "$TMPDIR/build/${FAKE_DIR}/coop"
(cd "$TMPDIR/build" && tar -czf "$FIXTURE/${FAKE_TARBALL}" "$FAKE_DIR")
(cd "$FIXTURE" && sha256sums_line "${FAKE_TARBALL}" > SHA256SUMS)

# ── Start local HTTP server ──────────────────────────────────────────────────

PICK_PORT='import socket; s=socket.socket(); s.bind(("",0)); print(s.getsockname()[1]); s.close()'
PORT="$(python3 -c "$PICK_PORT")"
(cd "$FIXTURE" && python3 -m http.server "$PORT" > /dev/null 2>&1) &
SERVER_PID=$!
BASE_URL="http://127.0.0.1:${PORT}"

# Wait for server readiness
for _ in $(seq 1 30); do
    if curl -fsS "${BASE_URL}/SHA256SUMS" > /dev/null 2>&1; then
        break
    fi
    sleep 0.1
done
if ! curl -fsS "${BASE_URL}/SHA256SUMS" > /dev/null 2>&1; then
    echo "FATAL: local HTTP server did not come up on ${BASE_URL}" >&2
    exit 1
fi

export COOP_UPDATE_API_BASE_URL="$BASE_URL"

# ── Test 1: success flow ─────────────────────────────────────────────────────

write_release_json \
    "$FIXTURE/repos/trailofbits/coop/releases/latest" \
    "$FAKE_TAG" \
    "$(full_assets_block)"

echo "==> Test 1: successful update replaces binary"
if "$COOP_BIN" update --yes > "$TMPDIR/t1.log" 2>&1; then
    out="$("$COOP_BIN" 2>&1 || true)"
    if echo "$out" | grep -q "MARKER: fake-replacement-binary"; then
        pass "update --yes replaces binary with release contents"
    else
        fail "update --yes left binary unchanged" "got: ${out}"
    fi
else
    fail "update --yes returned non-zero" "$(tail -5 "$TMPDIR/t1.log")"
fi

# ── Test 2: --check when already up to date ──────────────────────────────────

# Restore the real binary; point "latest" at its version.
cp "$RELEASE_BIN" "$COOP_BIN"
CURRENT_VERSION="$("$COOP_BIN" --version | awk '{print $2}')"
write_release_json \
    "$FIXTURE/repos/trailofbits/coop/releases/latest" \
    "v${CURRENT_VERSION}" \
    "[]"

echo "==> Test 2: --check reports up-to-date"
if out="$("$COOP_BIN" update --check 2>&1)"; then
    if echo "$out" | grep -qi "up to date"; then
        pass "--check emits up-to-date message"
    else
        fail "--check did not mention up-to-date" "got: ${out}"
    fi
else
    fail "--check exited non-zero" "$out"
fi

# ── Test 3: checksum mismatch leaves binary unchanged ────────────────────────

cp "$RELEASE_BIN" "$COOP_BIN"
ORIG_SHA="$(sha_of "$COOP_BIN")"

# Restore the newer-release fixture but corrupt SHA256SUMS.
write_release_json \
    "$FIXTURE/repos/trailofbits/coop/releases/latest" \
    "$FAKE_TAG" \
    "$(full_assets_block)"
echo "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  ${FAKE_TARBALL}" \
    > "$FIXTURE/SHA256SUMS"

echo "==> Test 3: checksum mismatch aborts"
if "$COOP_BIN" update --yes > "$TMPDIR/t3.log" 2>&1; then
    fail "update --yes should have failed on checksum mismatch"
else
    new_sha="$(sha_of "$COOP_BIN")"
    if [[ "$new_sha" == "$ORIG_SHA" ]]; then
        pass "checksum mismatch leaves binary unchanged"
    else
        fail "binary replaced despite checksum mismatch"
    fi
fi

# Restore valid SHA256SUMS for subsequent tests (not needed here — test 4 rebuilds).
(cd "$FIXTURE" && sha256sums_line "${FAKE_TARBALL}" > SHA256SUMS)

# ── Test 4: dev build refuses to self-update ─────────────────────────────────

echo "==> Building dev binary..."
# Force kind=dev rather than relying on git state. When CI runs on a tag
# matching the Cargo version (the release workflow's normal trigger),
# build.rs correctly bakes kind=release, which would defeat this test.
(
    cd "$PROJECT_DIR"
    COOP_FORCE_BUILD_KIND=dev cargo build --release --quiet
)
cp "$PROJECT_DIR/target/release/coop" "$TMPDIR/bin/coop-dev"

echo "==> Test 4: dev build refusal"
if "$TMPDIR/bin/coop-dev" update --yes > "$TMPDIR/t4.log" 2>&1; then
    fail "dev build should have refused"
else
    if grep -qi "dev build" "$TMPDIR/t4.log"; then
        pass "dev build refuses update"
    else
        fail "dev-build refusal message missing" "$(cat "$TMPDIR/t4.log")"
    fi
fi

# ── Summary ──────────────────────────────────────────────────────────────────

echo
echo "  $pass_count passed, $fail_count failed"
[[ $fail_count -eq 0 ]]
