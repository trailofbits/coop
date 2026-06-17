#!/usr/bin/env bash
set -euo pipefail

# Release preflight for coop.
#
# Runs every check that gates a release from one machine, mirroring the CI
# jobs (fmt, clippy, test, deny, the lightweight integration scripts) so a
# doomed tag is never pushed, and adds the checks CI does not perform:
#   - Cargo.toml / Cargo.lock / CHANGELOG / git-tag version agreement
#   - release builds of all three target binaries (CI builds only the native
#     target; a cross-compile break otherwise first surfaces on the tag, which
#     burns the version under immutable releases)
#   - formal verification (kani), and opt-in mutation testing and fuzzing
#   - the cross-platform VM integration suite, driven over SSH the same way
#     tests/run-integration.sh does (these need Firecracker/Lima hardware and
#     cannot run in GitHub-hosted CI)
#
# The full integration suite runs on THIS machine (one platform) plus one
# remote host you specify for the other platform.
#
# Usage:
#   ./scripts/preflight-release.sh [options]
#
# Options:
#   --remote USER@HOST   Run the full integration suite on this remote host (the
#                        other platform). Local always runs too. If omitted and
#                        running interactively, you are prompted for it.
#   --mutants            Run mutation testing on lines changed since the last tag.
#   --fuzz               Build and briefly run every fuzz target (needs nightly).
#   --install-targets    rustup target add any missing release targets (the
#                        cross-linker tools must already be installed).
#   --quick              Skip the slow gates: full integration, mutants, fuzz.
#   -h, --help           Show this help.
#
# Environment:
#   FUZZ_SECONDS   Per-target fuzz budget when --fuzz is set (default 30).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_DIR"

REMOTE=""
RUN_MUTANTS=0
RUN_FUZZ=0
RUN_INSTALL_TARGETS=0
QUICK=0

usage() {
  sed -n '/^# Release preflight/,/^# *FUZZ_SECONDS/p' "${BASH_SOURCE[0]}" |
    sed 's/^# \{0,1\}//'
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --remote) REMOTE="$2"; shift 2 ;;
    --mutants) RUN_MUTANTS=1; shift ;;
    --fuzz) RUN_FUZZ=1; shift ;;
    --install-targets) RUN_INSTALL_TARGETS=1; shift ;;
    --quick) QUICK=1; shift ;;
    -h | --help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
done

FAILURES=()
WARNINGS=()

section() { printf '\n========== %s ==========\n' "$1"; }
warn() {
  printf 'WARN: %s\n' "$1"
  WARNINGS+=("$1")
}
have() { command -v "$1" >/dev/null 2>&1; }

# step "Name" cmd args... — runs cmd, records a failure on non-zero exit but
# keeps going so one preflight run reports every problem.
step() {
  local name="$1"
  shift
  section "$name"
  if "$@"; then
    printf 'PASS: %s\n' "$name"
  else
    printf 'FAIL: %s\n' "$name"
    FAILURES+=("$name")
  fi
}

cargo_toml_version() {
  awk -F'"' '/^\[package\]/{p=1} p && /^version *=/{print $2; exit}' Cargo.toml
}

check_worktree() {
  local dirty=0
  if ! git diff --quiet || ! git diff --cached --quiet; then
    printf 'Tracked changes are uncommitted:\n'
    git status --short --untracked-files=no
    dirty=1
  fi
  if [[ -n "$(git ls-files --others --exclude-standard)" ]]; then
    warn "Untracked files present — confirm they are not meant for the release"
  fi
  return "$dirty"
}

