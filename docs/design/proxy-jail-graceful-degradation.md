# Proxy jail: graceful degradation on older Linux kernels

Implementation spec for a follow-up to **PR #417** (`issue-411-proxy-jail`,
issue #411). Self-contained: everything needed to implement is here.

## Branch / preconditions

- This work builds on the **`issue-411-proxy-jail`** branch, **not `main`**.
  The files below (`coop-proxy/src/jail.rs`, the proxy launcher, the trust-model
  additions) exist only on that branch. Start from it (branch off it or push a
  follow-up commit onto it, per whatever the PR state is when you pick this up).
- No new dependency. Uses the already-pinned `landlock = "0.4.5"`.
- Scope is **only** the Linux jail's compatibility handling plus its self-test
  and one doc paragraph. Do **not** touch the macOS/Seatbelt path, the host-side
  launcher's fail-closed logic, or any credential/proxy behavior.

## Problem

`coop-proxy` self-confines with Landlock. Today `coop-proxy/src/jail.rs`
(`linux::apply`) builds **one** ruleset at `ABI::V4` with
`CompatLevel::HardRequirement`, handles filesystem-write + `Execute` + TCP
`ConnectTcp | BindTcp`, and **bails unless the result is `FullyEnforced`**.

Landlock's TCP network rules require **ABI v4 = kernel ≥6.7** (Jan 2024).
Filesystem/exec rules require only **ABI v1 = kernel ≥5.13**. Because the
current check is all-or-nothing at v4, the proxy **fails closed on any host
kernel <6.7**, which includes a large share of production Linux hosts:
Debian 12 (6.1), RHEL/Alma/Rocky 9 (5.14), Amazon Linux 2023 (6.1), stock
Ubuntu 22.04 (5.15). On those hosts the user's only fallback is to disable proxy
mode entirely — which puts the raw API key *back into the guest*, strictly worse
than an unconfined proxy.

## Decision

**Graceful degradation, no escape hatch.** Keep the high-value denials
(filesystem-write + program-exec) as a **hard requirement** (kernel ≥5.13), and
make the **TCP network tier best-effort** so it is silently dropped on kernels
5.13–6.6. Only kernels <5.13 (pre-2021, EOL) fail closed.

Resulting kernel ladder:

| Host kernel | Filesystem-write | Program-exec | TCP egress scoping |
|-------------|:----------------:|:------------:|:------------------:|
| ≥ 6.7 (v4)  | denied           | denied       | scoped to :443/:53 (as today) |
| 5.13–6.6    | denied           | denied       | **not scoped** (open egress) |
| < 5.13      | **fail closed — proxy refuses to start** |||

We are **not** adding a `--no-jail` / `allow_unconfined` opt-out. Graceful
degradation covers every non-EOL kernel; keeping "the credential-holding proxy
never runs fully unconfined" intact is worth more than rescuing pre-2021
kernels. The open-egress rung is acceptable because the network tier is already
the weak, port-scoped (not host-scoped) layer the docs concede — upstream
identity is still enforced at the TLS layer in `coop-proxy`'s `proxy.rs`, and
the guest still cannot retarget it.

## Landlock 0.4.5 facts this design relies on

Verified against the v0.4.5 tag (docs.rs + `github.com/landlock-lsm/rust-landlock`).

1. **`set_compatibility` is a stateful, order-sensitive switch on the *builder*,
   not on access-right values.** `AccessFs`/`AccessNet` do not implement
   `Compatible`. You get per-tier behavior by interleaving
   `.set_compatibility(level)` between `handle_access`/`add_rule` calls. The
   crate's own `examples/sandboxer.rs` does exactly this (Hard for one tier,
   then BestEffort before `restrict_self`).

2. **`RulesetStatus` is a single ruleset-wide value** (`FullyEnforced` /
   `PartiallyEnforced` / `NotEnforced`) with **no per-access-type reporting.**
   **This is the trap:** in one mixed ruleset, when the best-effort net tier is
   dropped on a 5.13–6.6 kernel, the overall status is **`PartiallyEnforced`,
   not `FullyEnforced`**. So the current `if !FullyEnforced { bail }` would
   **wrongly fail closed on exactly the kernels we want to support.** The
   fail-closed guarantee must come from the **error channel**, not the status: a
   `HardRequirement` violation returns `Err(CompatError)` from
   `handle_access`/`create`/`restrict_self`; best-effort drops only lower the
   status and never error.

