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

You can also run the suite directly if you already have a binary:

```bash
./tests/integration.sh --binary /path/to/coop --full
```

The test exercises the full VM lifecycle (setup → start → status → shell →
guest environment → docker → stop → destroy). CI additionally runs the fast,
host-only `tests/integration-install.sh`, `tests/integration-update.sh`, and
`tests/integration-uninstall.sh` suites.

When adding new features, consider whether they should be covered here. New
commands or guest-visible changes are good candidates for a new test phase.

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
- `src/devcontainer.rs`, `src/guest_env_state.rs` — env merging and persistence
- `src/github_repo.rs`, `src/github_pat.rs`, `src/secret_store.rs` — slug
  parsing, secret routing
- `src/fs_util.rs` — path manipulation helpers
- `src/commands/` (`lifecycle.rs`, `profiles.rs`, `commands/devcontainer.rs`,
  `quickstart.rs`, `admin.rs`) — the pure helpers the command handlers were
  carved into: input-compatibility guards, summary/message builders, the
  `TranslatorInputs` builder, byte→GiB arithmetic kernels, and predicates like
  `discovered_local_devcontainer` / `is_sensitive_workspace`

**Don't bother with:** `backend.rs`, `lima.rs`, `setup.rs`, `update.rs`,
`shell.rs`, `port_forward.rs`, `cmd.rs`, `ssh.rs`, `vm.rs`, `prompt.rs` (TTY
prompts), `main.rs`, and — inside `src/commands/` — the `cmd_*` dispatch
entrypoints and the handlers that take a `&PlatformBackend`, write stdout, or
open a TTY prompt (e.g. `create_up_instance`, `restart_instance`,
`find_stopped_instance`, `resolve_running`, `resolve_devcontainer`,
`purge_all_data`, and `model.rs`'s `render_status`/`set_local`/`set_remote`/
`report_switch`/`apply_to_running`/`prompt_endpoint`), plus the `lib.rs`
`run`/`init_tracing` shims. These mostly shell out, run SSH, or talk to external
services — unit tests can't catch behavioral changes there. `tests/integration.sh`
does that job. This list is enforced (not just advised) by `.cargo/mutants.toml`
— see **Scoping** below.

### Scoping (`.cargo/mutants.toml`)

The mutation surface is curated in `.cargo/mutants.toml` so the `missed` list
means "real unit-test gap," not "code a `--lib` test structurally cannot reach."
cargo-mutants reads this file automatically on every run (`--list` included). It
scopes out three things:

- **The whole-module "Don't bother with" files above** (`main.rs`, and
  `prompt.rs` — every function short-circuits off a TTY and otherwise reads
  stdin, with no pure logic a `--lib` test can reach), via `exclude_globs`.
- **`cfg(kani)` proofs** (`config.rs mod proofs`), via `exclude_re = ["proofs::"]`
  — never compiled in a normal build, so every mutation is a silent no-op that
  always reports `missed`. They are exercised by `cargo kani`.
- **Individual shell-out / IO / terminal functions inside otherwise-logic-bearing
  modules** (`github_pat.rs`, `workspace.rs`, `devcontainer.rs`,
  `secret_store.rs`, `fs_util.rs`, `commands/model.rs`'s stdout/backend/TTY
  functions), via `exclude_re`. Each pattern is `\b`-anchored to a function name
  (or qualified `Type::method`) so it scopes the whole function without catching
  longer names that share a prefix. The module-agnostic `replace gh_auth_token ->`
  pattern also covers the identical `gh_auth_token` shell-out in
  `git_repo_devcontainer.rs`.
- **The `src/commands/` dispatch entrypoints and backend-driving / TTY handlers**,
  via `exclude_re`: a single `\bcmd_[a-z_]+\b` covers every `coop <subcommand>`
  entrypoint, plus `\b`-anchored names for the `&PlatformBackend` handlers
  (`create_*`, `restart_instance`, `start_instance`, `find_stopped_instance`,
  `resolve_running`, `preflight_start_target`, `current_disk_gib`, …), the IO
  handlers in `admin.rs`/`profiles.rs`/`commands/devcontainer.rs`/`quickstart.rs`,
  and the `lib.rs` `run`/`init_tracing` shims.

A cargo-mutants quirk to know about: `exclude_re` does **not** match `delete
field … from struct …` mutants — emitted for every struct literal that uses
`..Default::default()`, and no pattern filters them. In this crate they all
target `devcontainer::TranslatorInputs`, assembled in four places. The one pure
builder (`up_translator_inputs`) stays in scope and is unit-tested, which kills
its field-deletion mutants; the three shell-out handlers that build it inline
(`run`, `cmd_devcontainer_check`, `quickstart_fresh_start`) carry an in-source
`#[mutants::skip]` with a back-reference to `.cargo/mutants.toml`.

