# Experiment Specification: Stock Apple Containers vs Direct Containerization for coop

**Status:** Run and concluded: results and decision in [`apple-sandbox-runtime.md`](apple-sandbox-runtime.md); Track B's checks live on as [`tests/integration-apple-sandbox.sh`](../../tests/integration-apple-sandbox.sh)
**Target product:** macOS 27+ / Apple Silicon coop fork
**Objective:** Determine whether stock Apple `container` containers can replace a fork of `container machine`; if not, determine whether a small purpose-built runtime on `apple/containerization` can satisfy the end-state architecture without forking the full Apple Container machine subsystem.
**Date:** 2026-09-25

## Baseline versions

Primary baseline:

- macOS: **27.x**, Apple Silicon
- Apple `container`: **1.4.1**, released 2026-09-09
- `apple/containerization`: **0.45.0** (the version pinned by `container` 1.4.1)
- coop reference: `chr33s/coop` main at `db5eb22850880db671d82b787b61d8394fe6f256`

A second, non-decisive compatibility run MAY be made against current `apple/container` and `apple/containerization` main after the release-pinned experiment passes. Results from unreleased `main` MUST NOT be used to claim that the released runtime satisfies a requirement.

---

# 1. Question being answered

Can coop use **ordinary stock Apple containers** as its persistent VM sandbox primitive while preserving the existing security model and the proposed OCI-first end state?

If not, can coop satisfy the same requirements by using **Apple Containerization directly** through a small Swift runtime without adopting or forking the general-purpose `container machine` layer?

The experiment is not trying to prove that either approach is generally better. It is trying to identify the smallest runtime architecture that fully satisfies coop's requirements.

---

# 2. Competing architectures

## Track A — stock Apple containers

```text
coop experiment harness
        |
        | stock public CLI only
        v
Apple `container` 1.4.1
        |
        v
container-apiserver / runtime helpers
        |
        v
one Linux VM per ordinary container
```

The intended sandbox is created with a stock container roughly equivalent to:

```bash
container network create <unique-network>

container create \
  --name <sandbox> \
  --network <unique-network> \
  --cpus 4 \
  --memory 8g \
  --cap-add ALL \
  --masked-path NONE \
  --read-only-path NONE \
  --entrypoint /sbin/init \
  <coop-machine-image>
```

Notably absent:

```text
--ssh
--mount
--volume
--publish
--publish-socket
```

No patched Apple runtime is allowed in Track A.

## Track B — direct Containerization

```text
Rust experiment driver
        |
        | small versioned protocol or direct test invocation
        v
Swift sandbox runtime
        |
        v
apple/containerization 0.45.0
        |
        v
Virtualization.framework + vmnet
```

Track B uses the public Swift package directly and implements only the sandbox primitives coop needs.

It does **not** fork or import `container machine`.

---

# 3. Hypotheses

## H-A: stock containers are sufficient

An ordinary Apple container can behave as coop's persistent machine if the OCI workload uses systemd as its main process, receives the Linux capabilities needed for Docker, is attached to a unique custom network, and receives no host integration.

Expected advantages:

- no Apple runtime fork;
- standard OCI image lifecycle;
- stock update path;
- one VM per agent already provided;
- mounts, SSH-agent forwarding, and published ports are opt-in rather than automatic;
- machine security policy can be verified from `container inspect`.

Primary uncertainty:

- whether normal container lifecycle exposes enough **persistent-machine resource and disk management** for the desired coop end state.

## H-B: direct Containerization is sufficient

If stock containers are lifecycle-incomplete, `Containerization` provides enough lower-level primitives to implement a small coop-specific sandbox runtime without reimplementing a general container platform.

Expected advantages:

- explicit rootfs and writable-layer sizing;
- direct control of network interfaces and VM configuration;
- direct OCI/ext4/VM primitives;
- no unwanted general-purpose machine defaults;
- secure sandbox shape can be encoded in the API.

Primary uncertainty:

- how much lifecycle, persistence, networking, recovery, and service management coop must own itself.

---

# 4. Hard security invariants

Both tracks MUST satisfy every invariant below.

Failure of any invariant is a **hard failure** for that track unless the failure is clearly an experiment-harness bug.

## S1 — one VM per sandbox

Each coop sandbox MUST execute in a distinct Linux VM.

No shared Linux kernel between two coop sandboxes.

## S2 — dedicated network

Each sandbox MUST be attached to exactly one network unique to that sandbox.

