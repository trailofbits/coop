# Design: Apple sandbox mutations — transactions, serialization, and maintenance

**Status:** implemented (runtime 0.2.0, protocol 2) · **Scope:** `macos/coop-sandbox` and `src/apple_container/`; the backend contract is in [`backends.md`](../backends.md), the security spec in [`trust-model.md`](../trust-model.md)
**Date:** 2026-09-26

---

## 0. TL;DR

- **Kept:** the direct `apple/containerization` runtime
  ([`apple-sandbox-runtime.md`](apple-sandbox-runtime.md)) and its security
  model. Nothing here removes a check.
- **Mechanisms:**
  - one runtime disk-publication path (`DiskUpdate`);
  - one protected resource update used in both directions
    (`update_resources`);
  - one boot-and-validate sequence with explicit host-key trust modes.

## 1. Invariants

Every mutation path must preserve these. New code that touches sandbox
lifecycle, disks, resources, or recovery is reviewed against them.

| ID | Invariant |
| --- | --- |
| INV-01 | A mutation never operates on an instance whose ownership or identity cannot be established. |
| INV-02 | A sandbox's disk is never replaced or resized while its VM owner can be using it. A prior "stopped" observation alone is not enough: the check and the change hold the same guard. |
| INV-03 | After recovery, the installed disk and the authoritative record describe the same committed operation. |
| INV-04 | An interrupted forward update or rollback stays recoverable; neither silently leaves Rust and Swift records inconsistent. |
| INV-05 | A rollback never overwrites a newer successful operation. |
| INV-06 | Mutations of one sandbox serialize at the runtime boundary; mutations of different sandboxes stay concurrent. |
| INV-07 | A new SSH host key is enrolled only on first provisioning or after a correlated, coop-authorized restore. A disk-generation increase alone is not authorization. |
| INV-08 | Runtime qualification, effective-configuration verification, and SSH readiness stay separate checks. |
| INV-09 | Maintenance runs trusted tools from a separate boot image, never programs from the guest-controlled target disk. |
| INV-10 | Cleanup and stopping remain available when qualification or safe SSH hand-out fails, subject to ownership checks. |

## 2. Responsibility boundary

| Concern | Authoritative component |
| --- | --- |
| VM owner, actual resources, disk files, disk generations | Swift runtime |
| Same-sandbox serialization and interrupted runtime-operation recovery | Swift runtime |
| Instance-to-sandbox mapping, requested policy, guest user, image aliases | Rust adapter |
| SSH pin and the authority to replace it | Rust adapter |
| Mutation identity and committed outcome | The runtime's `record.lastOperation`, correlated with the operation id in coop's journal |
| IP, PID, diagnostic strings | Observations only |

Requested CPU/memory (the sidecar) and effective CPU/memory (the runtime) stay
separate facts; the isolation gate compares them. coop keeps its own journal:
it authorizes restores and reconciles coop's metadata. Operation ids let it
tell its own committed change from anyone else's.

## 3. What was built

### 3.1 Runtime disk updates (`DiskUpdate.swift`)

`grow` and `restore` prepare the new disk on a scratch clone named per
operation (`.update-<op>.ext4`). They then publish through one path:

1. Stage the new record, with the prepared disk's inode, in
   `disk-update.pending.json`.
2. Rename the disk over `rootfs.ext4`. This is the commit point.
3. Write `record.json` and drop the staged copy.

Recovery (`DiskUpdate.settle`) runs from three places: the next guarded
operation on the sandbox, the owner's claim, or `reconcile`.

- **Staged inode installed:** it finishes step 3.
- **Otherwise:** it discards the scratch disk and keeps the old state.
- **Unreadable or self-contradictory staged state:** an error, never a guess.
  `delete` still works in that case.

`commit` publishes a committed disk the same way (`DiskCommit`): it stages
`disks/.pending-<name>.json` with the work disk's inode and metadata, renames
the disk into place (the commit point), then writes `<name>.json`. `reconcile`
settles staged commits before sweeping orphans (`settled-disk-commit`) and
keeps any disk whose staged commit it cannot resolve
(`unresolved-disk-commit`).

Readers resolve the committed view without writing, so a read never mutates
outside a guard. Restores staged by runtime 0.1.0 (`restore.pending.json`)
are still recovered. Growth keeps the disk generation (no re-enrollment);
restore increments it.