check_versions() {
  local v tag lockv
  v="$(cargo_toml_version)"
  if [[ -z "$v" ]]; then
    echo "Could not read version from Cargo.toml" >&2
    return 1
  fi
  tag="v$v"
  printf 'Cargo.toml version: %s  (release tag: %s)\n' "$v" "$tag"

  lockv="$(awk '/^name = "coop"$/{getline; gsub(/version = "|"/, ""); print; exit}' Cargo.lock)"
  if [[ "$lockv" != "$v" ]]; then
    printf 'Cargo.lock coop version (%s) != Cargo.toml (%s) — run cargo build to refresh the lockfile\n' "$lockv" "$v"
    return 1
  fi

  # release.yml extracts notes with an exact whole-line match ($0 == "## vX.Y.Z"),
  # so the header must match exactly — a trailing date would pass a looser check
  # here yet make the release run find no notes. Mirror that exact-match.
  if ! grep -qxF "## $tag" CHANGELOG.md; then
    printf 'CHANGELOG.md has no exact "## %s" header — promote "## Unreleased" (header must be exactly "## %s", no trailing text)\n' "$tag" "$tag"
    return 1
  fi

  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    printf 'Tag %s already exists — bump the version in Cargo.toml first\n' "$tag"
    return 1
  fi

  printf 'Version sources agree; tag %s is free.\n' "$tag"
}

run_deny() {
  if ! have cargo-deny; then
    warn "cargo-deny not installed — supply-chain check skipped (CI still runs it; cargo install cargo-deny --locked)"
    return 0
  fi
  cargo deny check
}

run_zizmor() {
  if ! have zizmor; then
    warn "zizmor not installed — workflow audit skipped (CI still runs it; pipx install zizmor)"
    return 0
  fi
  zizmor .github/workflows/
}

run_kani() {
  if ! have cargo-kani; then
    warn "kani not installed — formal-verification proofs skipped (cargo install --locked kani-verifier && cargo kani setup)"
    return 0
  fi
  cargo kani
}

run_mutants() {
  if ! have cargo-mutants; then
    warn "cargo-mutants not installed — mutation testing skipped (cargo install cargo-mutants --locked)"
    return 0
  fi
  local lasttag
  lasttag="$(git describe --tags --abbrev=0 2>/dev/null || true)"
  if [[ -n "$lasttag" ]]; then
    printf 'Mutating src/*.rs lines changed since %s\n' "$lasttag"
    cargo mutants --in-diff <(git diff "$lasttag" -- 'src/*.rs') -- --lib
  else
    printf 'No prior tag found; mutating the logic-dense modules\n'
    cargo mutants \
      -f src/config.rs \
      -f src/workspace.rs \
      -f src/devcontainer.rs \
      -f src/guest_env_state.rs \
      -f src/github_repo.rs \
      -f src/github_pat.rs \
      -f src/secret_store.rs \
      -f src/fs_util.rs \
      -- --lib
  fi
}

run_fuzz() {
  if ! have cargo-fuzz; then
    warn "cargo-fuzz not installed — fuzzing skipped (cargo install cargo-fuzz --locked)"
    return 0
  fi
  cargo +nightly fuzz build
  local target
  for target in parse_repo_slug jsonc_to_json config_load; do
    cargo +nightly fuzz run "$target" -- -max_total_time="${FUZZ_SECONDS:-30}"
  done
}

# Release-build targets, matching the matrix in .github/workflows/release.yml.
RELEASE_TARGETS=(aarch64-apple-darwin x86_64-unknown-linux-musl aarch64-unknown-linux-musl)

# Report missing rustup targets and always explain how to install them.
# `rustup target add` installs only the std library — cross-LINKING also needs
# platform tools, so spell that out too. With --install-targets, run the
# install non-interactively; otherwise warn and continue (never block on stdin).
handle_missing_targets() {
  local targets=("$@")
  printf '\nMissing rustup targets for the release build: %s\n' "${targets[*]}"
  printf 'Install the standard libraries with:\n'
  printf '  rustup target add %s\n' "${targets[*]}"
  printf 'Cross-LINKING also needs platform tools (musl-cross on macOS; musl-tools\n'
  printf '+ gcc-aarch64-linux-gnu on Linux) — see .github/workflows/release.yml.\n'
  if [[ "$RUN_INSTALL_TARGETS" == 1 ]]; then
    rustup target add "${targets[@]}" || warn "rustup target add failed for: ${targets[*]}"
  else
    warn "missing release targets won't be built locally: ${targets[*]} — re-run with --install-targets (plus the cross-linker tools above) to cover them."
  fi
}