The default/shared Apple network is forbidden.

## S3 — peer isolation

A root-controlled guest A MUST NOT reach guest B by:

- IPv4 TCP;
- IPv4 UDP;
- IPv4 ICMP;
- IPv6 TCP;
- IPv6 UDP;
- IPv6 ICMP;
- forged on-link routes;
- source-address spoofing;
- ARP/NDP/neighbor manipulation;
- broadcast/multicast where applicable.

The test must be performed on real macOS 27 hardware.

Guest firewall rules do not count as isolation.

## S4 — no host SSH agent

The guest MUST NOT receive:

- `SSH_AUTH_SOCK`;
- the host SSH agent Unix socket;
- an equivalent host signing proxy.

A root guest must not be able to discover or use one.

## S5 — no host filesystem exposure

The guest MUST NOT receive:

- `$HOME`;
- the project directory;
- arbitrary host directories;
- arbitrary host files;
- host credential sockets.

Runtime-internal bootstrap files are allowed only when their source is runtime-owned and tightly scoped.

## S6 — no runtime-published ports or sockets

The sandbox configuration MUST contain no host port publication and no host socket publication.

Later coop-managed SSH forwards are out of scope for the runtime primitive and are tested separately.

## S7 — explicit ownership

The harness MUST be able to distinguish its own resources from unrelated Apple Container resources.

Deletion MUST operate only on experiment-owned resources.

Use random IDs plus labels where available.

## S8 — pinned guest identity

The experiment MUST demonstrate a path to:

1. enroll a per-instance SSH client public key without relying on unverified SSH;
2. read the guest SSH Ed25519 host public key over a trusted native control path;
3. pin the key on the host;
4. reconnect with `StrictHostKeyChecking=yes`;
5. fail on host-key replacement;
6. never auto-reenroll.

For Track A, stock `container exec` may serve as the native enrollment/control path.

For Track B, `LinuxProcess`/guest RPC or an equivalent native control path may serve this role.

## S9 — no secret leakage through runtime

A canary secret supplied only to the host test process MUST NOT appear in:

- runtime process argv;
- runtime child environment unless explicitly required by the test;
- `container inspect`;
- runtime logs;
- OCI image layers;
- sandbox filesystem before explicit credential bootstrap.

## S10 — fail-closed effective inspection

Before the harness declares a sandbox safe, it MUST be able to read effective state sufficient to verify:

- sandbox identity;
- image identity;
- networks;
- mounts;
- SSH forwarding state or proof of absence;
- published ports;
- published sockets;
- CPU/memory configuration.

If effective state cannot be verified, the track fails the security contract.

---

# 5. Required product behavior

These requirements determine whether a security-valid runtime is actually useful for coop.

## P1 — OCI-native image

The reusable environment is a standard OCI image built from a Dockerfile/Containerfile.

The image MUST be usable by normal OCI tooling and identified by digest.

## P2 — systemd guest

The sandbox MUST boot a normal distro userspace with systemd sufficient to manage:

- sshd;
- Docker daemon;
- supporting guest services.

`systemctl is-system-running` may report a degraded state if only known irrelevant units fail, but the experiment must record the exact state and failed units.

## P3 — Docker inside guest

Inside the sandbox:

```bash
docker info
docker run --rm hello-world
```

or an equivalent deterministic local test MUST succeed.

A stronger nested workload test SHOULD run a multi-process image or Docker build.

## P4 — persistent filesystem across stop/start

Write a random marker to:

```text
/root or /var/lib/coop-experiment
```

and to Docker state.

Stop the sandbox using the runtime's normal stop path.

Start the same sandbox again.

Both marker and expected Docker state MUST remain.

## P5 — stable machine identity across normal restart

The guest's generated:

- machine-id;
- SSH host key

MUST remain unchanged across normal stop/start.

A recreate from the same OCI image MUST produce new values.

## P6 — CPU and memory configuration

The runtime MUST support setting CPU and memory at creation.

The guest MUST observe the expected values within documented runtime semantics.

## P7 — CPU and memory mutation

Desired end state: change CPU/memory for a stopped sandbox without rebuilding the environment.

Test whether the track can:

1. stop;
2. alter values;
3. restart;
4. read back the new values;
5. preserve guest disk identity.

If not possible, record as a lifecycle gap.

## P8 — explicit disk size

Desired end state: create a sandbox with a requested root/writable capacity, e.g.:

```text
8 GiB
32 GiB
64 GiB
```

