# coop

Isolated VM environment for running Claude Code and Codex — Firecracker on Linux, Lima on macOS.

## Architecture

Rust CLI that orchestrates VM lifecycle: setup, start, shell, stop, destroy, status, logs.

Two backends, auto-detected by platform:
- **Linux**: Firecracker microVMs with KVM. Cross-compiled from arm64 macOS (`x86_64-unknown-linux-musl` via `musl-cross`).
- **macOS**: Lima VMs using Apple Virtualization.framework (`limactl`). Native arm64 binary.

Backend abstraction in `src/backend.rs` provides `SshTarget` and `Backend` enum. All SSH-based operations (config injection, workspace sync, VS Code) are shared across backends.

## Before committing

Pre-commit hooks (prek) run automatically: fmt, clippy, test, trailing whitespace, EOF fixer, large file check, merge conflict check. If hooks aren't installed, run `prek install`.

After hooks pass, run integration tests on **both platforms** — these are too slow for hooks:

```bash
# Local (macOS/Lima) — builds and runs automatically
./tests/run-integration.sh

# Remote (Linux/Firecracker) — detects remote arch, cross-compiles, copies, and runs
./tests/run-integration.sh --remote user@remote-host
```

## Testing

### Integration tests

Two scripts:
- `tests/integration.sh` — the test suite. Runs locally, requires `--binary`.
- `tests/run-integration.sh` — the runner. Builds, deploys (if remote), and invokes the test suite.

Run on **both platforms** before every commit:

```bash
# Local (macOS/Lima)
./tests/run-integration.sh

# Remote (Linux/Firecracker)
./tests/run-integration.sh --remote user@remote-host

# With options (forwarded to integration.sh)
./tests/run-integration.sh --remote user@remote-host --full
./tests/run-integration.sh --profile python,node --name my-test
```

You can also run the test script directly if you already have a binary:

```bash
./tests/integration.sh --binary /path/to/coop --full
```

When adding new features, consider whether they should be covered by the integration test. The test exercises the full VM lifecycle (setup → start → status → shell → guest environment → docker → stop → destroy). New commands or guest-visible changes are good candidates for new test phases.

### Mutation testing