3. **No public runtime ABI detector.** `LandlockStatus::current()` is
   deliberately private; there is no `ABI::new_current()`. So "detect the kernel
   ABI and branch" is not available — per-tier compat levels are the only path.
   You *can* read the effective ABI **after** enforcement:
   `restrict_self()` returns `RestrictionStatus { ruleset, landlock:
   LandlockStatus, .. }` and `LandlockStatus::Available { effective_abi,
   kernel_abi }` reports what the running kernel supported. This is the hook the
   self-test uses to decide whether to expect net-scoping.

4. **Best-effort + unsupported right ⇒ the right is omitted and that action is
   left fully unrestricted.** Dropped TCP handling ⇒ TCP entirely open on that
   kernel. This is the intended 5.13–6.6 outcome.

5. **NetPort gotchas:** you must still **call `handle_access(AccessNet::…)`**
   (under BestEffort) before adding `NetPort` rules, or `check_consistency`
   returns `AddRuleError::UnhandledAccess` *regardless of compat level*
   (consistency is checked against *requested*, not kernel-*actual*, handled
   access). Also, `AccessNet::from_all(abi)` is **empty for abi ≤ v3** and
   `handle_access` of empty flags errors — pass the explicit
   `AccessNet::BindTcp | AccessNet::ConnectTcp`, never `from_all` for net.

## Design: two stacked Landlock rulesets

Landlock domains stack across successive `restrict_self()` calls. Build two so
each tier's status is checkable in isolation.

**Ruleset 1 — filesystem/exec floor (HardRequirement, kernel ≥5.13):**

- Compat level `HardRequirement`.
- Hard-handle the **ABI v1** write-family rights + `Execute`. Concretely, the
  hard floor is `(AccessFs::from_all(ABI::V1) & !AccessFs::from_read(ABI::V1))`
  for the write/make rights, plus `AccessFs::Execute` handled explicitly
  (`Execute` is in the read family, so it is excluded by `& !from_read` and
  **must** be handled separately — same reasoning as the existing code's
  comment; keep that comment).
- To preserve today's enforcement on new kernels, also handle the fs write
  rights **above v1** (refer/rename v2, truncate v3) as **BestEffort** — switch
  to `BestEffort` before handling them so a 5.13–6.1 kernel drops them instead
  of failing. Compute as `AccessFs::from_all(ABI::V4) & !AccessFs::from_all(ABI::V1)
  & !AccessFs::from_read(ABI::V4)` (the write-family rights introduced after v1).
- `create()`, `restrict_self()`.
- **Assert `status.ruleset == FullyEnforced`** here and `bail!` otherwise. This
  is meaningful because this ruleset's *hard* content is only the v1 fs+exec
  floor — a non-`FullyEnforced` result means a <5.13 kernel (or the v1 hard
  handles errored), which is the correct fail-closed case. (The best-effort v2/v3
  rights being dropped lowers status to `PartiallyEnforced` on 5.13–6.1, so do
  **not** assert `FullyEnforced` if you fold v2/v3 into this ruleset as
  best-effort — instead keep the v2/v3 best-effort rights in a *separate* stacked
  ruleset from the v1 hard floor, OR assert on the `Err` channel only. Prefer
  the cleanest option below.)

**Cleanest structure — three stacked rulesets, each single-purpose:**

1. **v1 fs+exec, HardRequirement** → assert `FullyEnforced`, else `bail!`
   (fail-closed on <5.13). Unambiguous because it contains only hard v1 content.
2. **v2/v3 extra fs write rights, BestEffort** → ignore status.
3. **TCP net (BindTcp/ConnectTcp) + NetPort rules, BestEffort** → ignore status;
   capture `effective_abi` from its `RestrictionStatus` for the self-test.

Three `restrict_self()` calls stack into one cumulative domain. This keeps every
`bail!`-worthy condition isolated to ruleset 1 and makes the invariant
("write+exec always denied on ≥5.13, else refuse to run") checkable by
construction. If you prefer two rulesets, fold v2/v3 into ruleset 1 but then you
**cannot** assert `FullyEnforced` there — you must rely solely on the `Err`
channel for fail-closed. The three-ruleset shape is preferred for clarity.

Keep the existing `bail!` message intent but update it: <5.13 is now the only
fail-closed kernel, and the message should say the filesystem/exec floor (v1,
kernel ≥5.13) could not be enforced.

## Self-test changes (`selftest` / `probe` in `jail.rs`)

The current `probe()` asserts, unconditionally: write BLOCKED, exec BLOCKED,
non-upstream TCP port BLOCKED, upstream :443 ALLOWED. With graceful degradation
the **"non-upstream port BLOCKED"** property only holds when the net tier is
active (≥6.7). On 5.13–6.6 that port is OPEN and the current assertion would
fail the integration self-test on those kernels.