The guest must observe a filesystem capacity consistent with the requested value.

Stock Track A must use only public stock surfaces. Hidden/private Apple APIs do not count.

## P9 — disk growth

Desired end state: grow a stopped 8-GiB sandbox to 32 GiB without replacing its logical instance or losing data.

Read back and verify the resulting guest filesystem capacity.

Shrink is not required.

## P10 — local checkpoint/restore

Desired end state:

1. write state A;
2. create checkpoint;
3. mutate to state B;
4. restore checkpoint;
5. observe state A.

A raw filesystem export MAY be tested as an approximation, but it does not count as a successful checkpoint implementation unless it can restore the same logical instance with acceptable performance and identity semantics.

## P11 — logs

The runtime must provide useful:

- workload/system logs;
- VM boot logs or equivalent diagnostics.

Logs must be streamable or incrementally readable.

## P12 — crash recovery

The runtime must survive or cleanly reconcile:

- client process killed during create;
- client killed during boot;
- client killed during stop;
- client killed during delete;
- runtime service restart;
- VM process crash.

A retry must result in a known state.

## P13 — concurrent sandboxes

Run at least:

```text
1
4
8
```

sandboxes concurrently.

Validate:

- isolation still holds;
- unique networks/addresses;
- Docker works;
- no cross-resource deletion;
- startup remains bounded.

## P14 — workspace copy workflow

Without host mounts:

1. copy a git repo into `/workspace`;
2. mutate files in guest;
3. copy results back;
4. verify checksums;
5. ensure no runtime feature implicitly shares the host source directory.

This does not need the full coop `push`/`pull` implementation; it proves the runtime primitive does not block it.

---

# 6. Experiment repository layout

Do not initially modify coop lifecycle code.

Create an isolated experimental workspace, for example:

```text
experiments/apple-sandbox/
├── README.md
├── results/
│   ├── environment.json
│   ├── track-a/
│   └── track-b/
├── image/
│   ├── Dockerfile
│   ├── machine-setup.sh
│   ├── sshd.conf
│   └── verify.sh
├── scripts/
│   ├── common.sh
│   ├── peer-isolation.sh
│   ├── host-exposure.sh
│   ├── persistence.sh
│   ├── docker.sh
│   ├── ssh-identity.sh
│   ├── crash-injection.sh
│   └── benchmark.sh
├── stock/
│   └── run-experiment.sh
└── direct/
    ├── Package.swift
    ├── Sources/
    │   └── CoopSandboxExperiment/
    └── Tests/
```

The harness should produce machine-readable JSON results plus raw logs.

---

# 7. Common OCI test image

Both tracks MUST consume functionally equivalent OCI image contents.

## 7.1 Base

Use:

```text
ubuntu:24.04
linux/arm64
```

Pin the resolved base digest in `results/environment.json`.

## 7.2 Required packages

At minimum:

```text
systemd
systemd-sysv
dbus
openssh-server
sudo
curl
ca-certificates
iproute2
iputils-ping
netcat-openbsd
socat
procps
jq
docker-ce
docker-ce-cli
containerd.io
docker-buildx-plugin
docker-compose-plugin
```

## 7.3 Guest setup

The image should:

- set `ENV container=container`;
- set systemd default target to `multi-user.target`;
- enable sshd and Docker;
- disable password SSH;
- disable root password login;
- disable SSH agent forwarding;
- generate no persistent host keys in the image;
- clear `/etc/machine-id`;
- generate host keys on first boot;
- include a fixed experiment user or use root only for the initial experiment.

Do not bake a host SSH public key into the image.

## 7.4 Entry point

For Track A, use:

```text
/sbin/init
```

as the ordinary container's entrypoint.

Do **not** use Apple's `--init` flag as a replacement for systemd; the experiment is testing a persistent Linux machine userspace.

## 7.5 Container restriction overrides

Stock ordinary containers apply OCI-style masked/read-only paths by default.

The experiment MUST explicitly test whether Docker/systemd requires clearing them.

Initial Track A configuration should use:

```text
--cap-add ALL
--masked-path NONE
--read-only-path NONE
```

If the environment works with a narrower set, record the minimum required configuration as a secondary hardening result.

---

# 8. Track A — stock Apple containers

## A0 — environment capture

Record:

```bash
sw_vers
uname -a
container --version
container system status
```

Record hardware model/chip and host RAM.

Record existing container/network resources before the experiment.

## A1 — build test image

Build with stock:

