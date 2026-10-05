# Testing

coop has four test layers: integration tests (the primary gate), unit tests,
and three manual quality checks — mutation testing, fuzzing, and formal
verification (kani). Only the integration and unit tests run in CI; the other
three are manual, run when a change warrants them.

## Integration tests

VM integration uses two scripts:

- `tests/integration.sh` — the test suite. Runs locally, requires `--binary`.
- `tests/run-integration.sh` — the runner. Builds, deploys (if remote), and
  invokes the test suite.

Run on **both platforms** before every commit:

```bash
# Local (macOS/Lima) — builds and runs automatically
./tests/run-integration.sh

# Remote (Linux/Firecracker) — detects remote arch, cross-compiles, copies, runs
./tests/run-integration.sh --remote user@remote-host

# With options (forwarded to integration.sh)
./tests/run-integration.sh --remote user@remote-host --full
./tests/run-integration.sh --profile python,node --name my-test
```

Release preflight runs `--full --require-proxy` on each selected host. Use the
same flags to require the proxy build, host curl, the macOS Seatbelt profile,
and completion of the credential-proxy phase:

```bash
./tests/run-integration.sh --full --require-proxy
./tests/run-integration.sh --remote user@remote-host --full --require-proxy
```

Developer runs without `--require-proxy` may skip missing proxy prerequisites.
The runner consumes `--require-proxy`; it requires `--full` or `TEST_FULL=1`.

You can also run the suite directly if you already have a binary:

```bash
./tests/integration.sh --binary /path/to/coop --full
```

The test exercises the full VM lifecycle (setup → start → status → shell →
guest environment → docker → stop → destroy). CI additionally runs the fast,
host-only `tests/integration-install.sh`, `tests/integration-update.sh`, and
`tests/integration-uninstall.sh` suites.

The `--full` suite includes a dedicated `--no-github` phase. It captures the
boot session through `post_start` for fresh `up`, `start`, and a stopped-project
`up`, checks that model credentials still arrive, and witnesses normal GitHub
forwarding on an intervening invocation without the flag.

When adding new features, consider whether they should be covered here. New
commands or guest-visible changes are good candidates for a new test phase.

Run `python3 tests/test-integration-probes.py` for host-only regression tests
of Codex installer failure propagation, update/config assertions, address
discovery, ping result handling, and bounded HTTP retries. These use a
temporary loopback HTTP server and require Python 3, Bash, and curl;
Linux CI runs them. The full VM suite additionally checks these probes against
real guests. A host FORWARD policy other than ACCEPT still causes an explicit
skip of the routed guest-isolation probe, since it would mask the coop rule.

Run `python3 tests/test-codex-account.py` for the account wrapper's argument,
login/logout, API-key passthrough, and `codex-yolo` regressions (also in Linux
CI). To additionally test implicit daemon reuse with a real Linux Codex binary:

```bash
COOP_TEST_CODEX="$(command -v codex)" python3 tests/test-codex-account.py
```

This requires `dbus-run-session`, `gnome-keyring-daemon`, `secret-tool`, and
`strace`. It uses temporary homes and disposable keyring passwords, starts a
real app-server on a separate unusable keyring session, and observes terminal
socket connections. It checks that sign-in is reached without reusing that
server and that removing the wrapper override restores reuse. No account login
or real tokens are needed. Run it when upgrading Codex: daemon selection is
version-dependent. This opt-in test does not replace either VM backend gate.

The full Codex update tests install native release `0.153.0` before running
`codex update` as the guest user, and require the installed version to change.
They compare the actual `config.toml` contents across host updates, self-updates,
and migration from a profile-provided system command. The native installer owns
package validation; coop additionally verifies that the CLI and Code Mode host
are executable, exposed through stable system links, and resolve to the same
native release.

## Host-only bridge isolation test

`./tests/run-integration.sh --full` runs the bridge isolation gate before
the VM suite, on the selected local or remote host. A failure stops the full
run; macOS explicitly skips this Linux-only gate. `TEST_FULL=1` also enables
both gates. Remote full runs copy the tracked working-tree source and require
the build and namespace prerequisites below on the remote host.

Run `./tests/integration-network.sh` directly on Linux to test bridge-port
isolation without KVM or VM images. It builds a library test as the current
user, then uses passwordless sudo to run it in disposable network, mount, UTS, and PID
namespaces. Prerequisites are Rust/Cargo, Python 3, sudo, iproute2, iptables,
iputils-ping, util-linux, hostname, and coreutils. Missing prerequisites fail
the gate; macOS reports an explicit skip. Linux CI runs this gate.

