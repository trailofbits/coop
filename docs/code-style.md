# Authoring and reviewing Rust code

These notes are coop's project-specific Rust conventions. They complement the
global Rust guidance (clippy lint policy, `thiserror`/`anyhow`, `tracing`,
newtypes, enums over bools) rather than restating it. The focus here is on
**using the type system to eliminate error states** — not on style. The
conventions and design lenses in the shared
[`review`](../.agents/skills/review/SKILL.md) workflow enforce these; the
[architecture doc](ARCHITECTURE.md) shows where the patterns already live in the
codebase.

Apply these patterns when they pay for themselves; skip them when a primitive is
genuinely fine. A type system that fights the reader is worse than one that lets
a bug through.

## Lean on the type system before lean on validation

The default move when you see a bug is to add a runtime check. The better move
is usually to change a type so the bug cannot be expressed. Before writing a
check or returning an error, ask: *can the function signature make this case
unreachable?*

- **Parse, don't validate.** A function that takes a `&str` and returns
  `Result<Url, _>` is better than one that takes a validated `&str` by
  convention. Downstream code should not re-check what an earlier layer proved.
  Convert untrusted inputs to strong types at the boundary; pass the strong type
  inward. coop does this pervasively in `config.rs` — value bounds live in
  newtype constructors, so `validate()` only checks environmental facts.
- **Smart constructors.** When an invariant can't be expressed structurally,
  wrap the type in a module-private struct and expose `fn new(...) -> Result<Self, Error>`.
  The invariant then holds by construction everywhere the type appears
  (`Hostname`, `SshUser`, `RepoSlug`, `EnvVarName`, `InstanceIndex`).
- **Make illegal states unrepresentable.** Two `Option<T>` fields that are
  always both-`Some`/both-`None` should be one `Option<(T, T)>`. A `bool` plus a
  payload meaningful only when the bool is true should be an `Option`. A
  `String` holding one of three values should be an enum.

## Type-state for lifecycles

coop orchestrates VMs through a sequence — `setup → start → shell → stop →
destroy`. Operations are only legal on certain states (you can't `shell` into a
stopped VM). When you find yourself writing `if self.state == State::Running
{ ... } else { return Err(...) }`, consider whether the state belongs in the
type rather than in a field. Two flavors, pick the lightest that works:

- **State enum with method gating.** An enum for the state, methods that
  pattern-match the variant and return an error for illegal transitions. Use
  when call sites are few and an explicit error is reasonable.
- **Type-state with phantom markers.** `Vm<Stopped>`, `Vm<Running>`, where
  `start(self) -> Vm<Running>` consumes the stopped value. Illegal transitions
  become compile errors. Use when the lifecycle is the *primary* abstraction a
  type exposes. coop uses this for `FirecrackerVm<Configured|Running>` (`vm.rs`)
  and for the `RunningInstance`/`StoppedInstance` liveness proofs (`backend.rs`)
  — don't reach for it on a type that mostly does something else.

## Newtypes that earn their keep

The global guidance says "newtypes over primitives." In practice the win comes
when:

- Two primitives of the same underlying type are easy to swap at a call site
  (`fn copy(src: PathBuf, dst: PathBuf)` — newtype the destination, or use a
  struct).
- A primitive carries an invariant (non-empty, valid UTF-8, an absolute path, a
  hostname). The newtype's constructor is the one place that invariant is
  checked.
- A primitive is a domain concept that shows up in many signatures (a VM name, a
  guest path, an SSH user). The newtype reads as documentation and resists drift
  — see `GuestPath`/`HostPath`, `ImageName`/`InstanceName`, `Sha256Hash`.

If a primitive appears in one place and crosses no boundary, leave it alone.
Wrapping `u8` because "newtypes are good" is noise.

## Error design

- Distinct failure modes → distinct enum variants. A function that can fail
  because the VM is missing *or* because SSH timed out should return an error
  type whose variants reflect that, so callers can branch without string
  matching.
