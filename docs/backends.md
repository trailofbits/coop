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
3. The provision script installs all packages (Docker, GitHub CLI, Claude Code, Codex, Grok Build, and any profile packages), creates the `ubuntu` user with SSH access, and enables services.
4. After provisioning completes, cleans cloud-init state so it re-runs on cloned instances.
5. Stops the builder VM and extracts its disk as the golden image.
6. Generates a fast-start Lima template that references the golden image directly. No cloud-init provisioning runs on instance start.

The builder VM is deleted after extraction, whether the build succeeds or fails.

### How instances work

Each instance is a Lima VM created with `limactl start` using the fast-start template. Lima names are prefixed with `coop-` (e.g., `coop-my-instance`). The instance gets its own disk, a copy-on-write layer over the golden image. Lima handles SSH port allocation automatically; coop reads the assigned port from `limactl list --json`.

The Lima template configures:
- `vmType: "vz"` (Virtualization.framework, not QEMU)
- Rosetta enabled for x86_64 binary translation on Apple Silicon
- `mountType: "virtiofs"` with no host mounts (empty `mounts: []`). When `coop up --mount` is used, Lima adds virtiofs mount entries for the specified host directories, providing live mounts where changes are visible immediately on both sides. Host and guest paths are serialized as YAML scalars so special characters remain part of the path.
- Lima's built-in containerd disabled (Docker is installed in the guest instead)

### Resize (disk, memory, vCPUs)

Resizing a stopped instance's disk truncates the Lima disk to the new size and updates the instance `lima.yaml` `disk:` field to match, so the next start sees the grown size. Cloud-init's `growpart` module expands the partition and filesystem on next boot. Shrinking is not supported.

Memory and vCPU changes rewrite the `cpus`/`memory` fields in the instance's `lima.yaml`, which Lima re-reads on `limactl start`. The edit is written atomically, then coop starts the instance to validate and apply the new spec — if `limactl` rejects it (e.g. a spec larger than the host), the previous `lima.yaml` is restored. Without `--start` the instance is stopped again after the validating boot. The `lima.yaml` is authoritative: the global `[vm]` `cpus`/`memory` settings only seed *new* instances.

### Resource ownership

Lima runs as the current user. No `sudo` is required for any Lima operation: setup, start, stop, or destroy.

### SSH resets or times out while Lima says Running

If a coop command fails with `kex_exchange_identification: read: Connection reset by peer`, or `coop start` times out waiting for SSH, inspect the Lima messages too. A repeated `tcpproxy: error dialing "<guest-ip>:22": ... no route to host` and/or an unavailable vsock guest agent can occur while `limactl list` still says `Running`. That state describes the VZ process, not a completed guest boot or a working SSH server. The address in the message is diagnostic; do not assume it stays the same.

Try the less invasive recovery steps first:

```sh
coop stop <instance>
coop start <instance>
```

Check the host's VPN and network state. If several VZ guests or host networking are affected, reboot macOS and retry. Do not start recovery by running `coop destroy`: it deletes the instance disk. If the same SSH and guest-agent errors persist, they may be symptoms of a guest boot or filesystem failure rather than a host network problem. `coop logs <instance>` and Lima's logs under the instance directory may show where boot stopped.

To inspect the VZ console, stop the instance, enable Lima video output, then start it directly:

```sh
coop stop <instance>
limactl edit --tty=false --video coop-<instance>
limactl start coop-<instance>
```

The display may show an initramfs prompt asking for a manual `fsck`. Follow the device named by the boot message only after confirming it is the root filesystem and is not mounted. If console repair is unavailable, use the offline procedure below. After diagnosis, stop the VM before touching its disk; `limactl edit --tty=false --video=false coop-<instance>` restores the usual headless display while it is stopped.

#### Offline filesystem repair on macOS

**Never run host-side `fsck` or `e2fsck` on an instance disk while its VM is running.** Filesystem repair changes guest data and can lose damaged files. Keep a separate backup so you can recover the pre-repair disk.

**Host-side repair crosses coop's VM isolation boundary.** The guest disk is untrusted. Attaching it causes macOS to parse guest-controlled partition metadata, and the commands below run `debugfs` and `e2fsck` as root on guest-controlled filesystem metadata. A maliciously crafted or corrupted disk could exploit a vulnerability in those host parsers and compromise the host. The safer option is to perform the diagnosis and repair inside a disposable VM that has access only to the stopped instance disk; configuring such a helper VM is outside the scope of this guide. Proceed with the host-side steps only if you accept this risk.

