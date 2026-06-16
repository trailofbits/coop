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
| Formal verification (`cargo kani`) | | ✓ (if installed) | |
| Full VM integration, both platforms | | ✓ (over SSH) | decide hosts |
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

   It prompts for the `user@host` targets to run the cross-platform VM suite on
   — give it both a **Linux/Firecracker** host and a **macOS/Lima** host. Or
   pass them up front:

   ```bash
   ./scripts/preflight-release.sh --remote you@linux-box --remote you@mac-box
   ```

6. **Run the deep checks when the diff warrants it** (these are slow and not CI
   gates — see `CLAUDE.md`):
   - `--mutants` when this release changed logic-dense modules (config,
     workspace, devcontainer, parsing, secret routing).
   - `--fuzz` when it changed a parser of user-editable input
     (`parse_repo_slug`, `jsonc_to_json`, `config_load`).

   ```bash
   ./scripts/preflight-release.sh --remote you@linux-box --remote you@mac-box --mutants --fuzz
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
   - three `coop-vX.Y.Z-<target>.tar.gz` artifacts plus `SHA256SUMS`,
   - the build-provenance attestation is attached,
   - the notes match the `## vX.Y.Z` CHANGELOG section.

   Then smoke-test the install path (`install.sh`) against the new tag.

## If the tag run fails

`release.yml` re-runs CI before building, so a red tag run means a check the
preflight would have caught was skipped (or the build matrix failed). Delete the
tag, fix forward on `main`, and re-tag — don't move a published tag.

```bash
git push origin :refs/tags/vX.Y.Z   # delete remote tag
git tag -d vX.Y.Z                    # delete local tag
```