Change:

- `apply` (or `selftest`) must surface whether the **net tier was actually
  enforced** — derive it from the `effective_abi` in ruleset 3's
  `RestrictionStatus` (net enforced iff `effective_abi >= ABI::V4`). Thread this
  boolean/enum out to `probe`.
- `probe` keeps write/exec/upstream assertions unconditional. Make the
  **non-upstream-port** expectation **conditional**: assert BLOCKED when net is
  enforced; assert (or simply skip / assert OPEN) when it is not. Log which mode
  it verified so the integration output is honest, e.g.
  `net-scoping=enforced|degraded`.
- Keep the existing `probe_fails_when_unconfined` unit test working — it runs
  unconfined so write/exec probes succeed and `probe` must still return `Err`.
  If you thread a "net enforced" flag into `probe`, that unit test should pass
  the value corresponding to "no net tier" (degraded) so its assertion still
  exercises the write/exec failure path.

## Doc change (`docs/trust-model.md`)

Update the **"Host-kernel floor / deprecated primitive"** bullet under the
credential-proxy jail section. It currently says an older host "fails closed."
Replace with the honest three-rung story:

- kernel ≥6.7: full jail (fs/exec + port-scoped TCP egress);
- kernel 5.13–6.6: filesystem-write + program-exec denied, **TCP egress not
  scoped** (open egress) — acceptable because the net tier is already the weak,
  port-scoped layer and upstream identity is TLS-pinned in the proxy;
- kernel <5.13: fails closed, the proxy refuses to start.

Keep it factual and plain. No "robust"/"comprehensive" language.

## Testing

- **Unit:** keep `probe_fails_when_unconfined`. Consider a small unit test
  around any pure helper you extract (e.g. a function mapping `effective_abi` →
  "net enforced" expectation).
- **Integration:** `coop-proxy --jail-selftest` already runs in
  `tests/integration.sh::test_proxy` on both backends. On CI Linux kernels ≥6.7
  it must still report full enforcement; the self-test must not regress there.
  There is no easy way to exercise a 5.13–6.6 kernel in CI, so the degraded path
  is covered by construction + the conditional-probe logic; call this out in the
  PR description rather than faking a kernel.
- Do **not** weaken any existing assertion for ≥6.7 — today's behavior must be
  byte-for-byte preserved on a modern kernel.

## Constraints / gates

- Rust 1.94.0, edition 2024. Absolute imports only.
- `cargo clippy --all-targets --all-features -- -D warnings` must pass; honor
  the repo lint policy (no `unwrap`/`expect`/`panic`/`todo`; `anyhow` in this
  binary). Use `let…else` / early returns; keep the happy path unindented.
- `cargo fmt -- --check`, `cargo test`, `cargo deny check`, `taplo format
  --check`, `prek run` all clean before opening the PR.
- Keep the module doc-comment at the top of `jail.rs` accurate — it currently
  describes an all-or-nothing v4 jail; update it to describe the tiered floor.
- One logical change. Update `CHANGELOG.md` if the PR is user-visible (the
  behavior on <6.7 hosts changes from "refuses to start" to "runs with fs/exec
  jail").

## Current code being modified (for reference)

`coop-proxy/src/jail.rs` → `mod linux` → `apply` on `issue-411-proxy-jail`:

```rust
pub fn apply(listen_port: u16) -> Result<()> {
    let abi = ABI::V4;
    let write_access = AccessFs::from_all(abi) & !AccessFs::from_read(abi);
    let status = Ruleset::default()
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(write_access)?
        .handle_access(AccessFs::Execute)?
        .handle_access(AccessNet::ConnectTcp | AccessNet::BindTcp)?
        .create()?
        .add_rule(NetPort::new(UPSTREAM_PORT, AccessNet::ConnectTcp))?
        .add_rule(NetPort::new(DNS_TCP_PORT, AccessNet::ConnectTcp))?
        .add_rule(NetPort::new(listen_port, AccessNet::BindTcp))?
        .restrict_self()?;
    if !matches!(status.ruleset, RulesetStatus::FullyEnforced) {
        bail!(/* kernel too old */);
    }
    Ok(())
}
```

(Error-context `.context(...)` calls elided above; preserve them.) Replace this
with the three-stacked-ruleset structure. `selftest()` calls
`#[cfg(target_os = "linux")] apply(8788)?; probe()` — keep that shape but let
`apply` return the net-enforcement signal (or expose it another way) so `probe`
can branch.
