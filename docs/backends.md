# Platform Backends

coop selects its VM backend at compile time. macOS builds use Lima. Linux builds use Firecracker. The binary determines the backend; there is no runtime override.

Both backends expose the same CLI commands and produce the same guest environment: Ubuntu with Docker, GitHub CLI, and Claude Code pre-installed. The backends differ in how they create and manage the VM underneath.

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
3. The provision script installs all packages (Docker, GitHub CLI, Claude Code, and any profile packages), creates the `ubuntu` user with SSH access, and enables services.
4. After provisioning completes, cleans cloud-init state so it re-runs on cloned instances.
5. Stops the builder VM and extracts its disk as the golden image.
6. Generates a fast-start Lima template that references the golden image directly. No cloud-init provisioning runs on instance start.

The builder VM is deleted after extraction, whether the build succeeds or fails.

### How instances work

Each instance is a Lima VM created with `limactl start` using the fast-start template. Lima names are prefixed with `coop-` (e.g., `coop-my-instance`). The instance gets its own disk, a copy-on-write layer over the golden image. Lima handles SSH port allocation automatically; coop reads the assigned port from `limactl list --json`.

The Lima template configures:
- `vmType: "vz"` (Virtualization.framework, not QEMU)
- Rosetta enabled for x86_64 binary translation on Apple Silicon
- `mountType: "virtiofs"` with no host mounts (empty `mounts: []`). When `coop start --mount` is used, Lima adds virtiofs mount entries for the specified host directories, providing live mounts where changes are visible immediately on both sides.
- Lima's built-in containerd disabled (Docker is installed in the guest instead)

### Disk resize

Resizing a stopped instance truncates the Lima disk to the new size. Cloud-init's `growpart` module expands the partition and filesystem on next boot. Shrinking is not supported.

### Resource ownership

Lima runs as the current user. No `sudo` is required for any Lima operation: setup, start, stop, or destroy.

## Linux / Firecracker

The Firecracker backend runs [Firecracker microVMs](https://firecracker-microvm.github.io/) with KVM hardware virtualization. Each instance is a lightweight VM with its own rootfs, TAP network device, and Firecracker process.

### Prerequisites

- **KVM access**: `/dev/kvm` must exist and be readable/writable by the current user. Setup checks this and offers to fix permissions via `setfacl` or by adding the user to the `kvm` group.
- **x86_64 architecture**: The Firecracker backend targets x86_64 Linux hosts.
- **curl**: Required for downloading the Firecracker binary and kernel.
- **System packages**: Setup installs `squashfs-tools` and `e2fsprogs` (for rootfs manipulation) via `apt-get` if they are missing.

### Setup process

`coop setup` prepares three artifacts:

1. **Firecracker binary**: Downloaded from the latest GitHub release and stored in the data directory. The jailer binary is extracted alongside it.
2. **Guest kernel**: Fetched from Firecracker's CI S3 bucket. This is a minimal `vmlinux` image matching the Firecracker release version.
3. **Template rootfs**: Built by downloading the Firecracker CI squashfs rootfs (Ubuntu-based), unpacking it, creating an ext4 image at the configured template size, and running an install script inside a chroot. The script installs Docker, GitHub CLI, Claude Code, and profile packages. It configures the `ubuntu` user with SSH keys and sets up systemd-networkd.

All three steps are idempotent. If the artifact already exists and is up to date, setup skips it.

### How instances work

Creating an instance (`coop start`) follows this sequence:

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
- Each instance's TAP device is created, attached to the bridge, and brought up.
- Guest IPs are assigned statically: `172.16.0.{index + 2}`. Instance 0 gets `172.16.0.2`.

On teardown, the TAP device is removed. If no TAP devices remain on the bridge, the bridge and all associated iptables rules are also removed.

### Network configuration

The `network` section in `config.toml` controls Firecracker networking:

| Field | Default | Description |
|---|---|---|
| `host_ip` | `172.16.0.1` | IP address assigned to the bridge on the host side |
| `subnet_mask` | `/24` | CIDR subnet mask for the bridge network |
| `host_iface` | `auto` | Host interface for NAT. `auto` detects the default route interface. Set explicitly if auto-detection fails. |

These settings are ignored on macOS. Lima handles its own networking.

### Disk resize

Resizing a stopped Firecracker instance runs `truncate` to extend the rootfs image, then `e2fsck -fy` and `resize2fs` to grow the filesystem in place. Shrinking is not supported.

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
| `coop start` | `limactl start` with fast-start template | Copies rootfs, configures TAP, starts Firecracker |
| `coop stop` | `limactl stop` | API socket shutdown, SIGTERM, SIGKILL |
| `coop destroy` | `limactl delete --force` | Kill process, remove TAP, delete instance dir |
| `coop status` | Queries `limactl list --json` | Reads PID file, queries guest via SSH |
| `coop logs` | Reads Lima's `serial.log` | Reads Firecracker log file |
| `coop shell` | SSH to localhost on Lima-assigned port | SSH to guest IP on configured port |
| `coop resize` | Truncates Lima disk | Truncates + resize2fs on rootfs |
| Resource monitoring | SSH query to guest | SSH query to guest |
| Docker in guest | Works (full kernel) | Works (with iptables-legacy workaround) |
| `--mount` host mounts | Live virtiofs (changes visible immediately) | One-time rsync sync (use `push`/`pull` to re-sync) |
| Needs sudo | No | Yes (VM start, stop, networking, rootfs ops) |