```bash
container build --platform linux/arm64 -t local/coop-exp:<id> image/
```

Capture:

- image digest;
- build log;
- elapsed time;
- resulting disk usage.

Verify the image contains no canary host secret.

## A2 — create dedicated network

Create one network for sandbox A and a different network for sandbox B.

Example:

```bash
container network create coop-exp-a-<random>
container network create coop-exp-b-<random>
```

Capture network inspection/list output.

## A3 — create stopped sandbox

Create without starting:

```bash
container create \
  --name coop-exp-a-<random> \
  --label coop.experiment=<run-id> \
  --label coop.owner=<random-owner-id> \
  --network coop-exp-a-<random> \
  --cpus 4 \
  --memory 8g \
  --cap-add ALL \
  --masked-path NONE \
  --read-only-path NONE \
  --entrypoint /sbin/init \
  local/coop-exp:<id>
```

Absolutely do not pass:

```text
--ssh
--mount
--volume
--publish
--publish-socket
--rm
```

Immediately inspect the stopped object.

### A3 acceptance

`container inspect` must show:

- expected ID;
- expected image;
- zero external mounts;
- zero published ports;
- zero published sockets;
- `ssh == false`;
- exactly one configured dedicated network;
- expected CPU/memory;
- experiment ownership labels.

If any unsafe property is implicit and cannot be disabled, Track A hard-fails.

## A4 — boot systemd

Start the sandbox.

Probe first through:

```bash
container exec <id> ...
```

Verify:

```text
PID 1 identity
systemd state
sshd active
docker active
```

Capture failed systemd units.

### A4 acceptance

- `/sbin/init` or expected systemd process behaves as the sandbox's workload init;
- services stay alive after `container exec` exits;
- Docker service reaches active state;
- sshd reaches active state.

If systemd cannot function robustly under the normal-container execution model, Track A fails product requirement P2.

## A5 — Docker-in-guest

Run:

```bash
docker info
docker version
docker run --rm <small pinned test image> /bin/true
docker build <small deterministic build context>
```

Capture daemon logs if failure occurs.

### A5 acceptance

All commands succeed without host Docker.

## A6 — effective security inspection

While running, capture full `container inspect`.

Verify:

- one dedicated network only;
- no default network;
- zero host mounts;
- `ssh == false`;
- zero published ports/sockets;
- expected labels;
- expected resources.

Also inspect guest environment for:

```text
SSH_AUTH_SOCK
```

and scan common socket locations.

### A6 acceptance

All S-invariants observable through inspect pass.

## A7 — peer isolation

Create sandbox B on its own network.

Run listeners in B for TCP and UDP.

From A, attempt:

- direct B IPv4;
- direct B IPv6;
- ICMP;
- TCP;
- UDP;
- route manipulation;
- neighbor manipulation;
- source spoofing where feasible;
- broadcast/multicast probes.

Run a positive control from the host or from a container on B's network to prove the listener is actually reachable from an allowed peer.

Repeat after stop/start of both sandboxes.

### A7 acceptance

No cross-network guest-to-guest connectivity.

If cross-network reachability is possible from a root guest, Track A hard-fails.

## A8 — host exposure

From guest root:

- inspect mount table;
- inspect `/proc/self/mountinfo`;
- search for host-home paths;
- search for SSH-agent socket paths;
- test known host integration names/endpoints;
- enumerate reachable host gateway services;
- attempt to reach the runtime's host control sockets indirectly.

Use a canary file in the host home that must never appear in guest-visible mounts.

### A8 acceptance

No filesystem/SSH-agent exposure.

Network-reachable host services must be enumerated. Any unintended sensitive host service is a hard failure until a runtime/network policy can block it.

## A9 — SSH enrollment and pinning

Using only native stock control:

1. create a per-instance Ed25519 client key;
2. use `container exec` as root to install the public key;
3. use `container exec` to read `/etc/ssh/ssh_host_ed25519_key.pub`;
4. record it in a temporary `known_hosts`;
5. SSH directly to the container IP with strict checking;
6. stop/start the container;
7. repeat strict SSH and verify the host key is unchanged;
8. deliberately regenerate host keys inside the guest;
9. verify the next SSH attempt fails.

### A9 acceptance

All steps work without `ssh-keyscan` and without disabling host-key checking.

## A10 — persistence

Write:

```text
/var/lib/coop-experiment/marker
```

Create Docker state:

- pull/create a pinned image;
- create a named Docker volume with a marker.

