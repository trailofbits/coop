#!/usr/bin/env bash
set -euo pipefail

# Install coop's development tools at pinned versions for local development.
#
# This installs the local dev tools at reviewed, pinned versions — bump a
# version here, review the diff, and re-run, never a floating "latest". The
# `=<version>` requirement pins the tool exactly; `--locked` additionally pins
# each tool's transitive dependency tree to its published Cargo.lock, so an
# install is reproducible. CI pins its own taplo and cargo-deny versions in
# .github/workflows/ci.yml — keep them in sync when bumping here.
#
# Usage:
#   scripts/install-dev-tools.sh          # baseline: git hooks + supply-chain check
#   scripts/install-dev-tools.sh --all    # also the heavy, occasional quality tools
#
# Requires a Rust toolchain (rustup); the version is pinned in rust-toolchain.toml.

# --- Pinned versions ---------------------------------------------------------
PREK_VERSION="0.4.8"           # git hook runner (prek install / prek run)
TAPLO_VERSION="0.10.0"         # TOML formatter (taplo format --check hook + CI)
CARGO_DENY_VERSION="0.19.9"    # supply-chain: advisories, licenses, bans, sources
CARGO_MUTANTS_VERSION="27.1.0" # mutation testing (manual, opt-in)
CARGO_FUZZ_VERSION="0.13.2"    # fuzzing (manual, opt-in; run targets on nightly)
KANI_VERSION="0.67.0"          # formal verification (manual, opt-in)

install_pinned() {
    local crate="$1" version="$2"
    echo ">> ${crate} =${version}"
    cargo install "${crate}" --version "=${version}" --locked
}

extra=false
case "${1:-}" in
    "") ;;
    --all) extra=true ;;
    -h | --help)
        sed -n '3,17p' "$0" | sed 's/^# \?//'
        exit 0
        ;;
    *)
        echo "unknown argument: $1" >&2
        echo "usage: $0 [--all]" >&2
        exit 2
        ;;
esac

if ! command -v cargo >/dev/null 2>&1; then
    echo "cargo not found on PATH — install Rust via https://rustup.rs first" >&2
    exit 1
fi

install_pinned prek "${PREK_VERSION}"
install_pinned taplo-cli "${TAPLO_VERSION}"
install_pinned cargo-deny "${CARGO_DENY_VERSION}"

if [[ "${extra}" == true ]]; then
    install_pinned cargo-mutants "${CARGO_MUTANTS_VERSION}"
    install_pinned cargo-fuzz "${CARGO_FUZZ_VERSION}"
    install_pinned kani-verifier "${KANI_VERSION}"
    echo ">> cargo kani setup"
    cargo kani setup
fi

echo "Done. Run 'prek install' once to enable the git hooks."