What is deliberately *kept* (a survivor here is a genuine coverage regression):
the pure-logic helpers the #321–#327 fixes carved the shell-out/IO functions
down to — `parse_curl_status_body`, `parse_user_login`, `github_pat.rs`'s
`render_status` (note `commands/model.rs` has a *different*, excluded
`render_status`, so its exclude is file-anchored), `parse_gh_token` /
`normalize_token`, `pick_backend`, `doc_contains_literal_token`, the SSH-config
marker-block helpers (`remove_marker_blocks` / `remove_named_marker_block` /
`remove_all_ssh_config_at` / `remove_ssh_config_at`), `CmdToken::from_words`'s
Linux/`op`/`cat` arms (only the macOS keychain arm is scoped, pinned on macOS by
`parse_recognises_macos_keychain`), `Report::push`, `atomic_write_with_mode`,
and the editor strategy helpers (`vscode_strategies` / `zed_strategies` /
`editor_strategies` / `install_hints` / `may_try_after_nonzero_exit`). The thin
wrappers those were split out of
(`probe_user_login`, `run_status`, `remove_*_ssh_config`, `gh_auth_token`) are
excluded — a `--lib` test can't reach them without a real `$HOME` or network.
When adding a new shell-out or IO function to one of these modules, add a
matching `exclude_re` line; when adding logic, leave it in scope.

The same split applies in `src/commands/`. Kept in scope: the
input-compatibility guards (`ensure_up_existing_inputs_are_compatible[_for_git_repo]`,
`up_has_restart_only_inputs`, `restart_has_ignored_creation_flags`,
`validate_copy_workspace_mounts`), the config-IO lookups
(`find_workspace_instance`, `find_git_repo_instance`), the message/summary
builders (`no_stopped_instance_message`, `creation_options_rejected_message`,
`builtin_summary`, `format_custom_summary`, `script_summary`), the
`up_translator_inputs` builder, the arithmetic kernels `bytes_to_gib` and
`format_dir_size`, `project_dir_to_str`, and the predicates
`discovered_local_devcontainer` / `is_sensitive_workspace`. The backend-driving
wrappers those kernels were carved out of (`current_disk_gib`,
`dir_size_display`) are excluded.

The `coop model` feature (#352) follows the same split. Kept in scope (and
unit-tested): `tools_needing_prompt`, `switch_report_lines`,
`ModelState::resolved_claude` / `resolved_codex` / `is_default` /
`load_or_default`, and `ModelMode::as_str`; plus `From<ModelAction> for
ModelMode` in `lib.rs`. Excluded as IO/backend/TTY: `model.rs`'s `render_status`
/ `write_tool_line` / `set_local` / `set_remote` / `report_switch` /
`apply_to_running` / `prompt_endpoint`, and `lifecycle.rs`'s
`bootstrap_and_post_start` / `prepare_session_from_target`.

**Keep `.cargo/mutants.toml` in sync in the same PR that adds the code** — this
is not a follow-up chore. #352 was merged without scoping its new IO/backend/TTY
functions, which silently broke the documented baseline and surfaced 22
survivors only at the next release preflight (#373). When a change adds a
function that shells out, drives a `&PlatformBackend`, reads a TTY, or writes
stdout, add its `exclude_re`/`exclude_globs` entry (and extract any pure logic
into a kept, tested helper) before merging. Verify with `cargo mutants -f
<touched files> -- --lib` — not just the `--in-diff` sweep, which only mutates
changed lines and so misses pre-existing same-class survivors in a touched file.
The [`mutation-check`](../.agents/skills/mutation-check/SKILL.md) skill walks
this workflow.

### Running it

Always scope with `-f`; all logic lives in the library crate, and every unit
test runs in the lib target, so pass `-- --lib`. (`-- --bins` runs zero tests
and reports every mutant as missed.)

```bash
# One file
cargo mutants -f src/config.rs -- --lib

# Several logic modules at once
cargo mutants -f src/config.rs -f src/workspace.rs -f src/devcontainer.rs -- --lib

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

- **2026-06-17 (after #329 scoping, #321–#330 fixes).** A sweep of the eight
  logic modules (`config.rs`, `workspace.rs`, `devcontainer.rs`,
  `guest_env_state.rs`, `github_repo.rs`, `github_pat.rs`, `secret_store.rs`,
  `fs_util.rs`) reports **0 missed**. Treat any *new* survivor as a coverage
  regression — first confirm it isn't a shell-out/IO function that belongs in
  `.cargo/mutants.toml`, then add a test.
- **2026-06-24 (issue #344).** A sweep of `lifecycle.rs`, `profiles.rs`,
  `commands/devcontainer.rs`, `quickstart.rs`, `admin.rs`, `commands/mod.rs`,
  `lib.rs`, and `jsonc.rs` reports **0 missed** out of 229 mutants. The
  non-caught results are `unviable` (~18) and `timeout` (~16–17, all `jsonc.rs`
  scanner-index increment mutants where mutating the step makes the loop never
  terminate).
- **2026-06-26 (issue #373).** After scoping the #352 local-model IO/backend/TTY
  functions and adding the `mode_as_str_round_trips`, `model_action_maps_to_mode`,
  and `load_or_default_returns_saved_state` tests, a sweep of
  `src/commands/model.rs`, `src/model_state.rs`, and `src/prompt.rs` reports
  **0 missed** (32 caught, 3 unviable), and a full `src/lib.rs` sweep reports
  **0 missed** (11 caught).

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
- `jsonc_to_json` — `coop::jsonc::jsonc_to_json`, fed hand-authored
  `devcontainer.json` text. Property: never panics.
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
