# Releasing coop

How a `coop` release is cut, and what to check before cutting one.

## How the automation works

- **`ci.yml`** runs on pushes to `main` and on every PR: `fmt --check`, `clippy -D warnings`,
  `cargo test --workspace`, the preflight/probe regression tests, Linux bridge
  isolation, `integration-proxy-forward.sh`, `integration-install.sh`, `integration-update.sh`,
  `integration-uninstall.sh`,
  `cargo deny --workspace check`, `taplo format --check`, and `zizmor`.
- **`release.yml`** runs when a `v*` tag is pushed. It **re-runs all of CI as a
  gate**, then builds both `coop` and `coop-proxy` on native runners for three
  targets
  (`aarch64-apple-darwin`, `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl`), checks the built CLI reports the tagged release
  version, packages both binaries per target, generates `SHA256SUMS`, attests
  build provenance, extracts the `## vX.Y.Z` section from `CHANGELOG.md` as the
  release notes, and publishes the GitHub release. **It fails if there is no
  matching CHANGELOG section.**

So pushing the tag is the release. Everything below is about making sure that
push succeeds and ships something correct.

## What runs where

| Check | CI (on PR + on tag) | `preflight-release.sh` | Manual judgement |
|-------|:---:|:---:|:---:|
| fmt / clippy / unit tests | ✓ | ✓ | |
| `cargo deny --workspace`, `zizmor`, `taplo` | ✓ | ✓ (if installed) | |
| Host-only integration and preflight/probe regression suites | ✓ | ✓ | |
| Version ↔ lock ↔ CHANGELOG ↔ tag agreement | | ✓ | |
| Release builds (3 targets) | native only | ✓ (per installed toolchain) | |
| Formal verification (`cargo kani`) | | ✓ (if installed) | |
| Full VM integration, both platforms | | ✓ (local + 1 remote) | pick remote host |
| Mutation testing (`--mutants`) | | opt-in | when logic changed |
| Fuzzing (`--fuzz`) | | opt-in | when a parser changed |

CI can't run the full VM integration suite or the extra-toolchain checks
(kani/mutants/fuzz); those are the preflight's job.

## Release checklist

1. **Land all release content on `main`.** Open PRs merged, `main` green in CI.

2. **Pick the version** (`X.Y.Z`, semver). Breaking changes → major; new
   features → minor; fixes only → patch. Look at the `## Unreleased` section of
   `CHANGELOG.md` to judge.

3. **Bump the version.**
   - Edit `[workspace.package].version` in `Cargo.toml`; both packages inherit it.
   - Run `cargo build --workspace` so `Cargo.lock` picks up both package versions.

4. **Promote the changelog.** Rename `## Unreleased` to `## vX.Y.Z` in
   `CHANGELOG.md`. The text under it becomes the GitHub release notes verbatim,
   so read it as release notes. Start a fresh empty `## Unreleased` above it.

5. **Run the preflight.** It refuses to pass until the version sources agree and
   the tag is free, then runs the full gate:

   ```bash
   ./scripts/preflight-release.sh
   ```

   A successful exit with warnings is incomplete validation: resolve skipped
   tools, targets, and platform gates before tagging.

   The full VM suite runs on **this machine** (one platform) plus **one remote
   host** you give for the other platform — so run the preflight from a
   macOS/Lima box and point `--remote` at a Linux/Firecracker box, or vice
   versa. It prompts for the host, or pass it up front:

   ```bash
   ./scripts/preflight-release.sh --remote you@other-platform-box
   ```

6. **Run the deep checks when the diff warrants it** (these are slow and not CI
   gates — see `AGENTS.md`):
   - `--mutants` when this release changed logic-dense modules (config,
     workspace, devcontainer, parsing, secret routing).
   - `--fuzz` when it changed a parser of user-editable input
     (`parse_repo_slug`, `jsonc_to_json`, `config_load`).

   ```bash
   ./scripts/preflight-release.sh --remote you@other-platform-box --mutants --fuzz
   ```

7. **Open the bump PR** (`Cargo.toml`, `Cargo.lock`, `CHANGELOG.md`), get it
   reviewed, and merge to `main`. Never push the bump straight to `main`.

8. **Tag the merge commit and push.**

   ```bash
   git checkout main && git pull
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

   This triggers `release.yml`.

9. **Verify the published release.** On the GitHub release page confirm:
   - three `coop-vX.Y.Z-<target>.tar.gz` artifacts, each containing both `coop`
     and `coop-proxy`, plus `SHA256SUMS` and `attestations.jsonl`,
   - the build-provenance attestation is attached,
   - the notes match the `## vX.Y.Z` CHANGELOG section.

   Then smoke-test the install path with credentials stripped, so the
   credential-free bundle verification is exercised as an external user sees
   it. Pin `VERSION` to the tag you just pushed rather than relying on
   "latest":

   ```bash
   env -u GH_TOKEN -u GITHUB_TOKEN GH_CONFIG_DIR="$(mktemp -d)" \
     VERSION=vX.Y.Z INSTALL_DIR="$(mktemp -d)" bash install.sh
   ```

   The run must print `Attestation verified against attestations.jsonl`. A
   "Could not use `attestations.jsonl`" line instead means the bundle could not
   be downloaded — the installer cannot tell a missing asset from a failed
   download, so confirm the asset on the release page (step 9's first bullet)
   before concluding it is missing.

## If the tag run fails

**Immutable releases are enabled org-wide, so a version cannot be recovered.**
Once `vX.Y.Z` is pushed, that version is spent: you cannot move or re-tag it and
re-run the release. A red `release.yml` run means you **bump to the next patch
version and cut a fresh release** — go back to step 2 with `vX.Y.(Z+1)`.

This is why the preflight matters: `release.yml` re-runs CI and then builds the
workspace for three targets, and a failure in *either* burns the version. Run
`./scripts/preflight-release.sh` before every tag — it mirrors the CI checks
**and** builds both workspace binaries for release targets locally (for each
rustup target you have installed; pass `--install-targets` to `rustup target add` any that are
missing — the cross-linker tools must already be installed), so build failures
can be caught before the tag. The release workflow uses
native macOS ARM64, Linux x86_64, and Linux ARM64 runners; Linux needs
`musl-tools` and `cmake` for the proxy's `aws-lc-sys` dependency, with `musl-gcc`
as its C compiler and Rust linker (see `release.yml`). Cross-building locally
also needs a C toolchain and linker configured for each target; installing the
Rust target alone is insufficient. Check any targets skipped by the preflight
on matching hosts before tagging.

Do **not** attempt `git push origin :refs/tags/vX.Y.Z` to delete and reuse a
tag — immutable releases reject it, and reusing a spent version is not allowed.