Mutation testing finds unit tests that pass even when the code is broken — i.e. real behavioral gaps. We use [`cargo-mutants`](https://mutants.rs/). It's a manual quality check, not a CI gate.

**Install once:**

```bash
cargo install cargo-mutants --locked
```

**When to run.** After significant edits to a logic-dense module, or before refactoring one (capture surviving mutants first to know what behavior isn't pinned down). Don't run it routinely — runs take minutes per module.

**Where it pays off in this crate.** Mutation testing only earns its keep on code with branches, arithmetic, parsing, or state composition. In coop that means:

- `src/config.rs` — parsing, validation, defaults, env composition
- `src/workspace.rs` — rsync arg construction, mount-state record/remove
- `src/devcontainer.rs`, `src/guest_env_state.rs` — env merging and persistence
- `src/github_repo.rs`, `src/github_pat.rs`, `src/secret_store.rs` — slug parsing, secret routing
- `src/fs_util.rs` — path manipulation helpers

**Don't bother with:** `backend.rs`, `lima.rs`, `setup.rs`, `update.rs`, `shell.rs`, `port_forward.rs`, `cmd.rs`, `ssh.rs`, `vm.rs`. These mostly shell out, run SSH, or talk to external services — unit tests can't catch behavioral changes there. `tests/integration.sh` does that job.

**Running it.** Always scope with `-f`; the crate is a binary so pass `-- --bins`:

```bash
# One file
cargo mutants -f src/config.rs -- --bins

# Several logic modules at once
cargo mutants -f src/config.rs -f src/workspace.rs -f src/devcontainer.rs -- --bins

# PR-scoped: mutate only lines changed vs main
cargo mutants --in-diff <(git diff origin/main -- 'src/*.rs') -- --bins

# Estimate cost without running
cargo mutants --list -f src/config.rs
```

A baseline run on `config.rs` (197 mutants) takes ~8 minutes on a workstation. Budget similarly for other modules and run them one at a time.

**Reading the output.** Results land in `mutants.out/` (gitignored):

- `caught.txt` — mutants tests killed (good)
- `missed.txt` — mutants tests didn't catch (the interesting ones)
- `unviable.txt` — mutants that broke the build (ignore; not test gaps)
- `timeout.txt` — mutants that hung (rare; bump `--timeout` if needed)

A kill rate around 70–80% on viable mutants is healthy for this code. Aim to drop the *number* of survivors, not chase 100% — many remaining mutants will be equivalent.

**Handling survivors.** For each line in `missed.txt`:

1. **Real test gap.** The mutation alters observable behavior and nothing fails. Add a test that distinguishes the mutant from the original (assert on the actual value, not just "it didn't panic"). Re-run on that file to confirm.
2. **Equivalent mutant.** The mutation doesn't change behavior any caller can observe. Common cases: `fmt::Display` impls returning `Ok(Default::default())`, getter functions returning `Default::default()` when the default happens to match the real value, constant accessors (`default_boot_args`, `mode_name`). Skip with an attribute and a one-line reason:
    ```rust
    #[mutants::skip] // equivalent: Display output isn't asserted by callers
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { ... }
    ```
3. **Dead code.** If the function is genuinely unused, delete it (per the global "replace, don't deprecate" rule). Surviving mutants on truly dead code are a useful smell.

**Baseline result on `config.rs` (recorded 2026-05-20):** 117 caught / 42 missed / 38 unviable. Real gaps were concentrated in `CoopConfig::validate` (5 survivors), `Instance::is_running` (4), `is_firecracker_process` (2), and `MiB::as_gib_f64` arithmetic. The rest were `fmt::Display` impls and default-value getters. Use this as a reference point — if a future run is much worse on these modules, treat it as a regression in test coverage.

### Fuzzing

Fuzzing is reserved for parsers of **untrusted or user-editable input** — it finds panics/hangs/OOM, not correctness (there's no oracle), so a standing harness only earns its keep where input crosses a trust boundary. Like mutation testing, it's a manual check, not a CI gate. We use [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer), which needs a nightly toolchain.

Targets live in `fuzz/fuzz_targets/`. Because `coop` is a binary-only crate (no lib target), a target cannot depend on it; instead it includes the module under test by `#[path]` (the `#[cfg(test)]` blocks stay inactive in a fuzz build). `fuzz/Cargo.toml` is its own workspace, so the main `cargo build`/`test`/`fmt`/`clippy`/`deny` never touch it.

**Install once:** `cargo install cargo-fuzz --locked`

```bash
cargo +nightly fuzz build                                  # compile all targets
cargo +nightly fuzz run parse_repo_slug                    # fuzz until a crash (Ctrl-C to stop)
cargo +nightly fuzz run parse_repo_slug -- -max_total_time=60   # bounded run
```

A crash is written to `fuzz/artifacts/<target>/`; reproduce it with `cargo +nightly fuzz run <target> <artifact-path>`.

**Current targets:**

- `parse_repo_slug` — `github_repo::parse_repo_slug_from_url`, fed `git remote get-url` output and `--git-repo` CLI args. Property: never panics.

## Known workarounds (revisit later)

The Firecracker CI kernel (`vmlinux-6.1.155`) is minimal and missing several modules. Two workarounds are applied in the guest install script (`src/setup.rs`, `guest_install_script()`):

1. **iptables-legacy** — The kernel lacks nftables support (`CONFIG_NF_TABLES` not set). Docker's default `iptables-nft` backend fails with "Protocol not supported". Fix: `update-alternatives --set iptables /usr/sbin/iptables-legacy`. A custom kernel with nftables enabled would remove this workaround.

2. **Static resolv.conf** — The CI rootfs ships `/etc/resolv.conf` as a symlink to systemd-resolved's stub (`127.0.0.53`), but `systemd-resolved` is not installed. DNS fails silently. Fix: replace the symlink with a static file pointing to `8.8.8.8`. Installing `systemd-resolved` in the guest would be the proper fix, letting systemd-networkd's DNS= directives propagate automatically.

Both could be resolved by building a custom Firecracker kernel with the needed netfilter modules enabled, rather than using the minimal CI kernel.

## Docker networking in the guest

The Firecracker CI kernel lacks the `iptable_raw` module (`CONFIG_IP_NF_RAW` not set). Docker 28+ uses the raw table for "direct access filtering" — a PREROUTING DROP rule that prevents direct routing to published container ports, ensuring traffic goes through Docker's port-mapping rules.

Without the raw table, Docker refuses to start bridge networking. The fix uses Docker 28.0.2's `DOCKER_INSECURE_NO_IPTABLES_RAW=1` env var (moby/moby#49621), set via a systemd drop-in at `/etc/systemd/system/docker.service.d/no-raw.conf`. This tells Docker to skip raw table rules while keeping full bridge networking: NAT, port mapping (`-p`), container-to-container communication, and embedded DNS all work normally.

The "insecure" label refers to the fact that without raw table rules, other hosts on the local network could route directly to published container ports even if they're bound to loopback. This is irrelevant here — the guest's only network neighbor is the Firecracker host, and the VM itself is the isolation boundary.

## scp tilde expansion (OpenSSH 9+)

Modern scp (OpenSSH 9+) uses SFTP by default, which does **not** expand `~` in remote paths. `scp file user@host:~/.claude/CLAUDE.md` silently creates a literal `~` directory instead of writing to the home directory.

Fix: `GuestPath` values use `./` instead of `~/` in remote paths (e.g., `GuestPath::new("./.claude")`). SFTP defaults to the user's home directory, so `./path` is equivalent to `~/path`. This convention is used in `scp_to` and `scp_to_recursive`.

SSH commands (`exec`) are unaffected — the remote shell expands `~` normally. Only scp's SFTP mode has this issue.

## Tracing output goes to stderr

The coop binary's tracing output (INFO/DEBUG/WARN logs) goes to **stderr**.

## Authoring and reviewing Rust code

These notes complement the global Rust guidance (clippy lints, `thiserror`/`anyhow`, `tracing`, newtypes, enums over bools). The focus here is on **using the type system to eliminate error states** — not on style. Apply these patterns when they pay for themselves; skip them when a primitive is genuinely fine. A type system that fights the reader is worse than one that lets a bug through.

### Lean on the type system before lean on validation

The default move when you see a bug is to add a runtime check. The better move is usually to change a type so the bug cannot be expressed. Before writing a check or returning an error, ask: *can the function signature make this case unreachable?*

- **Parse, don't validate.** A function that takes a `&str` and returns `Result<Url, _>` is better than one that takes a validated `&str` by convention. Downstream code should not have to re-check what an earlier layer already proved. Convert untrusted inputs to strong types at the boundary; pass the strong type inward.
- **Smart constructors.** When an invariant cannot be expressed structurally, wrap the type in a module-private struct and expose a `fn new(...) -> Result<Self, Error>`. The invariant then holds by construction everywhere the type appears.
- **Make illegal states unrepresentable.** Two `Option<T>` fields that are always both-`Some` or both-`None` should be one `Option<(T, T)>`. A `bool` plus a payload that's only meaningful when the bool is true should be an `Option`. A `String` that holds one of three values should be an enum.

### Type-state for lifecycles

This crate orchestrates VMs through a sequence — `setup → start → shell → stop → destroy`. Operations are only legal on certain states (you can't `shell` into a stopped VM). When you find yourself writing `if self.state == State::Running { ... } else { return Err(...) }`, consider whether the state belongs in the type rather than in a field.

Two flavors, pick the lightest that works:

- **State enum with method gating.** Cheap and clear: an enum representing the state, and methods that pattern-match the variant and return an error for illegal transitions. Use this when the call sites are few and an explicit error is reasonable.
- **Type-state with phantom markers.** `Vm<Stopped>`, `Vm<Running>`, where `start(self) -> Vm<Running>` consumes the stopped value. Illegal transitions become compile errors. Use this when the lifecycle is the *primary* abstraction a type exposes and call sites would benefit from compile-time enforcement. Don't reach for it on a type that mostly does something else.

### Newtypes that earn their keep

The global guidance says "newtypes over primitives." In practice the win comes when:

- Two primitives of the same underlying type are easy to swap at a call site (`fn copy(src: PathBuf, dst: PathBuf)` — newtype the destination, or use a struct).
- A primitive carries an invariant (non-empty, valid UTF-8, an absolute path, a hostname). The newtype's constructor is the one place that invariant is checked.
- A primitive is a domain concept that shows up in many signatures (a VM name, a guest path, an SSH user). The newtype reads as documentation and resists drift.

If a primitive appears in one place and crosses no boundary, leave it alone. Wrapping `u8` because "newtypes are good" is noise.

### Error design

- Distinct failure modes → distinct enum variants. A function that can fail because the VM is missing *or* because SSH timed out should return an error type whose variants reflect that. Callers can then handle them differently without string-matching.
- Attach context at boundaries, not at every `?`. Use `anyhow::Context` at the layer where an error becomes user-facing; let library code propagate clean variants. Re-wrapping at every level produces verbose, low-signal errors.
- `unwrap`/`expect` are forbidden by the global lints in production paths. If you genuinely need one, the comment should explain *why the invariant holds*, not just *what is being unwrapped*. "Safe because `parse` was called above" is a smell — restructure to carry the parsed value through.

### Other small idioms worth checking

- `&str` over `&String`, `&[T]` over `&Vec<T>` in parameters — accepts more callers, costs nothing.
- `Cow<'_, str>` when a function sometimes returns a borrowed slice and sometimes an owned modification.
- `NonZeroU32` / `NonZeroUsize` when zero is a real invariant (capacities, indices into a known-nonempty structure).
- `#[non_exhaustive]` on public enums and structs that may grow variants/fields; saves a breaking change later.
- `From`/`Into` for infallible conversions, `TryFrom`/`TryInto` for fallible. Don't write `fn from_x(...) -> Result<Self, _>` — that's `TryFrom`.
- Sealed traits when you publish a trait but want to control implementations.

### Review checklist (in priority order)

1. **Correctness against the spec.** Does the change do what was asked, including edge cases the author may not have surfaced? Run the relevant tests and re-read the diff against the request.
2. **Invariants in types vs. checks.** Scan for `bool` parameters, primitive types representing domain concepts, sentinel values (`-1`, `""`, `0` meaning "missing"), and `Option<Option<T>>`. Each is a candidate for a stronger type. Flag the ones with real payoff; don't demand a refactor for every primitive.
3. **Error paths.** Every `?` produces an error that bubbles somewhere. Is the eventual user-facing message specific enough to act on? Are distinct failures distinguishable by the caller without string matching?
4. **`unwrap`/`expect`/`panic`.** Forbidden by global lints, but easy to slip in. If one exists, the justification must be in a comment and must be load-bearing.
5. **API surface.** New public items: do they need to be public? Public types: are they `#[non_exhaustive]` where appropriate? Public functions: do they accept the most general parameter types (`&str`, `&[T]`, `impl AsRef<Path>`) without overreaching?
6. **Tests cover behavior, not shape.** Refactoring the implementation shouldn't break tests if the behavior is the same. Edge cases — empty inputs, boundaries, the error variants the code actually returns — should each have a test.
7. **Tracing.** New operations that can take time, fail, or alter state should log at an appropriate level. INFO for user-visible lifecycle, DEBUG for internals, WARN/ERROR for actual problems. No `println!`/`eprintln!` outside the CLI's intentional output.
8. **Cross-platform.** Touching anything backend-shared? Confirm the abstraction still holds for both Firecracker and Lima paths. Integration tests must run on both platforms per the "Before committing" section.

### Authoring checklist

1. **Sketch the types first.** Before writing the body, write the signatures. If the signatures don't make the legal call sequences obvious, the types are wrong — fix them before the implementation locks them in.
2. **Take the smallest input you need.** `&str` not `String`, `&Path` not `PathBuf`, `&[T]` not `Vec<T>`. Owning is the caller's choice.
3. **Return owned values; let callers borrow.** The reverse forces lifetimes through the call graph.
4. **One `?` per error category.** If a function has five `?`s that all produce different user-meaningful errors, the function probably wants an error enum with five variants, not one `anyhow::Error` with five contexts.
5. **Resist the "configuration knob" reflex.** A new flag, env var, or option is a long-lived commitment. Add it only when a real caller needs it. (See global "no speculative features.")
6. **Re-read your diff.** Before pushing for review, read your own change as if you were the reviewer. Most cleanup happens here, not in review.
