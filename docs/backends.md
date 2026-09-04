# Platform Backends

coop selects its VM backend at compile time. macOS builds use Lima. Linux builds use Firecracker. The binary determines the backend; there is no runtime override.

Both backends expose the same CLI commands and produce the same guest environment: Ubuntu with Docker, GitHub CLI, Claude Code, and Codex pre-installed. The backends differ in how they create and manage the VM underneath.

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

Resizing a stopped instance's disk truncates the Lima disk to the new size. Cloud-init's `growpart` module expands the partition and filesystem on next boot. Shrinking is not supported.

Memory and vCPU changes rewrite the `cpus`/`memory` fields in the instance's `lima.yaml`, which Lima re-reads on `limactl start`. The edit is written atomically, then coop starts the instance to validate and apply the new spec — if `limactl` rejects it (e.g. a spec larger than the host), the previous `lima.yaml` is restored. Without `--start` the instance is stopped again after the validating boot. The `lima.yaml` is authoritative: the global `[vm]` `cpus`/`memory` settings only seed *new* instances.

### Resource ownership

Lima runs as the current user. No `sudo` is required for any Lima operation: setup, start, stop, or destroy.

## Linux / Firecracker

The Firecracker backend runs [Firecracker microVMs](https://firecracker-microvm.github.io/) with KVM hardware virtualization. Each instance is a lightweight VM with its own rootfs, TAP network device, and Firecracker process.

### Prerequisites

- **KVM access**: `/dev/kvm` must exist and be readable/writable by the current user. Setup checks this and offers to fix permissions via `setfacl` or by adding the user to the `kvm` group.
- **x86_64 or arm64 architecture**: The Firecracker backend supports both. x86_64 is the primary test target; arm64 builds are produced but less exercised.
- **curl**: Required for downloading the Firecracker binary and kernel.
- **System packages**: Setup installs `squashfs-tools` and `e2fsprogs` (for rootfs manipulation) via `apt-get` if they are missing.

### Setup process

`coop setup` prepares three artifacts:

1. **Firecracker binary**: Downloaded from the latest GitHub release and stored in the data directory. The jailer binary is extracted alongside it.
2. **Guest kernel**: Fetched from Firecracker's CI S3 bucket. This is a minimal `vmlinux` image matching the Firecracker release version.
3. **Template rootfs**: Built by downloading the Firecracker CI squashfs rootfs (Ubuntu-based), unpacking it, creating an ext4 image at the configured template size, and running an install script inside a chroot. The script installs Docker, GitHub CLI, Claude Code, Codex, and profile packages. It configures the `ubuntu` user with SSH keys and sets up systemd-networkd.

All three steps are idempotent. If the artifact already exists and is up to date, setup skips it.

### How instances work

Creating an instance (`coop up`) follows this sequence:

1. Copies the template rootfs to the instance directory using `cp --reflink=auto` for copy-on-write on supported filesystems.
2. Mounts the copy and patches the guest network config with the instance's unique IP address and hostname.
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
- A `FORWARD -i br0 -o br0 -j DROP` rule is inserted at the head of the chain.
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

Both backends support the same CLI commands and guest capabilities:

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