Two veth-backed endpoints first communicate through a bridge with no firewall
rules. The test calls the production isolation helper on each bridge port:
one isolated port still permits communication, while two block peer traffic
in both directions and preserve gateway access. Removing isolation restores
communication. Ping execution errors fail the test rather than counting as
isolation. The runner bounds execution and destroys the namespace resources
on success, failure, or timeout.

This exercises the bridge mechanism shared by veths and TAPs. The existing
Firecracker `--full` phase checks actual VM TAP flags and both direct and routed
traffic; it also detects removal of the helper call from `setup_tap`. This
host-only gate does not replace Firecracker or Lima VM integration.

## Host-only proxy reverse-forward test

Run `./tests/integration-proxy-forward.sh` on Linux to exercise the production
reverse-tunnel startup against real OpenSSH. It authenticates with throwaway
keys, witnesses traffic through an accepted forward, then occupies the guest
loopback port and requires startup to return an error without publishing a PID
or leaving the SSH master alive. Separate host and guest network namespaces
allow the destination and reverse listener to use the same port.

The runner requires Rust/Cargo, Python 3, passwordless sudo, iproute2,
util-linux, coreutils, hostname, and OpenSSH client/server tools. It builds
unprivileged, then confines the fixture to disposable mount, network, UTS, and
PID namespaces. No user SSH configuration or keys are used. Namespace teardown
removes all children and temporary files on success, failure, or timeout.
Linux CI and release preflight run this gate explicitly; ordinary unit tests
mark it ignored, and macOS preflight reports it as unrun. This host test does
not replace the Firecracker and Lima VM integration gates.

The same fixture exercises guest environment forwarding through real OpenSSH:
literal values, empty values, transport-name collisions, PTYs, stdin, exit
status, missing forwarding, and redacted assignment failures. Its sshd accepts
only `COOP_SSH_ENV_*`, so original guest names cannot satisfy the test by
bypassing the transport. The ordinary unit suite separately checks host
environment isolation on all four SSH launch paths and saved CLI guest environment values through a later session.

The forwarding code in `backend.rs` and `ssh.rs` is outside cargo-mutants'
normal scope. When changing it, deliberately restore direct guest-map
`Command::envs` use and separately remove guest restoration: the launch-path
and round-trip regressions must fail, respectively. Removing export diagnostic
redaction must fail the assignment-error regression. Restore the code and
rerun the tests after each check.

The filesystem-backed non-UTF-8 workspace test runs on Linux; macOS APFS
rejects the fixture filename. The Lima resize spawn-failure test runs in an
isolated child process with an empty executable search directory, so it cannot
find a host `truncate` or change another test's environment.

## Host subprocess boundary tests

Changes that route project or guest configuration into host launchers need both
an intended guest result and evidence that the host launch context is unaffected.
A successful guest `printenv` or an assertion on the forwarding map alone does
not establish isolation. Trace the input from local/fetched project parsing
through config merging and saved-state replay to all applicable launch variants
(interactive, non-interactive, stdin, and output capture).

Use disposable fixtures with no real credentials or user configuration. For
executable lookup, put a marker-writing replacement tool in a project-controlled
directory and verify it never runs on the host. Pair that negative check with a
positive witness that the intended launcher ran and the guest received the
value. Inspect the child environment for loader and tool-control names too;
using an absolute executable does not cover those controls. A fake launcher can
observe host isolation, while a real transport fixture must verify guest
restoration. Keep platform-specific controls tied to the platform that consumes
them and report missing platform coverage.

Deliberately reintroduce the unsafe source-to-sink connection and require the
host-isolation assertion to fail; separately break guest delivery and require
the positive witness to fail. Do this even when the launch code is excluded
from cargo-mutants. Run such checks only in an authorized test environment;
read-only CI review must report them as unrun when contributor execution is
forbidden. The concrete forwarding checks above implement this pattern for SSH.

For file-transfer changes, extend the fixture through the later host operation
that consumes the transferred data. Use the relevant real tool to exercise
implicit file discovery, with a disposable destination and no real credentials
or user configuration. Pair an assertion on unintended host effects with a
positive witness that the intended transfer or rejection and consumer check
occurred. Separately break the boundary guard and the intended outcome to prove
both assertions work. Apply the execution restrictions above.

