---
name: mutation-check
description: Run cargo-mutants for changed coop logic and keep .cargo/mutants.toml synchronized. Use when logic-dense modules change, before refactors, or when asked to verify mutation coverage.
---

# Mutation Check

Read [`docs/testing.md`](../../../docs/testing.md) first.

1. Inspect the diff before running. New functions in logic modules that shell
   out, drive `&PlatformBackend`, read a TTY, or write stdout must be excluded in
   `.cargo/mutants.toml` in the same PR. Extract and test their pure decision
   logic. Pure helpers remain in scope.
2. Sanity-check changed exclusions with `cargo mutants --list -f <file>`.
3. Run a full-file sweep for each touched scoped module and library tests only:
   `cargo mutants -f src/<file>.rs -- --lib`. Redirect output to a file; do not
   pipe a long run through `head` or `grep`.
4. Triage `mutants.out/missed.txt`: add a discriminating test for real gaps,
   mark genuinely equivalent mutants with a narrow documented skip, and delete
   dead code. Confirm each new test by re-running the mutant or deliberately
   breaking the protected behavior.
5. Report files swept, missed count before/after, every survivor's disposition,
   and whether `.cargo/mutants.toml` changed.

Do not spend a full mutation run on whole-module exclusions (`backend.rs`,
`lima.rs`, `setup.rs`, `update.rs`, `ssh.rs`, `vm.rs`, `network.rs`,
`port_forward.rs`, `cmd.rs`, `prompt.rs`, `main.rs`). Instead, identify the
unit/integration blind spot explicitly and test extracted pure logic directly.