# Build each release target whose toolchain is installed. CI only builds the
# native target, so a cross-compile break otherwise first surfaces on the tag —
# which, with immutable releases, burns the version.
build_release_targets() {
  if ! have rustup; then
    warn "rustup not found — release targets not built locally; release.yml builds them on the tag (a failure there burns the version)."
    return 0
  fi
  local installed target built=0 missing=()
  installed="$(rustup target list --installed 2>/dev/null || true)"
  for target in "${RELEASE_TARGETS[@]}"; do
    grep -qx "$target" <<<"$installed" || missing+=("$target")
  done
  if ((${#missing[@]})); then
    handle_missing_targets "${missing[@]}"
    installed="$(rustup target list --installed 2>/dev/null || true)"
  fi
  for target in "${RELEASE_TARGETS[@]}"; do
    if grep -qx "$target" <<<"$installed"; then
      printf 'Building %s...\n' "$target"
      cargo build --release --target "$target" || return 1
      built=$((built + 1))
    else
      warn "release target $target not built locally (toolchain absent) — release.yml builds it on the tag, uncaught here."
    fi
  done
  if [[ "$built" -eq 0 ]]; then
    warn "no release targets built locally — cross-compile breakage won't surface until the tag is pushed. Run the preflight from a host that can cross-compile all three (macOS + musl-cross)."
  fi
  return 0
}

prompt_for_remote() {
  if [[ -n "$REMOTE" || "$QUICK" == 1 ]]; then
    return
  fi
  if [[ ! -t 0 ]]; then
    warn "No --remote host and not a TTY — the other platform's integration suite is NOT run. Pass --remote user@host, or run ./tests/run-integration.sh --remote user@host yourself."
    return
  fi
  printf '\nThe full suite runs here (this platform). Enter a remote user@host for\n'
  printf 'the other platform (Firecracker on Linux, Lima on macOS), blank to skip: '
  local line
  read -r line || true
  REMOTE="$line"
}

# ── Run ──────────────────────────────────────────────────────────

step "Working tree clean" check_worktree
step "Version consistency" check_versions
step "Format (cargo fmt --check)" cargo fmt -- --check
step "Clippy" cargo clippy --all-targets --all-features -- -D warnings
step "Unit tests" cargo test
step "Release target builds" build_release_targets
step "Supply chain (cargo deny)" run_deny
step "Workflow audit (zizmor)" run_zizmor
step "Integration — coop update" ./tests/integration-update.sh
step "Integration — coop uninstall" ./tests/integration-uninstall.sh
step "Formal verification (kani)" run_kani

if [[ "$RUN_MUTANTS" == 1 ]]; then
  step "Mutation testing" run_mutants
elif [[ "$QUICK" != 1 ]]; then
  warn "Mutation testing not run — pass --mutants if this release touches logic/parsing modules"
fi

if [[ "$RUN_FUZZ" == 1 ]]; then
  step "Fuzzing" run_fuzz
elif [[ "$QUICK" != 1 ]]; then
  warn "Fuzzing not run — pass --fuzz if this release touches parsers of user-editable input"
fi

if [[ "$QUICK" == 1 ]]; then
  warn "--quick: full cross-platform integration suite skipped"
else
  prompt_for_remote
  step "Full integration (local host)" ./tests/run-integration.sh
  if [[ -n "$REMOTE" ]]; then
    step "Full integration ($REMOTE)" ./tests/run-integration.sh --remote "$REMOTE"
  else
    warn "No remote host given — only the local platform's integration suite ran; the other platform was not covered."
  fi
fi

# ── Summary ──────────────────────────────────────────────────────

section "Summary"
if [[ ${#WARNINGS[@]} -gt 0 ]]; then
  printf 'Warnings (%d):\n' "${#WARNINGS[@]}"
  printf '  - %s\n' "${WARNINGS[@]}"
fi
if [[ ${#FAILURES[@]} -gt 0 ]]; then
  printf '\nFailed checks (%d):\n' "${#FAILURES[@]}"
  printf '  - %s\n' "${FAILURES[@]}"
  printf '\nPreflight FAILED — do not tag the release.\n'
  exit 1
fi
version="$(cargo_toml_version)"
printf 'All required checks passed for v%s.\n' "$version"
printf 'Next: tag v%s on the merge commit and push to trigger release.yml.\n' "$version"