- Attach context at boundaries, not at every `?`. Use `anyhow::Context` at the
  layer where an error becomes user-facing; let library code propagate clean
  variants. Re-wrapping at every level produces verbose, low-signal errors.
- `unwrap`/`expect`/`panic` are forbidden by the global lints in production
  paths. If you genuinely need one, the comment must explain *why the invariant
  holds*, not just what is unwrapped. "Safe because `parse` was called above" is
  a smell — restructure to carry the parsed value through.

## Other small idioms worth checking

- `&str` over `&String`, `&[T]` over `&Vec<T>`, `&Path`/`impl AsRef<Path>` over
  `PathBuf` in parameters — accepts more callers, costs nothing.
- `Cow<'_, str>` when a function sometimes returns a borrowed slice and sometimes
  an owned modification.
- `NonZeroU32` / `NonZeroUsize` when zero is a real invariant.
- `#[non_exhaustive]` on public enums/structs that may grow.
- `From`/`Into` for infallible conversions, `TryFrom`/`TryInto` for fallible.
  Don't write `fn from_x(...) -> Result<Self, _>` — that's `TryFrom`.
- Sealed traits when you publish a trait but want to control implementations.
- Absolute imports only — no relative (`..`) paths.

## Review checklist (in priority order)

Before reviewing, sync to latest remote (`git fetch origin`).

1. **Correctness against the spec.** Does the change do what was asked, including
   edge cases the author may not have surfaced? Run the relevant tests and
   re-read the diff against the request.
2. **Invariants in types vs. checks.** Scan for `bool` parameters, primitive
   types representing domain concepts, sentinel values (`-1`, `""`, `0` meaning
   "missing"), and `Option<Option<T>>`. Each is a candidate for a stronger type.
   Flag the ones with real payoff; don't demand a refactor for every primitive.
3. **Error paths.** Every `?` produces an error that bubbles somewhere. Is the
   eventual user-facing message specific enough to act on? Are distinct failures
   distinguishable without string matching?
4. **`unwrap`/`expect`/`panic`.** Forbidden by global lints, but easy to slip in.
   If one exists, the justification must be in a comment and must be load-bearing.
5. **API surface.** New public items: do they need to be public? Public types:
   `#[non_exhaustive]` where appropriate? Public functions: most general
   parameter types (`&str`, `&[T]`, `impl AsRef<Path>`) without overreaching?
6. **Tests cover behavior, not shape.** Refactoring the implementation shouldn't
   break tests if behavior is unchanged. Edge cases — empty inputs, boundaries,
   the error variants the code returns — should each have a test.
7. **Tracing.** New operations that can take time, fail, or alter state should
   log at an appropriate level (INFO for user-visible lifecycle, DEBUG for
   internals, WARN/ERROR for problems). No `println!`/`eprintln!` outside the
   CLI's intentional output. Tracing goes to **stderr**.
8. **Cross-platform.** Touching backend-shared code? Confirm the abstraction
   still holds for both Firecracker and Lima. Integration tests must run on both
   platforms (see [`AGENTS.md`](../AGENTS.md) "Before committing").

## Authoring checklist

1. **Sketch the types first.** Write the signatures before the body. If they
   don't make the legal call sequences obvious, the types are wrong — fix them
   before the implementation locks them in.
2. **Take the smallest input you need.** `&str` not `String`, `&Path` not
   `PathBuf`, `&[T]` not `Vec<T>`. Owning is the caller's choice.
3. **Return owned values; let callers borrow.** The reverse forces lifetimes
   through the call graph.
4. **One `?` per error category.** Five `?`s that all produce different
   user-meaningful errors want an error enum with five variants, not one
   `anyhow::Error` with five contexts.
5. **Resist the "configuration knob" reflex.** A new flag, env var, or option is
   a long-lived commitment. Add it only when a real caller needs it.
6. **Re-read your diff.** Read your own change as the reviewer would before
   pushing. Most cleanup happens here, not in review.