Stop then start.

Verify all persisted data.

Repeat 20 stop/start cycles.

### A10 acceptance

No state loss and no host-key/machine-id churn.

## A11 — CPU/memory lifecycle

At create time, verify guest-visible CPU/RAM.

Then determine whether **stock public interfaces** permit mutation while preserving the container filesystem.

Test only documented/public mechanisms.

Do not patch runtime state files manually.

### A11 result classification

- `PASS`: can mutate/read back while preserving instance disk.
- `GAP`: cannot mutate but creation-time configuration works.
- `FAIL`: resources are unreliable or incorrectly applied.

A `GAP` does not fail Track A security, but may force Track B for the desired end state.

## A12 — disk lifecycle

Measure:

- guest filesystem capacity;
- host sparse-disk allocation;
- behavior when writing several GiB;
- capacity limit;
- behavior when full.

Determine whether stock public interfaces support:

- explicit initial disk capacity;
- disk growth while preserving instance state.

Do not edit Apple runtime files directly.

### A12 classification

- `PASS`: explicit size + growth supported.
- `PARTIAL`: sufficient automatic growth semantics with a well-defined upper bound acceptable to coop.
- `GAP`: no way to meet requested disk sizing/growth.
- `FAIL`: persistence/corruption issue.

Because configurable disk capacity is part of the intended end state, a `GAP` triggers Track B.

## A13 — checkpoint/restore

Test documented stock primitives:

- `container export`;
- image/container copy semantics if available;
- any released snapshot API exposed publicly.

Evaluate whether state A can be restored efficiently to the same logical sandbox semantics.

### A13 classification

`container export` alone should normally be recorded as `GAP`, not `PASS`, unless the experiment demonstrates acceptable restoration behavior and preserves the desired identity/lifecycle semantics.

A checkpoint gap triggers Track B but does not invalidate Track A's security result.

## A14 — service/runtime recovery

Test:

1. stop/start Apple container system service while sandbox stopped;
2. restart service while sandbox running;
3. kill the experiment client during create;
4. kill client during start;
5. kill client during stop;
6. kill client during delete;
7. kill backing VM/runtime process;
8. inspect/recover/delete afterward.

Record exact states and orphan behavior.

### A14 acceptance

The harness can deterministically discover and reconcile owned resources.

A result that can leave an uninspectable running VM after client/runtime failure is a hard concern.

## A15 — concurrency and performance

Run 1/4/8 sandboxes.

Collect:

- create time;
- start-to-systemd-ready;
- start-to-SSH-ready;
- start-to-Docker-ready;
- host RSS;
- host allocated disk;
- stop latency;
- delete latency.

The experiment is not passed/failed on absolute performance unless it is operationally unusable.

---

# 9. Track A decision gate

After Track A, classify the result.

## A-SUFFICIENT

Choose stock containers if:

- all S1-S10 pass;
- P1-P6 pass;
- P12-P14 pass;
- disk behavior meets the desired end state;
- checkpoint behavior is acceptable;
- no private Apple APIs or patched runtime are required.

In this case, do not run Track B except optionally as research.

## A-SECURE-BUT-INCOMPLETE

Proceed to Track B if:

- all hard security invariants pass;
- systemd + Docker + persistence work;
- but one or more of these remain missing:
  - resource mutation;
  - explicit disk size;
  - disk growth;
  - checkpoint/restore;
  - robust runtime reconciliation.

This is the expected fallback case.

## A-UNSUITABLE

Proceed to Track B immediately if stock containers fail:

- systemd/Docker machine semantics;
- dedicated-network isolation;
- host exposure controls;
- effective inspectability;
- persistent filesystem semantics;
- pinned identity path.

---

# 10. Track B — direct Containerization

Track B should not begin by recreating Apple Container.

Implement the smallest runtime necessary to retest coop's requirements.

## B0 — package baseline

Use:

```text
apple/containerization 0.45.0
Swift 6.x toolchain required by the package
macOS 27 SDK
```

Pin the package revision/version in `Package.swift`.

Use a private experiment root such as:

```text
~/Library/Application Support/coop-containerization-experiment/<run-id>/
```

Do not use the default Apple Container image store.

## B1 — minimal runtime composition

Start from `ContainerManager` / `LinuxContainer` rather than from raw `VZVirtualMachine`.

Required primitives:

