# Design: Apple sandbox runtime — stock containers vs `containerization` vs a machine fork

**Status:** decided and implemented (`USE_DIRECT_CONTAINERIZATION`) · **Scope:** the VM runtime behind the `apple-container` build; the coop-side backend contract is in [`backends.md`](../backends.md) and [`trust-model.md`](../trust-model.md)
**Date:** 2026-09-26 · **Method:** the experiment in [`apple-sandbox-experiment.md`](apple-sandbox-experiment.md), run on macOS 27.0 (26A428), Apple M5 Max, `container` 1.4.1, `containerization` 0.45.0

---

## 0. TL;DR

- **Build coop's own runtime on `apple/containerization`.** That runtime is
  [`macos/coop-sandbox`](../../macos/coop-sandbox), and it replaces the
  `container machine` fork previously vendored at `vendor/container`.
- **Stock Apple containers are secure but incomplete.** With systemd as
  PID 1, one network per container, and no host integration, they pass every
  isolation check. But no public surface sizes, grows, or resizes a container,
  or checkpoints one.
- **The direct runtime closes every gap in about 2.5k lines of Swift**
  (`Sources/`). That covers explicit disk sizes, offline growth, CPU/memory
  changes, commit and restore, and deterministic crash reconciliation. It
  forks nothing and uses only public API.
- **One accepted residual applies to every backend.** A root guest can reach
  host services listening on all interfaces through its NAT gateway.

## 1. Question

Can coop run each instance as an ordinary stock Apple container, keeping its
security model? If not, can a small runtime on `apple/containerization` do it
without forking `container machine`? Security dominated the decision. Stock was
preferred when complete, and a fork was the fallback only if the direct route
needed broad reimplementation.

## 2. Results

Both tracks ran the same OCI image (`sha256:3c8ada40…`). Stock is Track A, the
direct runtime Track B.

| Requirement | Stock container | Direct `containerization` |
|---|---|---|
| Standard OCI, one VM per sandbox, systemd, Docker in the guest | PASS | PASS |
| Dedicated network; root-guest peer isolation over IPv4/IPv6, including forged routes, neighbours, spoofed sources, and broadcast/multicast | PASS | PASS |
| No host mounts, SSH agent, published ports or sockets; canary secret never leaks | PASS | PASS |
| Effective-configuration inspection | PASS (`container inspect`) | PASS (owner-reported config) |
| Native bootstrap channel and pinned SSH | PASS (`container exec`) | PASS (vsock exec) |
| Stop/start persistence (20 cycles) | PASS; the address changes every restart | PASS; the address is stable |
| CPU/memory at create / changed later | PASS / GAP | PASS / PASS |
| Explicit disk size / disk growth | GAP / GAP: fixed 513 GiB sparse rootfs | PASS / PASS: 8–64 GiB, offline grow in ~1 s |
| Checkpoint/restore | GAP: `container export` fails on booted systemd images | PASS: APFS clone, ~50 ms |
| Crash reconciliation | PARTIAL: service restart untested (host-disruptive) | PASS |
| Concurrency (1/4/8), all ready from parallel start | PASS, 1.7–5.6 s | PASS, 1.3–1.7 s |
| Host services reachable via the gateway | PARTIAL: accepted, as on every backend | PARTIAL: same |

## 3. Findings that shaped the runtime

- **vmnet leaks a subnet when its owning process dies uncleanly.** The subnet
  stays reserved for hours, and the address is refused if recreated. The
  runtime quarantines a refused subnet and moves the sandbox to a free one:
  the address changes, the identity does not.
- **The VM lives in its owner process.** Virtualization.framework runs it
  in-process, so the owner runs as a launchd job and is respawned if it is
  killed. No VM is ever orphaned; a killed owner is a power-off, from which
  journaled ext4 recovers.
- **`containerization`'s ext4 formatter uses `sparse_super2`.** The guest
  kernel cannot resize that online, so growth runs `e2fsck`/`resize2fs` in a
  short maintenance VM. That VM boots a small maintenance image coop builds for
  the purpose (installed apart from the image store), never the guest's own
  disk or an application image, so a root guest cannot subvert it and an
  image's size or deletion cannot break it. The same VM strips host keys and
  machine-id from committed disks.
- **Docker's overlayfs snapshotter cannot run on an overlayfs root.** The
  read-only-image plus writable-upper disk model was therefore rejected in
  favour of one mutable rootfs per sandbox.
- **Stock `container export` fails on a booted systemd/Docker rootfs** (`could
  not read block N`). It works on plain Alpine. Stock containers therefore
  have no usable checkpoint path.
- **Upstream moves quickly.** `containerization` 0.46.0 built the runtime
  unchanged, and 0.47.0 removed two `Configuration` fields it read. The package
  is pinned exactly and bumped deliberately behind
  [`tests/integration-apple-sandbox.sh`](../../tests/integration-apple-sandbox.sh).

## 4. Consequences

- The `apple-container` build drives `coop-sandbox` over a versioned JSON CLI
  (protocol 1, now 2), with the isolation gate, journal, and host-key pinning of the
  earlier backend carried over. It gains `resize --size`, `commit`, and
  `restore`.
- Stock `container` 1.4.1 remains a prerequisite, but only to build images and
  supply the pinned guest kernel.
- Real-hardware coverage is `tests/integration-apple-sandbox.sh`. Unit tests
  (Rust and `swift test`) run in CI; the VM-booting suite cannot.

## 5. Sign-off

Approved 2026-09-26:

- `coop-sandbox init` fetches the digest-pinned init image (`vminit`) from
  ghcr.io on first use, a new outbound fetch recorded in
  [`trust-model.md`](../trust-model.md).
- The `vendor/container` fork, its build script, and its contract test are
  removed; `destroy` still clears instances the fork created.
- The x86_64 Firecracker integration run is waived for this change because
  the host was unavailable. The change is not confined to the
  `apple-container` build: it also modifies shared lifecycle, SSH, and proxy
  code (`coop stop`, `list`/`status` probe errors, ssh/rsync quoting and
  `HostKeyPolicy`, capability gates, proxy tunnels, the Lima disk resize), so
  the waiver accepts that those paths are unverified on Firecracker.
