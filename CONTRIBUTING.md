# Contributing to coop

Thanks for your interest in contributing to coop. This document covers how to
build the project, run its tests, and submit changes.

coop is a Rust CLI that orchestrates disposable VMs for running agent CLIs —
Firecracker microVMs on Linux, Lima on macOS. Because it drives real
virtualization backends, some tests only run on a host with the matching
backend. The sections below note where that applies.

## Prerequisites

- **Rust** via [rustup](https://rustup.rs/). The toolchain version is pinned in
  `rust-toolchain.toml` and installed automatically when you build.
- **Dev tools** at pinned versions — [prek](https://github.com/j178/prek) (git
  hooks), [taplo](https://taplo.tamasfe.dev/) (TOML formatting), and cargo-deny
  (supply-chain checks). Install them with:

  ```bash
  ./scripts/install-dev-tools.sh          # baseline
  ./scripts/install-dev-tools.sh --all    # also cargo-mutants, cargo-fuzz, kani
  ```

  The script pins the local dev-tool versions; bump a version there rather than
  installing a floating `latest`. CI pins its own copies of taplo and cargo-deny
  in `.github/workflows/ci.yml`, so keep those in sync when bumping.
- To run the full integration suite you need a working backend:
  - **macOS**: Apple Silicon with [Lima](https://github.com/lima-vm/lima)
    (`limactl` on your PATH).
  - **Linux**: x86_64 or arm64 with KVM access (`/dev/kvm`), `sudo`, and
    `curl`, `tar`, `e2fsprogs`.

  See [docs/getting-started.md](docs/getting-started.md#prerequisites) for the
  full backend requirements.

## Building

Clone the repository and build:

```bash
git clone https://github.com/trailofbits/coop
cd coop
cargo build --release
```

The binary lands at `target/release/coop`.

## Pre-commit hooks

Install the hooks once, then let them run on every commit:

```bash
prek install
```

The hooks run `cargo fmt -- --check`, `cargo clippy --all-targets --all-features
-- -D warnings`, `cargo test`, `taplo format --check` (TOML formatting, also
enforced by CI), and a set of file checks (trailing whitespace, end-of-file,
YAML, large files, merge conflicts). Run them by hand at any time with:

```bash
prek run --all-files
```

Fix every warning before committing. coop has a zero-warnings policy — clippy
runs with `-D warnings`, so a warning fails the build.

## Testing

### Unit tests

```bash
cargo test
```

Unit tests live in the library crate and cover the pure logic: config parsing
and validation, workspace sync argument construction, env merging, secret
routing, and the helpers the command handlers are built from. Test behavior,
not implementation — a test that breaks under a refactor but not a behavior
change is testing the wrong thing.

### Integration tests

The integration suite exercises the full VM lifecycle (setup → start → status
→ shell → guest environment → Docker → stop → destroy). It is too slow for the
pre-commit hooks, so run it before submitting a change.

Run it on **both platforms** — macOS/Lima and Linux/Firecracker — because the
two backends share an abstraction but exercise different code paths:

```bash
# Local (whichever backend this host provides) — builds and runs
./tests/run-integration.sh

# Remote host running the other backend — cross-compiles, copies, and runs
./tests/run-integration.sh --remote user@remote-host
```

When you add a command or a guest-visible change, consider whether it needs a
new integration test phase.

### Optional deeper checks

coop also carries mutation tests (`cargo-mutants`), fuzz targets
(`cargo-fuzz`), and formal proofs (`kani`). These are manual quality checks,
not required for every change. If you touch a logic-dense module — config
parsing, the JSONC reader, the arithmetic kernels — see
[docs/testing.md](docs/testing.md) for when and how to run them.

## Code style

- Format with `cargo fmt`; lint with `cargo clippy --all-targets
  --all-features -- -D warnings`. Both are enforced in CI.
- Lean on the type system to make illegal states unrepresentable rather than
  validating at runtime: parse untrusted input into strong types at the
  boundary, use newtypes over bare primitives that carry an invariant, and use
  enums for state rather than boolean flags.
- Use `thiserror` for library error types and `anyhow` for application-level
  errors. Attach context at boundaries, not at every `?`.
- Log with `tracing` (`error!`/`warn!`/`info!`/`debug!`), not `println!`.
  Tracing output goes to stderr.

[docs/code-style.md](docs/code-style.md) documents the project's conventions in
detail, including the Rust patterns reviewers look for.

## Commits

- Write commit subjects in the imperative mood, no more than 72 characters
  ("Add license field to Cargo.toml", not "Added..." or "Adds...").
- Keep each commit to one logical change.
- Work on a feature branch and open a pull request. Never push directly to
  `main`.

## Pull requests

1. Branch from the latest `main`.
2. Make your change, keeping commits focused.
3. Run the pre-commit hooks and the integration suite on both platforms.
4. Open a pull request describing what the change does. Describe the code as it
   stands — not discarded approaches or prior iterations — and use plain,
   factual language.
5. A maintainer will review. Address feedback with follow-up commits.

CI must pass before a pull request can merge. The
[CI workflow](.github/workflows/ci.yml) runs:

- **`cargo fmt -- --check`** — formatting.
- **`cargo clippy --all-targets --all-features -- -D warnings`** — lints.
- **`cargo test`** — unit tests.
- **`./tests/integration-update.sh`** and
  **`./tests/integration-uninstall.sh`** — the update and uninstall flows.
- **`cargo deny check`** — advisories, licenses, bans, and sources.
- **[zizmor](https://github.com/zizmorcore/zizmor)** — GitHub Actions security
  audit.

The full two-platform lifecycle suite is not run in CI — run it locally, as
described above.

## Reporting issues

Open an issue on the [issue tracker](https://github.com/trailofbits/coop/issues).
For bug reports, include the platform and backend, the command you ran, and the
output (coop's tracing output goes to stderr — `RUST_LOG=debug` adds detail).

If you believe you have found a security vulnerability, do not open a public
issue. Follow the private reporting process in [SECURITY.md](SECURITY.md).

## License

coop is licensed under the [Apache License 2.0](LICENSE). By contributing, you
agree that your contributions will be licensed under the same terms.