- `ImageStore` for OCI;
- `ContainerManager`;
- `LinuxContainer`;
- `VmnetNetwork` or a purpose-built network wrapper;
- explicit rootfs sizing;
- optional writable overlay sizing;
- native guest process execution.

Use a dedicated state root per experiment run.

## B2 — OCI image import/pull

Consume the same logical image used by Track A.

Record:

- resolved digest;
- unpack time;
- disk layout;
- rootfs capacity.

No host secret enters the image store.

## B3 — explicit disk construction

Create sandboxes with:

```text
8 GiB
32 GiB
64 GiB
```

using explicit `rootfsSizeInBytes` and/or `writableLayerSizeInBytes`.

Verify guest-visible capacity.

Test both models if useful:

### Model 1 — mutable rootfs

```text
OCI image unpacked into writable ext4 rootfs
```

### Model 2 — immutable lower + writable overlay

```text
read-only image rootfs
        +
separate writable ext4 upper layer
```

Measure:

- startup;
- disk usage;
- persistence;
- feasibility of cheap checkpoints/clones.

The experiment should identify which model better matches coop.

## B4 — systemd and Docker

Configure the workload so the sandbox provides equivalent machine semantics to Track A.

Do not assume Containerization's default vminitd workload model is sufficient.

Verify:

- systemd;
- sshd;
- Docker;
- nested Docker workload;
- persistence.

If a full systemd machine requires `runc` or additional configuration, record the exact architecture.

## B5 — dedicated networking

The default `VmnetNetwork` alone is not automatically equivalent to "one isolated network per coop VM."

Construct the network topology so each sandbox receives an isolated peer domain.

Possible implementations to test:

1. one `VmnetNetwork` instance per sandbox;
2. explicit vmnet network references/subnets per sandbox;
3. a small network manager modeled after Apple's network service.

Do not accept a design where all coop guests are peers on one segment merely because IPs are unique.

Run the full S3 peer-isolation suite.

## B6 — host reachability

Enumerate what a root guest can reach on the host through the chosen vmnet mode.

If direct Containerization gives more control than stock Apple Container, test whether coop can reduce host reachability to the minimum required.

This is a comparison criterion, not just pass/fail.

## B7 — no mounts / no agent

Create the VM with no host filesystem mounts except runtime-owned init/rootfs block devices.

Do not implement SSH-agent forwarding.

Inspect the guest mount table and host configuration.

Pass the same S4/S5 suite.

## B8 — native enrollment/control path

Use Containerization's guest process control over vsock to:

- install the SSH public key;
- read the guest SSH host key;
- perform readiness checks.

Then switch normal interactive/file operations to pinned SSH.

This validates the preferred end-state split between:

```text
native control for enrollment/security
SSH for ordinary coop operations
```

## B9 — persistence model

Direct `LinuxContainer` objects are runtime objects, not automatically a complete persistent-machine database.

Implement only enough metadata to prove restart across host process exit:

```text
SandboxRecord {
    id
    OCI digest
    rootfs/writable paths
    CPU
    memory
    network allocation
    SSH host key
}
```

Exit the Swift program entirely.

Relaunch it and reconstruct/start the same sandbox from persisted disks.

### B9 acceptance

Guest filesystem, machine-id, and SSH host key persist.

No Apple `container machine` code is needed.

## B10 — CPU/memory mutation

Stop sandbox.

Reconstruct with new CPU/memory settings using the same persistent guest disk.

Verify state and resources.

This is allowed because the product abstraction is "persistent sandbox", not "same VZVirtualMachine object."

### B10 acceptance

Resource mutation succeeds without rebuilding/reprovisioning the rootfs.

## B11 — disk growth

Starting from 8 GiB:

1. stop;
2. grow backing ext4/block image safely;
3. restart;
4. grow filesystem if required;
5. verify 32 GiB capacity;
6. verify old data and SSH identity.

If Containerization provides a direct helper, use it.

If a small filesystem utility is required, count that code/maintenance cost explicitly.

## B12 — checkpoint/restore

Prototype the simplest robust local checkpoint mechanism.

Candidates:

- copy-on-write/clone of persistent writable disk;
- macOS 27 layered disk APIs if usable;
- filesystem snapshot/copy while stopped.

The checkpoint does not need to become OCI.

Evaluate:

- correctness;
- creation latency;
- restore latency;
- allocated bytes;
- crash consistency.

## B13 — process/service crash recovery

Kill the Swift runtime process while VM is:

- booting;
- running;
- stopping.

Determine whether the VM dies with the owning process or persists.