## Private storage checks

Unit tests cover private creation under permissive and restrictive umasks, atomic replacement,
legacy state repair, concurrent instance removal, unsafe links and parents,
Firecracker config creation/replacement, and Linux POSIX ACL removal.
The umask fixtures run in child processes to avoid changing other tests' umask. Two Linux unit probes require passwordless sudo:

```bash
cargo test --lib rejects_files_and_directories_owned_by_another_user -- --ignored
cargo test --lib unmount_rejects_name_swapped_to_outside_mount -- --ignored
```

The VM integration suite checks host directory, JSON state, template disk, and
instance disk modes after creation and after commit/restore on both backends.

On Linux hosts with passwordless sudo, e2fsprogs, and loop-mount privileges,
run `bash tests/privileged-disk.sh` after `cargo build --bin coop`. It exercises
the privileged disk helper with real formatting, loop mounts, cleanup, symlink
rejection, sparse copy, and reuse of staging data left by an interrupted copy.
It also corrupts an ext4 inode reference count, checks that read-only verification
rejects it, and repairs the filesystem before resizing. The repair operation
accepts exit code 1 (errors corrected), as defined by
[e2fsprogs 1.47.0](https://github.com/tytso/e2fsprogs/blob/v1.47.0/e2fsck/e2fsck.8.in).
Nonzero results from other disk tools, reboot-required results, error combinations,
and signals remain failures.
The ignored unmount probe swaps a checked mountpoint name to an outside-mounted
symlink between validation and `umount2`; it also checks a normal unmount.
This host probe does not replace either VM integration gate.

## Mutation testing

Mutation testing finds unit tests that pass even when the code is broken — real
behavioral gaps. We use [`cargo-mutants`](https://mutants.rs/). It's a manual
quality check, not a CI gate.

**Install once** — via `./scripts/install-dev-tools.sh --all`, or directly:

```bash
cargo install cargo-mutants --locked
```

**When to run.** After significant edits to a logic-dense module, or before
refactoring one (capture surviving mutants first to know what behavior isn't
pinned down). Don't run it routinely — runs take minutes per module.

**Where it pays off in this crate.** Only on code with branches, arithmetic,
parsing, or state composition:

- `src/config.rs` — parsing, validation, defaults, env composition
- `src/workspace.rs` — rsync arg construction, mount-state record/remove
- `src/guest_env_state.rs` — env merging and persistence
- `src/github_repo.rs`, `src/github_pat.rs`, `src/secret_store.rs` — slug
  parsing and secret routing
- `src/fs_util.rs` — path manipulation helpers
- `src/commands/` — pure input-compatibility guards, summary/message builders,
  byte-to-GiB arithmetic kernels, and predicates such as
  `is_sensitive_workspace`

**Don't bother with:** `backend.rs`, `completions.rs`, `lima.rs`, `setup.rs`,
`update.rs`, `shell.rs`, `port_forward.rs`, `cmd.rs`, `ssh.rs`, `vm.rs`,
`prompt.rs` (TTY prompts), `main.rs`, and — inside `src/commands/` — the
`cmd_*` dispatch entrypoints and handlers that take a `&PlatformBackend`, write
stdout, or open a TTY prompt. These mostly shell out, run SSH, or talk to
external services, so unit tests cannot observe their effects. The integration
suite covers those paths. This inventory is enforced by
`.cargo/mutants.toml`, not merely advisory.

### Scoping (`.cargo/mutants.toml`)

The mutation surface is curated in `.cargo/mutants.toml` so the `missed` list
means "real unit-test gap," not "code a `--lib` test structurally cannot reach."
cargo-mutants reads this file automatically on every run (`--list` included).
It scopes out:

- Whole IO/backend modules through `exclude_globs`, including `main.rs` and
  `prompt.rs`.
- `cfg(kani)` proofs through `exclude_re = ["proofs::"]`; normal builds never
  compile them, and `cargo kani` exercises them separately.
- Shell-out, filesystem, network, stdout, backend, and terminal functions in
  otherwise logic-bearing modules through `\b`-anchored `exclude_re` entries.
- The `src/commands/` dispatch entrypoints and backend-driving or TTY handlers,
  while leaving their extracted pure helpers in scope.

What is deliberately *kept* (a survivor here is a genuine coverage
regression) includes `parse_curl_status_body`, `parse_user_login`,
`parse_gh_token`, `pick_backend`, `doc_contains_literal_token`, the SSH-config
marker-block helpers, `CmdToken::from_words`, `atomic_write_with_mode`, and the
editor strategy helpers. The thin IO wrappers around them are excluded because
a `--lib` test cannot reach the real host filesystem, network, or launcher.

The same split applies in `src/commands/`. Kept helpers include
`ensure_up_existing_inputs_are_compatible[_for_git_repo]`,
`up_has_restart_only_inputs`, `restart_has_ignored_creation_flags`,
`find_workspace_instance`, `find_git_repo_instance`,
`no_stopped_instance_message`, `creation_options_rejected_message`, profile
summary builders, `bytes_to_gib`, `format_dir_size`, `project_dir_to_str`, and
`is_sensitive_workspace`. Their backend-driving wrappers remain excluded.

The `coop model` feature follows the same split. `tools_needing_prompt`,
`switch_report_lines`, `ModelState` resolution/default logic, and
`ModelMode::as_str` stay in scope and are unit-tested. The stdout, backend, and
TTY operations in `commands/model.rs` and lifecycle bootstrap remain excluded.

**Keep `.cargo/mutants.toml` in sync in the same PR that adds or removes the
code.** This is not a follow-up chore. Issue #373 showed that missing exclusions
for new IO/backend/TTY functions can silently turn the documented zero-missed
baseline into a list of non-actionable survivors. Add anchored exclusions for
IO, leave pure logic in scope, and cover it with discriminating assertions.
Verify with `cargo mutants -f <touched files> -- --lib`; an `--in-diff` sweep
only mutates changed lines and can miss pre-existing same-class survivors in a
touched file. The [`mutation-check`](../.agents/skills/mutation-check/SKILL.md)
skill walks this workflow.

### Running it

Always scope with `-f`; all logic lives in the library crate, and every unit
test runs in the lib target, so pass `-- --lib`. (`-- --bins` runs zero tests
and reports every mutant as missed.)

```bash
# One file
cargo mutants -f src/config.rs -- --lib

# Several logic modules at once
cargo mutants -f src/config.rs -f src/workspace.rs -f src/guest_env_state.rs -- --lib

# PR-scoped: mutate only lines changed vs main
cargo mutants --in-diff <(git diff origin/main -- 'src/*.rs') -- --lib

# Estimate cost without running
cargo mutants --list -f src/config.rs
```

A baseline run on `config.rs` (197 mutants) takes ~8 minutes on a workstation.

### Reading the output

Results land in `mutants.out/` (gitignored): `caught.txt` (killed — good),
`missed.txt` (not caught — the interesting ones), `unviable.txt` (broke the
build; ignore), `timeout.txt` (hung; rare). A kill rate around 70–80% on viable
mutants is healthy. Aim to drop the *number* of survivors, not chase 100% —
many remaining mutants are equivalent.

### Handling survivors

For each line in `missed.txt`:

1. **Real test gap.** The mutation alters observable behavior and nothing fails.
   Add a test that distinguishes the mutant from the original (assert on the
   actual value, not "it didn't panic"). Re-run to confirm.
2. **Equivalent mutant.** The mutation doesn't change behavior any caller can
   observe (`fmt::Display` returning `Ok(Default::default())`, getters returning
   a default that matches the real value, constant accessors). Skip with an
   attribute and a one-line reason:
   ```rust
   #[mutants::skip] // equivalent: Display output isn't asserted by callers
   fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { ... }
   ```
3. **Dead code.** If genuinely unused, delete it (per "replace, don't
   deprecate"). Surviving mutants on dead code are a useful smell.

### Baselines

Historical mutation results are snapshots of the code at the time. Re-run a
touched-file sweep before relying on one:

- **2026-06-17 (after #329 scoping, #321–#330 fixes).** The then-current logic
  module sweep reported zero missed mutants. For the surviving modules
  (`config.rs`, `workspace.rs`, `guest_env_state.rs`, `github_repo.rs`,
  `github_pat.rs`, `secret_store.rs`, and `fs_util.rs`), a new survivor remains
  a coverage regression unless it belongs to an IO function that should be
  scoped out.
- **2026-06-24 (issue #344).** The then-current commands and parser sweep
  reported zero missed mutants. Removed modules no longer apply; surviving
  pure command helpers retain the zero-missed expectation.
- **2026-06-26 (issue #373).** After scoping the local-model IO/backend/TTY
  functions and adding the missing model-state tests, the model sweep reported
  zero missed mutants (32 caught, 3 unviable), and a full `src/lib.rs` sweep
  reported zero missed mutants (11 caught).

## Fuzzing

Fuzzing is reserved for parsers of **untrusted or user-editable input** — it
finds panics/hangs/OOM, not correctness (there's no oracle), so a standing
harness only earns its keep where input crosses a trust boundary. A manual
check, not a CI gate. We use [`cargo-fuzz`](https://github.com/rust-fuzz/cargo-fuzz)
(libFuzzer), which needs a nightly toolchain.

Targets live in `fuzz/fuzz_targets/`. `coop` exposes a library target, so a
target depends on the crate directly and imports the parser under test with
`use coop::…` — no `#[path]` includes. `fuzz/Cargo.toml` is its own workspace,
so the main `cargo build`/`test`/`fmt`/`clippy`/`deny` never touch it.

**Install once** (or `./scripts/install-dev-tools.sh --all`): `cargo install
cargo-fuzz --locked`

```bash
cargo +nightly fuzz build                                       # compile all targets
cargo +nightly fuzz run parse_repo_slug                         # fuzz until a crash
cargo +nightly fuzz run parse_repo_slug -- -max_total_time=60   # bounded run
```

A crash is written to `fuzz/artifacts/<target>/`; reproduce with `cargo +nightly
fuzz run <target> <artifact-path>`.

**Current targets:**

- `parse_repo_slug` — `coop::github_repo::parse_repo_slug_from_url`, fed `git
  remote get-url` output and `--git-repo` CLI args. Property: never panics.
- `config_load` — `toml::from_str` into `coop::config::CoopConfig` then
  `validate`, fed `config.toml` text. Exercises the custom `Deserialize`/
  `visit_map` impls (`SubnetMask`, `HostInterface`, `PortForward`). Property:
  never panics, only returns `Err`.

## Formal verification (kani)

[Kani](https://model-checking.github.io/kani/) is a bounded model checker that
proves the *absence* of a property (here: arithmetic overflow / panics) over all
inputs in a range, rather than sampling like proptest. It is a **narrow fit** —
the type system already makes most illegal states unrepresentable, so kani earns
its keep only on bounded integer/float arithmetic. A manual check, not a CI gate;
it needs its own toolchain.

Proofs live in a `#[cfg(kani)]` module so the normal build never compiles them.
They run as one module in `src/config.rs`.

**Install once** (or `./scripts/install-dev-tools.sh --all`): `cargo install
--locked kani-verifier && cargo kani setup`

```bash
cargo kani                                            # run every proof harness (~5s)
cargo kani --harness disk_relative_add_never_wraps    # one harness
```

**Current proofs (`src/config.rs`, `mod proofs`):**

- `disk_relative_add_never_wraps` — the arithmetic kernel of `DiskSize::resolve`'s
  relative branch (`current.checked_add(delta)`): for any two non-zero `u32`
  sizes it yields `Some(current + delta)` exactly when the sum fits, and `None`
  otherwise — never wraps, never panics.
- `mib_as_gib_f64_is_finite_and_positive` — `MiB::as_gib_f64` is finite and
  strictly positive across the whole non-zero range.
- `instance_index_octet_stays_in_range` — the guest IP/MAC last octet
  (`index + 2`) stays in `2..=254` for every valid `InstanceIndex` (`0..=252`).

A note on the disk proof: the harness verifies the `checked_add` kernel directly
rather than calling `DiskSize::resolve`, because `resolve` wraps the overflow
case with `anyhow`'s heap-allocating error construction, which CBMC cannot model
tractably. `resolve` adds only that infallible `.context()` on top of the
kernel; its end-to-end behavior is pinned by the deterministic unit tests
`disk_size_resolve_relative` / `disk_size_resolve_relative_overflows`. This is
the general rule for kani here: prove the arithmetic kernel, not code paths that
route through `anyhow`/allocation. The `InstanceIndex` range is also pinned the
cheaper way by the exhaustive `0..=252` unit test
`instance_network_derivations_over_full_range`, which the kani harness
demonstrates rather than replaces.