1. Run `coop stop <instance>` and check `limactl list` until `coop-<instance>` is `Stopped`. If a broken VZ process does not stop gracefully, `limactl stop -f coop-<instance>` is a last resort: it kills the VM without a clean guest shutdown and may add filesystem damage. Confirm `Stopped` again before continuing.
2. Locate the instance directory at `${LIMA_HOME:-$HOME/.lima}/coop-<instance>`. Identify the instance's disk there; Lima versions use `disk` or `diffdisk`, and `disk` may be a symlink to `diffdisk`. Resolve the path to the actual file, assign that absolute path to `disk_image`, and print it for confirmation. Verify that it is inside the selected instance directory and is not the golden image or another instance's disk. Copy it to a **separate backup location** with enough free space, for example:

   ```sh
   disk_image="/absolute/path/to/coop-INSTANCE/diffdisk"
   backup_image="/separate/backup/location/diffdisk.pre-fsck-YYYYMMDDTHHMMSS"
   printf 'Instance disk: %s\n' "$disk_image"
   if [ -e "$backup_image" ]; then
       printf 'Backup already exists: %s\n' "$backup_image" >&2
   else
       cp -p "$disk_image" "$backup_image"
   fi
   ```

   Replace both example paths, choose a new backup path, then verify that the copy completed.
3. Install `e2fsprogs` (`brew install e2fsprogs`). Attach the stopped raw instance disk read-only with `hdiutil attach -nomount -readonly -imagekey diskimage-class=CRawDiskImage "$disk_image"`. Note the whole-disk device reported by `hdiutil`, then run `diskutil list /dev/diskN` (replace `diskN` with that device). Identify the Linux ext4 **root partition** from the partition map; do not assume a partition number. If more than one ext4 partition exists, inspect each candidate with `sudo "$(brew --prefix e2fsprogs)/sbin/debugfs" -R 'stat /etc' /dev/rdiskNsY` and use the one containing the guest root directory. Do not select an EFI, swap, or data partition. If you cannot identify the root partition confidently, detach `/dev/diskN` and stop.
4. Run a read-only diagnostic on the confirmed root partition: `sudo "$(brew --prefix e2fsprogs)/sbin/e2fsck" -fn /dev/rdiskNsY`. Replace `rdiskNsY` with the partition you identified, and record the findings and exact exit status. Status `0` means no filesystem errors were found; detach `/dev/diskN` and do not run the repair pass. Exact status `4` means errors remain uncorrected and you may continue to step 5. Any other or combined status indicates a different failure; detach `/dev/diskN` and investigate instead of attempting repair.
5. Detach the disk with `hdiutil detach /dev/diskN`, then attach the same image for writing with `hdiutil attach -nomount -imagekey diskimage-class=CRawDiskImage "$disk_image"`. **Read the new device name and partition map again**; macOS may assign a different `diskN`. Confirm the root partition again, then run `sudo "$(brew --prefix e2fsprogs)/sbin/e2fsck" -fy /dev/rdiskNsY` on that partition only. Exit status `1` means errors were corrected; `0` means no errors were found. For any other exit status or unresolved error, detach `/dev/diskN` before stopping to investigate.
6. Run `sudo "$(brew --prefix e2fsprogs)/sbin/e2fsck" -fn /dev/rdiskNsY` again, record its result, and detach with `hdiutil detach /dev/diskN` regardless of the result. Run `coop start <instance>` and verify SSH works only if the final check was clean and returned exit status `0`.

Keep the backup until the guest boots and its files are checked. If `hdiutil` cannot attach the disk, stop. If a later step fails, detach any attached disk before stopping; never boot or retry the instance while its disk remains attached to the host.

## Linux / Firecracker