Design the eventual service model accordingly.

For a persistent coop runtime, test a LaunchAgent-style long-lived owner if necessary.

## B14 — full security suite

Repeat S1-S10 without weakening any test because this is a lower-level API.

Direct control is only valuable if the resulting security contract is easier to prove.

## B15 — concurrency/performance

Repeat the Track A 1/4/8 sandbox benchmarks.

Also record:

- OCI unpack cost;
- writable-layer creation cost;
- memory of the Swift runtime;
- amount of host state per sandbox.

---

# 11. Track B acceptance gate

Direct Containerization is considered a viable replacement if:

- all S1-S10 pass;
- P1-P14 can be implemented;
- the implementation does not need to fork broad parts of `apple/container`;
- the runtime-specific code remains a narrow sandbox service rather than a general-purpose container platform;
- lifecycle persistence and recovery can be made deterministic;
- network isolation is demonstrable on real hardware.

---

# 12. Comparative engineering-cost measurements

The experiment must record engineering cost, not just runtime behavior.

For each track record:

## Runtime integration size

- source LOC written;
- number of Apple APIs/types depended on;
- number of subprocess commands parsed;
- number of persisted state types;
- number of lifecycle states handled.

## Apple-upstream coupling

Classify dependencies:

```text
public documented CLI
public Swift package API
package-internal behavior
forked upstream source
private framework/API
```

Lower is better.

## Security state space

Count configurable dimensions relevant to isolation:

- networks;
- mounts;
- SSH;
- published ports;
- sockets;
- init mode;
- capabilities.

Prefer an API where unsafe states are impossible to express.

## Recovery ownership

Document which layer owns:

- resource state;
- orphan cleanup;
- operation idempotency;
- VM process lifetime;
- network lifetime;
- image-store lifetime.

## Upgrade burden

Perform one controlled dependency bump, if a newer patch exists by experiment time.

Measure:

- compile failures;
- protocol/schema changes;
- behavioral regression tests.

---

# 13. Decision framework

The final decision should use this order.

## Rule 1 — security dominates

If only one track passes all S-invariants, choose it.

## Rule 2 — prefer stock when complete

If stock containers pass security and all required lifecycle behavior through documented public surfaces, choose stock.

Do not build a private runtime merely for architectural elegance.

## Rule 3 — prefer direct Containerization over a machine fork when stock is incomplete

If stock containers are secure but cannot satisfy disk/resource/checkpoint requirements, and direct Containerization closes those gaps with a reasonably small runtime, choose direct Containerization.

## Rule 4 — do not silently create a hybrid based on private Apple APIs

A solution such as:

```text
stock container CLI
+
undocumented edits to Apple's runtime state
+
internal XPC calls
```

is a failed Track A result.

At that point use Track B explicitly.

## Rule 5 — machine fork is the comparison fallback, not the default

Only return to a `container machine` fork if both are true:

1. stock containers are insufficient; and
2. direct Containerization requires recreating so much persistence/network/runtime machinery that maintaining the smaller machine fork is demonstrably cheaper.

The experiment report must identify the exact missing primitive that makes the fork preferable.

---

# 14. Expected outcome table

Fill this with evidence.

| Requirement | Stock container | Direct Containerization | Evidence |
|---|---|---|---|
| Standard OCI | TBD | TBD | |
| One VM per sandbox | TBD | TBD | |
| systemd | TBD | TBD | |
| Docker inside guest | TBD | TBD | |
| Dedicated network | TBD | TBD | |
| Root-guest peer isolation | TBD | TBD | |
| No host mounts | TBD | TBD | |
| No SSH agent | TBD | TBD | |
| No published ports/sockets | TBD | TBD | |
| Effective inspection | TBD | TBD | |
| Native bootstrap channel | TBD | TBD | |
| Pinned SSH identity | TBD | TBD | |
| Stop/start persistence | TBD | TBD | |
| CPU/memory create | TBD | TBD | |
| CPU/memory resize | TBD | TBD | |
| Initial disk size | TBD | TBD | |
| Disk growth | TBD | TBD | |
| Checkpoint/restore | TBD | TBD | |
| Logs | TBD | TBD | |
| Crash reconciliation | TBD | TBD | |
| Concurrent instances | TBD | TBD | |
| Runtime implementation LOC | TBD | TBD | |
| Upgrade coupling | TBD | TBD | |

Allowed cell values:

```text
PASS
PARTIAL
GAP
FAIL
NOT TESTED
```

