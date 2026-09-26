# Platform Backends

coop selects its VM backend at compile time. macOS builds use Lima. Linux builds use Firecracker. A macOS build with the opt-in `apple-container` feature uses coop-sandbox VMs on Apple's `containerization` package instead of Lima (see [macOS / Apple sandbox](#macos--apple-sandbox-opt-in)). The binary determines the backend; there is no runtime override.

All backends expose the same CLI commands and produce the same guest environment: Ubuntu with Docker, GitHub CLI, Claude Code, and Codex pre-installed. The backends differ in how they create and manage the VM underneath.

## macOS / Lima

The Lima backend runs VMs through [Lima](https://lima-vm.io/), which wraps Apple's Virtualization.framework. Lima manages disk images, networking, and SSH port forwarding, so coop delegates most operations to `limactl`.

### Prerequisites

Install Lima before running `coop setup`:

```
brew install lima
```

Setup verifies that `limactl --version` is reachable. If it is not, setup fails with an install hint.

### Setup process

`coop setup` builds a golden disk image that all instances clone from:

1. Generates an ed25519 SSH key pair, stored in the coop data directory.
2. Creates a temporary builder VM from an Ubuntu 24.04 cloud image. The Lima YAML template includes a cloud-init provision script.
3. The provision script installs all packages (Docker, GitHub CLI, Claude Code, Codex, and any profile packages), creates the `ubuntu` user with SSH access, and enables services.
4. After provisioning completes, cleans cloud-init state so it re-runs on cloned instances.
5. Stops the builder VM and extracts its disk as the golden image.
6. Generates a fast-start Lima template that references the golden image directly. No cloud-init provisioning runs on instance start.

The builder VM is deleted after extraction, whether the build succeeds or fails.

### How instances work

Each instance is a Lima VM created with `limactl start` using the fast-start template. Lima names are prefixed with `coop-` (e.g., `coop-my-instance`). The instance gets its own disk, a copy-on-write layer over the golden image. Lima handles SSH port allocation automatically; coop reads the assigned port from `limactl list --json`.

The Lima template configures:
- `vmType: "vz"` (Virtualization.framework, not QEMU)
- Rosetta enabled for x86_64 binary translation on Apple Silicon
- `mountType: "virtiofs"` with no host mounts (empty `mounts: []`). When `coop up --mount` is used, Lima adds virtiofs mount entries for the specified host directories, providing live mounts where changes are visible immediately on both sides.
- Lima's built-in containerd disabled (Docker is installed in the guest instead)

### Resize (disk, memory, vCPUs)

Resizing a stopped instance's disk records the new size as `disk:` in its `lima.yaml` (Lima 2.x refuses to boot when the disk is larger than that value) and truncates the Lima disk to it. Cloud-init's `growpart` module expands the partition and filesystem on next boot. Shrinking is not supported. Re-running `coop resize --size` at the current size repairs an instance whose `lima.yaml` lags its disk.

Memory and vCPU changes rewrite the `cpus`/`memory` fields in the instance's `lima.yaml`, which Lima re-reads on `limactl start`. The edit is written atomically, then coop starts the instance to validate and apply the new spec — if `limactl` rejects it (e.g. a spec larger than the host), the previous `lima.yaml` is restored. Without `--start` the instance is stopped again after the validating boot. The `lima.yaml` is authoritative: the global `[vm]` `cpus`/`memory` settings only seed *new* instances.

### Resource ownership

Lima runs as the current user. No `sudo` is required for any Lima operation: setup, start, stop, or destroy.

## macOS / Apple sandbox (opt-in)

Build with `cargo build --release --features apple-container` to replace Lima with **coop-sandbox**, coop's own runtime on Apple's [`containerization`](https://github.com/apple/containerization) package ([`macos/coop-sandbox`](../macos/coop-sandbox)). Selecting the feature on a non-macOS target is a compile error. The default macOS build never calls, requires, or modifies it.

Each instance is one Linux VM running systemd from its own ext4 disk, on its own vmnet network, with no host mounts, socket relays, published ports, or host SSH-agent forwarding. The runtime's sandbox record has no field for any of those, and coop verifies the running VM's effective configuration before every hand-out. [`design/apple-sandbox-runtime.md`](design/apple-sandbox-runtime.md) records why this replaced the earlier `container machine` fork.

### Prerequisites

- Apple Silicon, macOS 26 or later (vmnet's per-network API). Validated on macOS 27.0.
- `coop-sandbox`, built with `scripts/build-coop-sandbox.sh` (Xcode with Swift 6.2+ required).
- Stock Apple `container` 1.4.1 or later, with its service running (`container system start`). coop uses it only to **build** images (`container build`) and to supply the guest kernel it installs; instances never run on it. coop never starts, stops, or restarts that service.

Binaries come from `[apple_container]` `binary` (coop-sandbox) and `builder` (`container`), or else fixed install locations: `~/.local/opt/coop-sandbox/bin/coop-sandbox`, `/usr/local/bin/coop-sandbox`, `/opt/homebrew/bin/coop-sandbox`, and `/usr/local/bin/container`, `/opt/homebrew/bin/container`. `PATH` and project files are never consulted, and a binary that is group/world-writable or owned by neither you nor root is rejected, as is one under a directory that is owned by neither you nor root, world-writable without the sticky bit, or group-writable without the sticky bit unless its group is `wheel` or `admin` (Homebrew's prefix is `admin`-writable).

Neither Lima nor host Docker is needed. Docker runs *inside* the guest.

### Installing the runtime

```bash
scripts/build-coop-sandbox.sh            # installs ~/.local/opt/coop-sandbox/bin/coop-sandbox
```

It builds the Swift package in release mode, signs it ad hoc with the hardened runtime and its one entitlement (`com.apple.security.virtualization`), and installs it without `sudo`. It refuses an existing `bin/` that is owned by neither you nor root, world-writable, or group-writable by a group other than `wheel` or `admin`. Pass a different prefix as the first argument and set `[apple_container] binary` to match. Rebuild after pulling changes to `macos/coop-sandbox`; coop refuses a runtime whose protocol or `containerization` version differs from the one it was built for.

### Supported combinations

| coop-sandbox | containerization | macOS | Hardware | Evidence |
|---|---|---|---|---|
| 0.2.0 (protocol 2) | 0.45.0 | 27.0 | Apple Silicon | [`tests/integration-apple-sandbox.sh`](../tests/integration-apple-sandbox.sh) (all phases, including maintenance install, same-sandbox races, and the `coop` end-to-end phase): 103 passed, 1 skipped by design ([run record](design/apple-sandbox-transactions.md#4-validation)) |
| 0.1.0 (protocol 1), refused since protocol 2 | 0.45.0 | 27.0 | Apple Silicon | [`tests/integration-apple-sandbox.sh`](../tests/integration-apple-sandbox.sh) (isolation, host exposure, canary, pinning, persistence, resources, growth, commit/restore, crash recovery, concurrency), coop `setup`/`up`/`exec`/`stop`/`resize`/`commit`/`restore`/`destroy` end to end |

The runtime also pins its guest kernel by sha256 (`vmlinux-6.18.15-186`, the kernel `container` 1.4.1 installs) and its init image (`vminit:0.45.0` by digest). `coop setup` fails with `APPLE_RUNTIME_UNAVAILABLE` on any other kernel.

### Configuration

```toml
[apple_container]
# binary = "/absolute/path/to/coop-sandbox"
# builder = "/absolute/path/to/container"
# kernel = "/absolute/path/to/vmlinux"   # must be a kernel the runtime pins
probe_timeout_seconds = 10     # version, inspect, list
operation_timeout_seconds = 60 # resource changes, deletes, guest commands
create_timeout_seconds = 600   # create (first unpack of an image), grow, commit, restore, init, maintenance install
boot_timeout_seconds = 120     # boot to SSH-ready
stop_timeout_seconds = 90      # clean systemd shutdown
build_timeout_seconds = 3600   # image build; `setup --builder-timeout` overrides
```

Each timeout must be between 1 and 86400 seconds. Unknown keys are rejected, and there is no key to mount the home directory, forward the SSH agent, share a network, or skip qualification. The existing `[vm]` CPU/memory, image, guest-user, profile, and workspace settings apply unchanged; `[vm] template_size_gib` is the default disk size.

### State

The feature build defaults to `~/.coop-apple` for its config file and data directory, so neither build's `uninstall --purge` can reach the other's state. `coop setup` refuses a `data_dir` that already holds a default build's `images/`, `instances/`, `vm_key`, or Firecracker/Lima artifacts, because that build's purge removes its whole `data_dir`. Whatever `data_dir` is configured, this backend keeps everything under `<data_dir>/backends/apple-container-v1/`:

- `owner.json` (installation owner ID) and `vm_key`
- `images/<name>/`: `template-config.json`, `apple-image.json`, and `build.log`
- `instances/<name>/`: `apple-machine.json`, `known_hosts`, `operation.json` while a mutation is pending, and the shared sidecars
- `runtime/`, the coop-sandbox state root: kernel, init filesystem, private OCI store, cached base disks, committed disks, the maintenance boot disk (`maintenance/`), lock files (`locks/`), and one directory per sandbox (disk, record, console and owner logs, launchd plist)

Control files are `0600`, directories `0700`. `uninstall --purge` destroys every instance, then removes all of `~/.coop-apple` when that is the data directory, and otherwise only `backends/apple-container-v1/`. Workspace copies always skip `.coop-apple/`. Editor `~/.ssh/config` entries use `coop-apple-<name>` aliases inside `# coop-apple START/END` markers, so the two builds never touch each other's entries. Because a default-build instance named `apple-<x>` has the same alias as this build's `<x>`, each build refuses to write an alias the other already manages. The data directory path may contain spaces (SSH options are quoted) but not quote or control characters.

Instances created by the retired `container machine` backend (schema 1) are refused, including by `coop destroy`, `destroy --all`, and `uninstall --purge`. Remove such an instance's directory under `backends/apple-container-v1/instances/` by hand, and delete its machine and network in Apple `container` (`container machine delete`, `container network delete`).

`coop update` is disabled in this build (`APPLE_UPDATE_VARIANT_UNSUPPORTED`): release artifacts carry only the Lima backend. Rebuild from source instead.

### Setup process

`coop setup`:

1. Checks the platform, resolves and qualifies coop-sandbox (`coop-sandbox version`: protocol 2, containerization 0.45.0), and creates `owner.json` and the VM-access key pair.
2. Initializes the runtime root: copies the kernel after checking its pinned sha256, and pulls the pinned init image. Unless the runtime already has the current maintenance image, builds it (Ubuntu with e2fsprogs; log in `maintenance-build.log`), installs it with `coop-sandbox maintenance install`, and deletes the store copy.
3. Renders a minimal build context in a private temporary directory: a Dockerfile `FROM ubuntu:24.04` pinned by digest, the same provisioning script Lima uses (packages, profiles, OCI features, guest user, Claude Code, Codex, Docker), and a machine-setup script. The context contains the coop **public** key only. There are no build arguments and no secrets.
4. Checks that the builder's service is running, then runs `container build --platform linux/arm64 -t local/coop-<owner>:<hash>-<nonce>`, with output in `images/<name>/build.log`. Every build gets a fresh tag, so a rebuild never retags an image in use.
5. Saves the image as an OCI archive, imports it into the runtime's private store, and deletes the builder's copy.
6. Boots the image in a disposable sandbox with no credentials, passing the same isolation gate an instance does. It checks the required guest binaries and the guest user's uid (1000), and waits (up to `boot_timeout_seconds`) for `ssh` and `docker`. Then it stops and deletes the sandbox. The unpacked disk stays cached for the first `coop up`.
7. Records the image digest and input hash in `apple-image.json` and `template-config.json`. A failed build or verification deletes the new image and leaves the previous manifest and image in place. After a successful rebuild, the superseded image is deleted.

The image carries no SSH host keys and an empty `/etc/machine-id`. Each sandbox generates its own on first boot and keeps them across restarts. `sshd` refuses passwords and root logins and disables agent forwarding. Units that would fight the runtime's addressing (networkd, resolved, udevd, timesyncd) are masked.

Marketplaces and plugins are not baked into the image. The first boot installs them through the shared bootstrap.

### How instances work

`coop up` creates one sandbox per instance, named `coop-<owner8>-<random16>`. The steps:

1. Write `operation.json`.
2. `coop-sandbox create` with explicit CPUs, memory (MiB), and disk (`--disk`, or the committed image's size, or `[vm] template_size_gib`). The disk is an APFS clone of the image's cached base, so this takes milliseconds after an image's first use.
3. Check the runtime's record: owner tag, CPUs, and memory.
4. `coop-sandbox start` loads the sandbox's owner as a launchd job and returns once it answers. The owner process holds the VM and a dedicated `10.231.N.0/24` vmnet network.
5. The isolation gate reads the effective VM configuration from the owner and checks all of the following:
   - The sandbox runs `/sbin/init`, without nested virtualization.
   - It boots from its own disk under `runtime/sandboxes/<id>/`.
   - Its only mounts are the kernel pseudo-filesystems (`proc`, `sysfs`, `devtmpfs`, `mqueue`, `tmpfs` at `/dev/shm`, `cgroup2`, `devpts`) from their fixed sources.
   - It has no socket relays, published ports, or agent forwarding.
   - It has exactly one interface, on its own vmnet subnet, carrying the address the owner reports.
   - Its CPUs, memory, and image digest match the record.
6. Read `/etc/ssh/ssh_host_ed25519_key.pub` over the runtime's native control channel (vsock exec, an argv rather than a shell string), confirm the owner did not restart meanwhile, and pin the key in the instance's `known_hosts`.
7. Connect over SSH with `StrictHostKeyChecking=yes` against that pin, then hand off to the shared lifecycle: forwards, credentials, agent bootstrap, workspace copy, hooks.

Every later `ssh_target` (shell, exec, agent launch, push/pull, editor) re-inspects the sandbox and re-runs the gate before it returns a target. A sandbox keeps its address across restarts, unless its subnet had to be quarantined (see [Recovery](#stop-destroy-recovery)). Every start compares the host key with the pin. A changed or missing key fails with `APPLE_HOST_KEY_CHANGED`, and coop never re-enrolls on its own. The one exception is `coop restore`: coop replaced the disk itself (which removes the host keys), so the next start pins the key the guest generates.

Workspaces are always copied. `--mount` directories are synced once, as on Firecracker; use `coop push`/`coop pull`.

Local model servers on host loopback reach the guest over a per-instance `ssh -R 127.0.0.1:<guest-port>:<host-addr>:<host-port>` tunnel. The forward goes to the exact loopback address the URL names. The guest port is the same as the host port, except that a privileged port (below 1024) moves to port + 40000, so `https://localhost` becomes `https://localhost:40443` in the guest. `localhost` and `127.0.0.1` URLs keep their host, so TLS names still verify. Other `127.x` addresses are rewritten to `127.0.0.1` for plain HTTP only. IPv6-loopback endpoints are rejected. Every boot first closes the tunnels recorded for the previous boot. Each bootstrap then reconciles the tunnels for both agents: live tunnels are kept, tunnels the config no longer needs are closed, and two endpoints that need the same guest port with different destinations are an error.

### Resize, commit, restore

All three need the instance stopped.

- **`coop resize --mem/--vcpus`** records the new values with `coop-sandbox set`, tagged with an operation id, and reads them back. They apply at the next start. The change is journaled; an interrupted one is reconciled on the next `coop start` from the runtime's record. With `--start`, if the boot fails and the sandbox is confirmed stopped again, the previous values are restored through the same journaled update, and only if the runtime's last committed operation is still the forward change; otherwise, or if the rollback cannot be confirmed, the result is `APPLE_OPERATION_UNCERTAIN`. The guest sees one more vCPU than configured: the runtime's own overhead.
- **`coop resize --size`** grows the disk offline. The runtime clones the disk, extends it, and runs `e2fsck`/`resize2fs` in a short maintenance VM, with the instance's disk attached as data. It then publishes the grown disk and its new size as one recoverable update (a crash between the two is finished by the runtime's next operation on the sandbox), so this takes about a second. Shrinking is refused.
- **Maintenance image.** Maintenance VMs boot their own small image (Ubuntu with e2fsprogs), which `coop setup` builds with the stock builder and installs into the runtime outside its image store, then removes from the store. It does not depend on any instance's image, so deleting or replacing images never affects growth or commits. `coop setup` reinstalls it when its recipe version changes.
- **`coop commit --image <name>`** saves an APFS clone of the disk with its SSH host keys and machine-id removed. `coop up --image <name>` and `coop restore` then clone it, and every instance created from it generates its own identity.
- **`coop restore`** swaps in a clone of a committed disk, or a fresh copy of a base image with `--reprovision`. It then grows the new disk back to the instance's size if that is larger. The operation is journaled: a restore interrupted by a crash is reconciled on the next `coop start` from the runtime's record, and it re-pins the host key only if the runtime's last committed operation is that restore (a higher disk generation alone is not enough).

### Stop, destroy, recovery

- **Stop.** `stop` asks systemd to halt (the runtime forces the VM down after 60 s) and confirms the sandbox reached `stopped`. An unconfirmed stop is `APPLE_OPERATION_UNCERTAIN`, and nothing is deleted. When the normal liveness check fails (an unqualified runtime, a sandbox that fails the gate, or an unfinished journal), `coop stop` stops the owned sandbox through the runtime alone, with no SSH and no qualification, and keeps its disk. `coop status` lists such an instance as `unknown`. A boot that fails or times out during `coop start` stops the sandbox again.
- **Interrupts.** Ctrl-C interrupts only image builds, creates, and boots. Stop, delete, and cleanup commands always run to completion.
- **Destroy.** `destroy` acts only on sandboxes whose names and local records match this installation's owner ID; the runtime also refuses to delete a sandbox whose recorded owner differs. It stops and deletes the sandbox, confirms it is gone, then removes local state. If an operation was interrupted (`operation.json` exists), `destroy` checks what the runtime actually has and removes only what the journal says coop created.
- **Crashed owners.** The VM lives inside its owner process. If that process dies, the VM powers off (no VM is ever orphaned) and launchd starts the owner again, which boots the same disk. Journaled ext4 recovers, but unsynced guest writes can be lost.
- **Subnet leaks.** After an unclean exit, vmnet keeps the sandbox's subnet reserved for hours. The runtime then quarantines it and moves the sandbox to a free subnet, so its address changes while its identity does not.
- **Image deletion.** Only this installation's images and committed disks are deleted. Instances never depend on them after creation.

### Diagnostics

| Identifier | Meaning |
|---|---|
| `APPLE_RUNTIME_UNAVAILABLE` | No usable coop-sandbox or builder, builder service not running, kernel not accepted, or unsupported platform. |
| `APPLE_RUNTIME_UNQUALIFIED` | Unknown runtime, protocol, `containerization` version, or output schema (including an unknown field in the effective configuration). |
| `APPLE_NETWORK_ISOLATION` | Missing, extra, or foreign network interface, or an address that does not match. |
| `APPLE_HOST_EXPOSURE` | A host mount, socket relay, published port, agent forwarding, foreign root disk, or non-systemd init. |
| `APPLE_IDENTITY_CONFLICT` | Ownership, name, image, resource, or boot identity mismatch. |
| `APPLE_HOST_KEY_CHANGED` | Missing or changed pinned host key. |
| `APPLE_BOOT_TIMEOUT` | Boot or readiness failed or exceeded its deadline, or no valid host key appeared in time; the error includes the last lines of the console log. Disk and journal are kept. |
| `APPLE_OPERATION_UNCERTAIN` | Timed-out or cancelled runtime call, unconfirmed stop, a booting or crashed sandbox, or an unfinished journal; reconciled on retry. `coop list` shows a crashed sandbox as stopped, since `coop start` accepts it. |

`coop logs` (snapshot and `--follow`) replaces control characters in the guest's console output before printing it.

Runtime and builder commands run with a cleared environment: only `HOME`, `USER`, `LOGNAME`, `TMPDIR`, locale, and a fixed `PATH` pass through. `SSH_AUTH_SOCK`, API and GitHub tokens, `DYLD_*`, and `CONTAINER_*` overrides are dropped. The launchd job that runs each owner gets a fixed environment of its own.

### Validation status

Validated on macOS 27.0 (Apple M5 Max) with coop-sandbox 0.1.0 and containerization 0.45.0. Runtime 0.2.0 has re-run the runtime suite and the `coop` phase; see the table above.

- **Runtime ([`tests/integration-apple-sandbox.sh`](../tests/integration-apple-sandbox.sh); the selection experiment is in [`design/apple-sandbox-runtime.md`](design/apple-sandbox-runtime.md)):**
  - Peer isolation: a root guest cannot reach another sandbox by TCP, UDP, or ICMP over IPv4 or IPv6. That holds with forged on-link routes, static neighbour entries, spoofed source addresses, and broadcast/multicast, and after restarts; the host reaches each listener as the positive control.
  - Host exposure: no mounts, agent sockets, host canary file, or host vsock listeners reach the guest, and a canary secret never reaches the runtime, its logs, the image, or the guest.
  - Identity and lifecycle: pinned SSH over the native channel; 20 stop/start cycles with no loss of data or identity; CPU/memory changes, disk growth, and commit/restore.
  - Recovery and scale: every crash-injection scenario ends in a known state, and 1/4/8 concurrent sandboxes each get their own address and subnet.
- **coop end to end:** the suite's `coop` phase covers `setup` (build, import, verification), `up`, `exec`, `status`, `stop`/`start`, `resize --size/--mem/--vcpus`, rollback of a failed `resize --start`, `commit`, `restore` with host-key re-pinning, `destroy`, and image deletion. Run by hand on 0.1.0 only: `up` with an explicit disk, `logs`, `up --image` from a committed image with a fresh identity, and rejection of a guest-changed host key.

Not covered: other macOS releases or kernels, and live-provider API calls.

## Linux / Firecracker

The Firecracker backend runs [Firecracker microVMs](https://firecracker-microvm.github.io/) with KVM hardware virtualization. Each instance is a lightweight VM with its own rootfs, TAP network device, and Firecracker process.

### Prerequisites

- **KVM access**: `/dev/kvm` must exist and be readable/writable by the current user. Setup checks this and offers to fix permissions via `setfacl` or by adding the user to the `kvm` group.
- **x86_64 or arm64 architecture**: The Firecracker backend supports both. x86_64 is the primary test target; arm64 builds are produced but less exercised.
- **curl**: Required for downloading the Firecracker binary and kernel.
- **System packages**: Setup checks for `setfacl`, `unsquashfs`, `mkfs.ext4`, `ssh`, and `rsync`. If tools are missing, it offers to install their Debian/Ubuntu packages (`acl`, `squashfs-tools`, `e2fsprogs`, `openssh-client`, and `rsync`) using `apt-get`. If `apt-get` is unavailable, setup lists the missing tools; install the packages providing them with your host's package manager and rerun `coop setup`. No package manager is needed for this check when all these tools are already on `PATH`. The guest remains Ubuntu regardless of the host distribution.

### Setup process

`coop setup` prepares three artifacts:

1. **Firecracker binary**: Downloaded from the latest GitHub release and stored in the data directory. The jailer binary is extracted alongside it.
2. **Guest kernel**: Fetched from Firecracker's CI S3 bucket. This is a minimal `vmlinux` image matching the Firecracker release version.
3. **Template rootfs**: Built by downloading the Firecracker CI squashfs rootfs (Ubuntu-based), unpacking it, creating an ext4 image at the configured template size, and running an install script inside a chroot. The script installs Docker, GitHub CLI, Claude Code, Codex, and profile packages. It configures the `ubuntu` user with SSH keys and sets up systemd-networkd.

All three steps are idempotent. If the artifact already exists and is up to date, setup skips it.

### How instances work

Creating an instance (`coop up`) follows this sequence:

1. Copies the template rootfs to the instance directory using `cp --reflink=auto` for copy-on-write on supported filesystems.
2. Mounts the copy and patches the guest network config with the instance's unique IP address, plus `/etc/hostname` and the matching `/etc/hosts` alias so the guest can resolve its own name.
3. Optionally resizes the rootfs if a larger disk was requested (truncate + e2fsck + resize2fs).
4. Writes a Firecracker JSON config specifying the kernel, rootfs drive, vCPU/memory allocation, network interface, and vsock device.
5. Creates and attaches a TAP device to the bridge (see TAP networking below).
6. Starts the Firecracker process with `sudo`. Firecracker requires root for KVM and TAP access.
7. Records the Firecracker PID and waits for SSH to become reachable.
8. If `--mount` was specified, rsyncs the host directory into the guest. This is a one-time copy, not a live mount. Use `coop push` and `coop pull` to re-sync.

The code uses a typestate pattern (`Configured` then `Running`) to enforce valid lifecycle transitions at compile time.

Stopping a VM sends `SendCtrlAltDel` via the Firecracker API socket for graceful shutdown, falls back to `SIGTERM`, then `SIGKILL` if the process does not exit.

### TAP networking

Each Firecracker instance gets a dedicated TAP device (`tap0`, `tap1`, ...) derived from its instance index.

The network is configured as follows:

- A Linux bridge (`br0`) is created if it does not already exist, with the configured host IP (default `172.16.0.1/24`).
- IP forwarding is enabled via `sysctl`.
- iptables NAT masquerade and forwarding rules route guest traffic through the host's default network interface. The interface is auto-detected from the default route, or set explicitly via `network.host_iface` in the config.
- A `FORWARD -i br0 -o br0 -j DROP` rule is inserted at the head of the chain. If an existing rule has lost that precedence, startup fails until the host firewall configuration places it first.
- Each instance's TAP device is created, attached to the bridge, marked as an isolated bridge port, and brought up.
- Guest IPs are assigned statically: `172.16.0.{index + 2}`. Instance 0 gets `172.16.0.2`.

**Instances cannot reach each other by IP.** Two controls enforce that and both are required: the isolated bridge-port flag blocks the direct L2 path, and the `FORWARD` rule blocks the L3 path a guest could otherwise take by routing through the host's bridge address. Each guest still reaches the host and the internet. The flag is read back after being set, so a host that cannot apply it fails the VM start rather than booting an unisolated guest; this needs Linux ≥ 4.18 and iproute2 ≥ 4.19.

Isolation is applied per start. Because the kernel drops a frame only when both ports are isolated, a VM still running from before the upgrade leaves the *whole bridge* unisolated until it is stopped and started — not just itself.

[`docs/trust-model.md`](trust-model.md) carries the full invariant and its known residuals (ARP/IP impersonation, IPv6, firewall reloads).

On teardown, the TAP device is removed. If no TAP devices remain on the bridge, the bridge and all associated iptables rules are also removed.

### Network configuration

The `network` section in `config.toml` controls Firecracker networking:

| Field | Default | Description |
|---|---|---|
| `host_ip` | `172.16.0.1` | IP address assigned to the bridge on the host side |
| `subnet_mask` | `/24` | CIDR subnet mask for the bridge network |
| `host_iface` | `auto` | Host interface for NAT. `auto` detects the default route interface. Set explicitly if auto-detection fails. |

These settings are ignored on macOS. Lima handles its own networking. coop's generated Lima templates declare no `networks:` stanza, so each macOS guest gets its own user-mode NAT: instances are isolated from each other by construction there, with no shared bridge and nothing to enforce.

### Resize (disk, memory, vCPUs)

Resizing a stopped Firecracker instance's disk runs `truncate` to extend the rootfs image, then `e2fsck -fy` and `resize2fs` to grow the filesystem in place. Shrinking is not supported.

Memory and vCPU changes edit the `machine-config` block of the instance's per-instance JSON (`vm_config.json`), written atomically so a crash mid-write leaves the prior values intact. This JSON is authoritative: on every restart `configure()` regenerates the infra fields (kernel path, boot args, drive, network) from the global config so they roll forward, but preserves the on-disk `mem_size_mib`/`vcpu_count` rather than resetting them to the global `[vm]` defaults. Those defaults therefore only seed *new* instances. Firecracker does not boot the VM to apply the change; it takes effect on the next `coop start` (or immediately with `--start`).

### Resource ownership

Firecracker requires `sudo` for several operations:

- Starting the VM (KVM device access, TAP device creation)
- Stopping the VM (the Firecracker process runs as root)
- Creating and manipulating TAP devices and bridge interfaces
- Managing iptables rules
- Rootfs operations during setup (chroot, mount, filesystem tools)
- Destroying instance directories (files owned by the root-owned Firecracker process)

## Cross-compilation

coop supports cross-compiling from macOS (arm64) to Linux (x86_64) for the Firecracker backend. The project includes a Cargo config that sets the linker for `x86_64-unknown-linux-musl` to `x86_64-linux-musl-gcc`, provided by the `musl-cross` Homebrew package.

The integration test runner (`tests/run-integration.sh --remote`) automates this workflow: it cross-compiles a release build, copies the binary to the remote Linux host via scp, and runs the test suite there.

## Feature parity

Lima and Firecracker support the same CLI commands and guest capabilities (the Apple sandbox backend's differences are listed in its section above):

| Capability | Lima (macOS) | Firecracker (Linux) |
|---|---|---|
| `coop setup` | Builds golden image via builder VM | Installs binary + kernel, builds rootfs via chroot |
| `coop up` | Creates or reconnects/restarts a project VM; `--profile` builds/starts a derived image | Copies rootfs, configures TAP, starts Firecracker; `--profile` builds/starts a derived image |
| `coop start` | Restarts a stopped Lima VM | Restarts a stopped Firecracker VM |
| `coop stop` | `limactl stop` | API socket shutdown, SIGTERM, SIGKILL |
| `coop destroy` | `limactl delete --force` | Kill process, remove TAP, delete instance dir |
| `coop status` | Queries `limactl list --json` | Reads PID file, queries guest via SSH |
| `coop logs` | Reads Lima's `serial.log` | Reads Firecracker log file |
| `coop shell` | SSH to localhost on Lima-assigned port | SSH to guest IP on configured port |
| `coop resize` | Disk: truncates Lima disk. Mem/vCPU: edits `lima.yaml`, validated via start | Disk: truncates + resize2fs on rootfs. Mem/vCPU: edits per-instance JSON |
| Resource monitoring | SSH query to guest | SSH query to guest |
| Docker in guest | Works (full kernel) | Works (with iptables-legacy workaround) |
| `--mount` host mounts | Live virtiofs (changes visible immediately) | One-time rsync sync (use `push`/`pull` to re-sync) |
| Needs sudo | No | Yes (VM start, stop, networking, rootfs ops) |
