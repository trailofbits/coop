#!/usr/bin/env bash
set -euo pipefail

# Build once into a fresh artifact directory, before consumers isolate HOME.
# Usage: build-policy-binaries.sh OUTPUT_DIR [test-source homebrew official dev release-smoke]
# With no variants specified, build the four updater policy variants.
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
OUTPUT_DIR="${1:?usage: build-policy-binaries.sh OUTPUT_DIR [VARIANT ...]}"
shift
mkdir "$OUTPUT_DIR"
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"
if [[ $# -eq 0 ]]; then
    set -- test-source homebrew official dev
fi

cd "$PROJECT_DIR"
for variant in "$@"; do
    # Policy provenance and Cargo optimization are independent axes.
    unset COOP_PACKAGE_MANAGER
    profile=dev
    case "$variant" in
        test-source) kind=test; profile=release ;;
        homebrew) kind=test; export COOP_PACKAGE_MANAGER=homebrew ;;
        official) kind=release ;;
        dev) kind=dev ;;
        release-smoke) kind=release; profile=release ;;
        *) echo "Unknown policy variant: $variant" >&2; exit 2 ;;
    esac
    echo "==> Building $variant binary (Cargo profile: $profile)..."
    COOP_FORCE_BUILD_KIND="$kind" cargo build --locked --quiet --bin coop \
        --profile "$profile" --message-format=json-render-diagnostics > "$OUTPUT_DIR/build.json"
    # Cargo reports the actual executable even with CARGO_TARGET_DIR or a
    # configured target triple. Snapshot it before the next policy build.
    python3 - "$OUTPUT_DIR/build.json" "$OUTPUT_DIR/coop-$variant" <<'PY'
import json
from pathlib import Path
import shutil
import sys

messages = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
executables = [m["executable"] for m in messages
               if m.get("reason") == "compiler-artifact"
               and m.get("target", {}).get("name") == "coop"
               and m.get("executable")]
if len(executables) != 1:
    raise SystemExit(f"Expected one coop executable, got {executables}")
destination = Path(sys.argv[2])
with destination.open("xb") as output, open(executables[0], "rb") as source:
    shutil.copyfileobj(source, output)
destination.chmod(0o555)
PY
done
rm "$OUTPUT_DIR/build.json"