Every `PASS` must point to raw evidence.

---

# 15. Evidence collection

Each experiment action should generate:

```text
results/<track>/<test-id>/
    command.txt
    stdout.log
    stderr.log
    before.json
    after.json
    guest.txt
    timing.json
    result.json
```

`result.json`:

```json
{
  "test": "A7-peer-isolation-ipv4-tcp",
  "status": "PASS",
  "runtime": "container 1.4.1",
  "host": "macOS 27.x arm64",
  "started_at": "...",
  "duration_ms": 1234,
  "notes": ""
}
```

Do not place secrets in evidence files.

---

# 16. Final experiment report

Produce:

```text
experiments/apple-sandbox/results/REPORT.md
```

Required sections:

1. Host/runtime versions.
2. Exact OCI image digest.
3. Track A result.
4. Track A hard failures.
5. Track A lifecycle gaps.
6. Whether Track B was triggered and why.
7. Track B result.
8. Security comparison.
9. Lifecycle comparison.
10. Performance comparison.
11. Engineering-maintenance comparison.
12. Recommended runtime architecture.
13. Required deviations from the proposed coop end-state spec.
14. Open risks.

The conclusion must be one of:

```text
USE_STOCK_CONTAINER
USE_DIRECT_CONTAINERIZATION
KEEP_MACHINE_FORK
NO_ACCEPTABLE_RUNTIME_YET
```

---

# 17. Stop conditions

Stop Track A early and move to Track B when a result is clearly fundamental and repeatable:

- systemd cannot provide the required persistent service environment;
- Docker-in-guest cannot run under any acceptable stock configuration;
- dedicated networks do not isolate root-controlled guests;
- host mounts/SSH agent are unavoidable;
- effective security state cannot be inspected.

Do **not** stop Track A early merely because:

- disk resize is absent;
- checkpoint is absent;
- CPU/memory mutation is absent.

Those are specifically the lifecycle gaps Track B is intended to evaluate.

Stop Track B and retain the machine fork if direct Containerization requires a broad reimplementation of:

- OCI service infrastructure;
- networking service infrastructure;
- persistent runtime supervision;
- lifecycle recovery;

such that the resulting code is materially larger and harder to secure than the existing narrowly modified machine implementation.

---

# 18. High-value first-day spike

Before implementing the full harness, run this minimal sequence.

## Stock

1. Build Ubuntu systemd + Docker OCI image.
2. Create two dedicated networks.
3. Create one stock container per network with:
   - `--cap-add ALL`;
   - `--masked-path NONE`;
   - `--read-only-path NONE`;
   - `/sbin/init`.
4. Start both.
5. Verify systemd.
6. Verify Docker.
7. Verify stop/start persistence.
8. Verify inspect says:
   - no mounts;
   - ssh false;
   - no published ports/sockets;
   - one dedicated network.
9. Verify A cannot ping/connect to B.
10. Use `container exec` to enroll SSH and prove pinned SSH works.

If all ten succeed, the stock-container hypothesis is strong enough to justify the full Track A suite.

If systemd/Docker fails for fundamental runtime reasons, move quickly to Track B.

---

# 19. High-value Direct Containerization spike

If Track B is triggered, the first Swift spike should prove only:

1. pull/open the same OCI image;
2. create an explicit 16-GiB rootfs using `ContainerManager`;
3. create an isolated vmnet interface;
4. boot a `LinuxContainer`;
5. run systemd or the proposed guest machine init;
6. start Docker;
7. exit the host client;
8. reconstruct the sandbox from the same persisted block filesystem;
9. verify its data persists.

Only after this works should the experiment add checkpoints, resize, and a long-lived service.

---

# 20. Why this experiment is structured this way

Stock ordinary Apple containers have an important property that `container machine` currently lacks for coop: their dangerous host integrations are **opt-in**.

A stock container configuration can express:

```text
mounts = []
publishedPorts = []
publishedSockets = []
ssh = false
networks = [dedicated]
```

and those fields are inspectable.

This makes stock containers worth proving before maintaining a runtime fork.

At the same time, the stock CLI does not currently expose every persistent-VM lifecycle primitive coop wants. Direct Containerization is therefore evaluated not as a totally different product, but as the next lower layer when the stock product surface becomes the constraint.

The experiment deliberately keeps the same OCI image and security tests across both tracks so the decision is based on runtime behavior and maintenance burden rather than two unrelated prototypes.