The Firecracker backend runs [Firecracker microVMs](https://firecracker-microvm.github.io/) with KVM hardware virtualization. Each instance is a lightweight VM with its own rootfs, TAP network device, and Firecracker process.

### Prerequisites

- **KVM access**: `/dev/kvm` must exist and be readable/writable by the current user. Setup checks this and offers to fix permissions via `setfacl` or by adding the user to the `kvm` group.
- **x86_64 or arm64 architecture**: The Firecracker backend supports both. x86_64 is the primary test target; arm64 builds are produced but less exercised.
- **curl**: Required for downloading the Firecracker binary and kernel.
- **System packages**: Setup checks for `setfacl`, `unsquashfs`, and `mkfs.ext4` on `PATH`, plus trusted resolved `ssh`, `scp`, and `rsync` clients. If tools are missing, it offers to install their Debian/Ubuntu packages (`acl`, `squashfs-tools`, `e2fsprogs`, `openssh-client`, and `rsync`) using `apt-get`. Non-FHS OpenSSH/rsync installations can be selected with `[ssh.host_tools]`; see [configuration](configuration.md#ssh-section). If `apt-get` is unavailable, setup lists the missing tools; install or configure the tools and rerun `coop setup`. The guest remains Ubuntu regardless of the host distribution.
- **Network control-plane tools**: `sudo`, `ip`, `bridge`, `iptables`, and
  `sysctl` use validated absolute system paths. Non-FHS hosts can provide exact
  per-tool paths under `[network.host_tools]`; see
  [configuration](configuration.md#exact-network-host-tool-paths).

### Setup process

`coop setup` prepares three artifacts:

1. **Firecracker binary**: Downloaded from the latest GitHub release and stored in the data directory. The jailer binary is extracted alongside it.
2. **Guest kernel**: Fetched from Firecracker's CI S3 bucket. This is a minimal `vmlinux` image matching the Firecracker release version.
3. **Template rootfs**: Built by downloading the Firecracker CI squashfs rootfs (Ubuntu-based), unpacking it, creating an ext4 image at the configured template size, and running an install script inside a chroot. The script installs Docker, GitHub CLI, Claude Code, Codex, Grok Build, and profile packages. It configures the `ubuntu` user with SSH keys and sets up systemd-networkd plus a socket-activated AF_VSOCK listener that forwards to loopback SSH. Firecracker lifecycle traffic remains TCP-based.

All three steps are idempotent. If the artifact already exists and is up to date, setup skips it.

### How instances work

Creating an instance (`coop up`) follows this sequence:

1. Copies the template rootfs to the instance directory using `cp --reflink=auto` for copy-on-write on supported filesystems.
2. Mounts the copy and patches the guest network config with the instance's unique IP address, plus `/etc/hostname` and the matching `/etc/hosts` alias so the guest can resolve its own name.
3. Optionally resizes the rootfs if a larger disk was requested (truncate + e2fsck + resize2fs).
4. Writes a Firecracker JSON config specifying the kernel, rootfs drive, vCPU/memory allocation, network interface, and vsock device.
5. Creates and attaches a TAP device to the bridge (see TAP networking below).
6. Starts the Firecracker process with `sudo` and `--no-api`. Firecracker requires root for KVM and TAP access; coop does not expose its control API socket.
7. Records the Firecracker PID and waits for SSH to become reachable.
8. If `--mount` was specified, rsyncs the host directory into the guest. This
   is a one-time copy, not a live mount. Use `coop push` to update the guest;
   use `coop pull --dir <new-directory>` to retrieve guest changes for review.

The code uses a typestate pattern (`Configured` then `Running`) to enforce valid lifecycle transitions at compile time.

Stopping a VM requests a guest reboot over SSH and waits up to ten seconds for
Firecracker to exit. If the process does not exit, coop falls back to `SIGTERM`,
then `SIGKILL`.

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

Resizing a stopped Firecracker instance's disk extends the rootfs image if needed, then runs `e2fsck -fy` and `resize2fs` to grow the filesystem in place. Retrying the same absolute size resumes an interrupted filesystem resize when the filesystem is repairable; it also checks a fully grown filesystem. Shrinking is not supported.

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

coop supports cross-compiling from macOS to Linux for the Firecracker backend.
The project includes Cargo linker configuration for both release targets:
`x86_64-unknown-linux-musl` uses `x86_64-linux-musl-gcc`, and
`aarch64-unknown-linux-musl` uses `aarch64-linux-musl-gcc`. The matching cross
compiler must be installed and available on `PATH`; see
[`RELEASING.md`](../RELEASING.md) for the target-scoped compiler and linker
variables used by the release preflight.

The integration test runner (`tests/run-integration.sh --remote`) automates this workflow: it cross-compiles a release build, copies the binary to the remote Linux host via scp, and runs the test suite there.

## Feature parity

Both backends support the same core CLI commands and guest capabilities, with
the intentional setup and mount differences listed below:

| Capability | Lima (macOS) | Firecracker (Linux) |
|---|---|---|
| `coop setup` | Builds golden image via builder VM | Installs binary + kernel, builds rootfs via chroot |
| Explicit `setup --extra-packages` / `--post-install` | Refused | Consumed by provisioning and image staleness checks |
| `coop up` | Creates or reconnects/restarts a project VM; `--profile` builds/starts a derived image | Copies rootfs, configures TAP, starts Firecracker; `--profile` builds/starts a derived image |
| `coop start` | Restarts a stopped Lima VM | Restarts a stopped Firecracker VM |
| `coop stop` | `limactl stop` | Guest reboot over SSH, SIGTERM/SIGKILL fallback, remove TAP |
| `coop destroy` | `limactl delete --force` | Kill process, remove TAP, delete instance dir |
| `coop status` | Queries `limactl list --json` | Reads PID file, queries guest via SSH |
| `coop logs` | Reads Lima's `serial.log` | Reads Firecracker log file |
| `coop shell` | SSH to localhost on Lima-assigned port | SSH to guest IP on configured port |
| `coop resize` | Disk: truncates Lima disk and updates `lima.yaml` `disk:`. Mem/vCPU: edits `lima.yaml`, validated via start | Disk: extends rootfs image if needed, then runs fsck + resize2fs. Mem/vCPU: edits per-instance JSON |
| Resource monitoring | SSH query to guest | SSH query to guest |
| Docker in guest | Works (full kernel) | Works (with iptables-legacy workaround) |
| `--mount` host mounts | Live virtiofs (changes visible immediately) | One-time rsync sync (use `push`/`pull` to re-sync) |
| Needs sudo | No | Yes (VM start, stop, networking, rootfs ops) |