The guarantee covers process interruption at every publication boundary. It
does not claim sudden-power-loss durability: each rename is atomic, but the
steps are not synced as a group.

### 3.2 Runtime serialization (`FileLock`, `Sandboxes.mutating`, `Owner.claim`)

**Per-sandbox guard.** Every mutation of a sandbox holds its guard,
`locks/sandbox-<id>.lock`, across both its stopped check and its change. That
covers create, start (up to the launchd bootstrap), set, grow, commit,
restore, and delete.

**Owner coordination.**

- **Claiming.** An owner takes the same guard to claim its sandbox, however
  it was launched (`start` or a launchd respawn). It then takes `owner.lock`,
  settles any staged update, and releases the guard. So no VM boots mid-update.
- **After a claim.** An operation arriving after the claim sees the owner and
  refuses.
- **No deadlock.** `start` releases the guard before waiting for readiness,
  so it never waits on the owner while holding it.
- **`stop`.** It takes no guard: it changes no disk or record, and must work
  while an owner is starting.

**Committed disks.** Each committed disk has its own lock:

- publication and deletion hold it exclusively;
- a clone (`create --from-disk`, `restore NAME`) holds it shared.

A clone therefore always pairs a disk with its own metadata.

**Lock order.** The order is documented on `FileLock`:

1. `operations.lock`
2. sandbox guard
3. disk lock
4. `subnets.lock`
5. leaf locks, held briefly: record publication and maintenance.

All are flock(2) locks that the kernel releases when the holder dies. Lock
files are never deleted, so every process locks the same inode across a
delete.

**Concurrency.** Different sandboxes proceed in parallel. The reconcile sweep
still takes `operations.lock` exclusively, and skips any sandbox whose guard
is held.

### 3.3 Protected resource update (`update_resources` in `src/apple_container/mod.rs`)

One function carries both the forward change and the rollback:

1. Takes the instance lock and reconciles any earlier journal.
2. Confirms the sandbox is stopped.
3. Journals a `SetResources` entry (operation id and prior values).
4. Sends `coop-sandbox set --operation <id>`.
5. Requires the read-back to show both the target values and
   `lastOperation == id`.

**Rollback.** When a restart after a change fails, rollback uses the same
function with a precondition: the runtime's last committed operation must
still be the forward change, with its values. coop checks this, and the
runtime checks it again under its guard (`--expect-operation`).

**Outcomes that stay uncertain.** Each is reported as
`APPLE_OPERATION_UNCERTAIN`:

- a superseded change is left in place;
- a sandbox not confirmed stopped gets no rollback;
- a rollback that cannot be confirmed keeps its journal, which the next
  `coop start` reconciles from the runtime's record.

### 3.4 Restore and grow correlation

**Restore.** `restore` journals an operation id. coop re-pins the host key
after an interrupted restore only when the runtime's last operation is that
id and the generation rose.

**Grow.** `grow` also passes an id and checks it on read-back. coop keeps no
grow journal: the runtime's disk update is self-recovering, and nothing coop
records depends on disk size.

### 3.5 Maintenance image (`Maintenance.swift`, `image::maintenance_*`)

**What maintenance runs from.** Maintenance VMs boot a disposable clone of a
dedicated artifact: a small image (Ubuntu plus e2fsprogs) that
`coop-sandbox maintenance install` unpacks into `maintenance/`. The runtime
records it with:

- its recipe version (`image::MAINTENANCE_VERSION`) and content digest;
- a capacity sized from the image's own layers;
- a check that it holds the programs the scripts run.

It lives apart from the image store, so application-image size or deletion
cannot affect it.

**Installation.** `coop setup` builds the image with the stock builder (the
same base and apt sources as the instance image, so no new outbound URL). It
installs it when the runtime reports a different version, then deletes the
store copy.

**Failures and safety.**

- A missing artifact fails `grow`/`commit` before any disk changes.
- The maintenance VM stays networkless.
- The target disk is attached as data.
- The identity-reset script still refuses symlinked `/etc` and `/etc/ssh`.

### 3.6 Simplification

**Journal.** `operation.json` holds one tagged variant per operation (`Create`
and `Destroy` with their own stages, `SetResources`, and `RestoreDisk`)
instead of parallel `Operation`/`JournalOp`/`Stage` types. Every variant that
changes the sandbox carries its operation id.

