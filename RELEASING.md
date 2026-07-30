# Releasing coop

How a `coop` release is cut, and what to check before cutting one.

## How the automation works

- **`ci.yml`** runs on pushes to `main` and on every PR: `fmt --check`, `clippy -D warnings`,
  `cargo test`, `integration-update.sh`, `integration-uninstall.sh`,
  `cargo deny check`, and `zizmor`.
- **`release.yml`** runs when a `v*` tag is pushed. It **re-runs all of CI as a
  gate**, then cross-compiles the three target binaries
  (`aarch64-apple-darwin`, `x86_64-unknown-linux-musl`,
  `aarch64-unknown-linux-musl`), generates `SHA256SUMS`, attests build
  provenance, extracts the `## vX.Y.Z` section from `CHANGELOG.md` as the
  release notes, and publishes the GitHub release. **It fails if there is no
  matching CHANGELOG section.**

So pushing the tag is the release. Everything below is about making sure that
push succeeds and ships something correct.

## What runs where

| Check | CI (on PR + on tag) | `preflight-release.sh` | Manual judgement |
|-------|:---:|:---:|:---:|
| fmt / clippy / unit tests | ✓ | ✓ | |
| `cargo deny`, `zizmor` | ✓ | ✓ (if installed) | |
| `integration-update` / `-uninstall` | ✓ | ✓ | |
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
   - Edit `version` in `Cargo.toml`.
   - Run `cargo build` so `Cargo.lock` picks up the new `coop` version.

4. **Promote the changelog.** Rename `## Unreleased` to `## vX.Y.Z` in
   `CHANGELOG.md`. The text under it becomes the GitHub release notes verbatim,
   so read it as release notes. Start a fresh empty `## Unreleased` above it.

5. **Run the preflight.** It refuses to pass until the version sources agree and
   the tag is free, then runs the full gate:

   ```bash
   ./scripts/preflight-release.sh
   ```

   The full VM suite runs on **this machine** (one platform) plus **one remote
   host** you give for the other platform — so run the preflight from a
   macOS/Lima box and point `--remote` at a Linux/Firecracker box, or vice
   versa. It prompts for the host, or pass it up front:

   ```bash
   ./scripts/preflight-release.sh --remote you@other-platform-box
   ```

6. **Run the deep checks when the diff warrants it** (these are slow and not CI
   gates — see `CLAUDE.md`):
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
   - three `coop-vX.Y.Z-<target>.tar.gz` artifacts plus `SHA256SUMS` and
     `attestations.jsonl`,
   - the build-provenance attestation is attached,
   - the notes match the `## vX.Y.Z` CHANGELOG section.

   Then smoke-test the install path with credentials stripped, so the offline
   bundle verification is exercised as an external user sees it. Pin `VERSION`
   to the tag you just pushed rather than relying on "latest":

   ```bash
   env -u GH_TOKEN -u GITHUB_TOKEN GH_CONFIG_DIR="$(mktemp -d)" \
     VERSION=vX.Y.Z INSTALL_DIR="$(mktemp -d)" bash install.sh
   ```

   The run must print `Verifying attestation...` without a "No
   `attestations.jsonl` published" line — that line means the asset is missing
   and verification silently fell back to the credential-requiring API path.

## If the tag run fails

**Immutable releases are enabled org-wide, so a version cannot be recovered.**
Once `vX.Y.Z` is pushed, that version is spent: you cannot move or re-tag it and
re-run the release. A red `release.yml` run means you **bump to the next patch
version and cut a fresh release** — go back to step 2 with `vX.Y.(Z+1)`.

This is why the preflight matters: `release.yml` re-runs CI and then builds the
three target binaries, and a failure in *either* burns the version. Run
`./scripts/preflight-release.sh` before every tag — it mirrors the CI checks
**and** builds the three release targets locally (for each rustup toolchain you
have installed; pass `--install-targets` to `rustup target add` any that are
missing — the cross-linker tools must already be installed), so the
cross-compile matrix is exercised before the tag rather than after. Run it from
a macOS/Lima box with `musl-cross` set up to cover all three targets at once.

Do **not** attempt `git push origin :refs/tags/vX.Y.Z` to delete and reuse a
tag — immutable releases reject it, and reusing a spent version is not allowed.