**Sidecar.** There is no creation-state field.

**Boot sequence.**

- **Shared steps.** Provisioning, restart, and image verification share
  `Runtime::boot_validated` (owner answers, then the gate passes, then the key
  is read from the approved boot). Two helpers finish the sequence:
  `pin_host_key`, with an explicit `HostKeyTrust` (`Enroll`, `RequirePin`, or
  `ReenrollAfterRestore`), and `wait_for_ssh`.
- **What stays separate.** Owner responsiveness, the gate, and SSH readiness
  remain distinct steps.
- **Cleanup.** A disposable sandbox is deleted, and a failed instance boot is
  stopped and kept.

**Tests.** Backend tests moved to `src/apple_container/tests.rs`. This is
mechanical; the behavioral simplification is the smaller number of
mutation/recovery paths.

## 4. Validation

**Automated (CI, `apple-container` job on macOS):**

- `cargo clippy` and `cargo test` for both the default (Lima) build and
  `--features apple-container`;
- `swift test` for the runtime.

**Coverage added with this work:**

- **Runtime (`TransactionTests.swift`):**
  - failure injection at each publication boundary, and repeated recovery;
  - settle before commit reads the record;
  - unreadable and contradictory staged state;
  - concurrent restores of one sandbox, with no lost update;
  - guard exclusion per sandbox but not across sandboxes;
  - an owner claim that waits for the guard and blocks later mutations;
  - a guard released when its holding process is killed;
  - delete waiting for an in-flight mutation;
  - a clone waiting for a committed-disk publication;
  - the `--expect-operation` precondition;
  - growth without a maintenance image failing before publication;
  - reconcile settling a staged update at each boundary, skipping a guarded
    sandbox, and keeping the scratch disk of an unresolved update;
  - maintenance install input checks, the merged-/usr program check, and
    saturating layer sizes.
- **Adapter (`tests.rs`, `state.rs`):**
  - resource-change recovery on both sides of the runtime update and the
    sidecar write;
  - rollback refused over a newer change;
  - no rollback without a confirmed stop;
  - an interrupted rollback reconciled from its journal;
  - each rollback precondition term refusing on its own;
  - grow and restore accepted only with their own committed operation;
  - restore re-pinning only for coop's own operation;
  - destroy working when the runtime cannot inspect the sandbox;
  - maintenance install, reinstall, and failure cleanup in setup.
- **Real hardware (`tests/integration-apple-sandbox.sh`):**
  - maintenance install, and survival after its store image is deleted;
  - two concurrent grows of one sandbox applying once;
  - a start racing a grow serializing to one valid outcome;
  - `grow`, `commit`, and `restore` clients killed at fractions of their uninterrupted duration,
    each reconciling to one committed state;
  - `coop restore` and `coop resize --size` killed partway, with the next
    `coop start` recovering;
  - an out-of-band runtime restore refused as host-key authorization.

**Not covered by automation:**

- an application image over 4 GiB of unpacked content;
- a deterministic crash between the disk rename and the record write inside
  a real grow (covered at unit level by failure injection; the real-hardware
  kills land wherever the delay falls);
- sudden power loss.

**Recorded run.** The latest real-hardware run of this design:

| Field | Value |
| --- | --- |
| Tree | `2e1bf20` plus the expanded suite (uncommitted when run) |
| Hardware | Apple M5 Max |
| OS | macOS 27.0 (26A428) |
| Toolchain | `container` 1.4.1, `containerization` 0.45.0, Swift 6.4 |
| Command | `./tests/integration-apple-sandbox.sh` (all phases), then `--only recovery,coop` |
| Result | All phases: 154 passed, 1 failed, 1 skipped. The failure was the coop-phase canary check matching its own command line in the guest's sudo journal; with that fixed, `recovery,coop` passed 89 of 89. The skip is host services on the NAT gateway, reachable by design. |

Killed operations in the `recovery,coop` run: 3 of 7 grows, 5 of 7 commits,
and 2 of 7 restores had applied when killed; every one settled consistently.

The coop-level `./tests/run-integration.sh` (Lima, and Firecracker remotely)
was not run for this change.

Record results for each candidate revision (commit, hardware, OS,
runtime/dependency versions, commands, results) in the PR that changes the
runtime. The run above is not evidence for a later revision.
