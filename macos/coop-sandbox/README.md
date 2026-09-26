# coop-sandbox

The macOS VM runtime behind coop's opt-in `apple-container` build: persistent
Linux sandboxes on [`apple/containerization`](https://github.com/apple/containerization)
0.45.0 (pinned exactly in `Package.swift`). coop drives it through the JSON CLI
below; see [`docs/backends.md`](../../docs/backends.md) for the coop side and
[`docs/trust-model.md`](../../docs/trust-model.md) for the isolation contract.

Each sandbox is one Linux VM running systemd from its own ext4 disk, on its own
vmnet network, with no host mounts, socket relays, published ports, or SSH-agent
forwarding. The sandbox record has no field for any of those, so they cannot be
configured. A running sandbox is owned by one `coop-sandbox run` process, which
holds the VM (Virtualization.framework runs it in-process) and serves a
peer-UID-checked 0600 Unix socket for exec/stop/inspect. `start` loads that
owner as a launchd job from a plist in the sandbox directory. launchd respawns
it if it is killed, and nothing starts at login.

## Build

```bash
scripts/build-coop-sandbox.sh [PREFIX]    # default ~/.local/opt/coop-sandbox
swift test --package-path macos/coop-sandbox --no-parallel
./tests/integration-apple-sandbox.sh      # boots real VMs; ~10 min
```

The binary needs only the `com.apple.security.virtualization` entitlement and is
signed ad hoc. Requires Xcode (Swift 6.2+) and macOS 26+ on Apple Silicon.

## CLI (protocol 2)

Every command except `version` takes `--root <absolute path>`, the state root.
It is canonicalized with realpath(3), and every path the runtime reports lies
under it. Commands that report state print JSON on stdout; `exec` and `logs`
pass output through.

```text
version                                   {name, version, protocol, containerization}
init --kernel K                           pinned kernel (sha256 allowlist) + vminit 0.45.0 initfs
image import --oci-tar T | image list | image delete REF
create ID (--image REF | --from-disk NAME) --cpus N --memory-mib M --disk-gib G --owner O
start ID [--wait-seconds S]               launchd job; returns once the owner answers
stop ID [--timeout-seconds S]             systemd halt (SIGRTMIN+3), then unload the job
exec [-i] [--timeout S] ID -- ARGV        root, over vsock; exit code is passed through
inspect ID                                {record, status, live, effective, disk}
list                                      [{id, status, owner}]
set ID [--cpus N] [--memory-mib M] [--operation OP] [--expect-operation OP]
                                          stopped only; applied at the next start
grow ID --disk-gib G [--operation OP]     stopped only; offline e2fsck + resize2fs
commit ID NAME [--replace]                save the disk with host keys and machine-id removed
restore ID (NAME | --image REF) [--operation OP]
                                          replace the disk; bumps record.diskGeneration
disk list | disk delete NAME
maintenance install --image REF --version V
                                          unpack REF as the maintenance boot disk
maintenance inspect                       the installed maintenance artifact, or null
logs ID [-n N] [--follow]                 serial console
delete ID --owner O                       refuses another owner's sandbox
reconcile                                 clear crashed owners, finish interrupted creates, deletes,
                                          disk updates, and commits (the sweep is skipped while an
                                          operation runs)
```

Status is `running`, `booting` (owner up, control channel not yet answering),
`stopped`, or `crashed` (owner died; `start` recovers).

`set`, `grow`, and `restore` record `--operation` (or a generated id) as
`record.lastOperation` when they commit, so a caller can tell after a crash
whether its own operation applied. `set --expect-operation` refuses unless the
last committed operation is still the given one: a caller undoing its change
never overwrites a newer one.

## Behaviour worth knowing

- **Subnets.** Each sandbox gets `10.231.N.0/24` from an allocator shared by
  every sandbox in the root. vmnet keeps a subnet reserved for hours after its
  owning process dies uncleanly and refuses to recreate it. The owner then
  quarantines that subnet (24 h, at most 64 entries) and moves the sandbox to
  another. The address changes; the identity does not.
- **Disks.** Images are unpacked once per (image, size) into a journaled ext4
  and APFS-cloned per sandbox, so creating from a cached base takes
  milliseconds. The formatter uses `sparse_super2`, which the guest kernel
  cannot resize online, so `grow` runs `e2fsck`/`resize2fs` in a short,
  network-less maintenance VM; the sandbox's disk is attached as data and none
  of its programs run. `commit` uses the same VM to strip host keys and
  machine-id.
- **Maintenance image.** Maintenance VMs boot a disposable clone of a disk
  installed by `maintenance install`: a small image (a shell and e2fsprogs)
  unpacked into `maintenance/`, sized from its own layers, checked for the
  programs the scripts run, and kept apart from the image store, so deleting
  or replacing an application image never affects it. Without one, `grow`,
  `commit`, and a `create`/`restore` that must grow fail before any disk
  changes. coop builds and installs it during `coop setup`.
- **Disk updates.** `grow` and `restore` prepare the new disk on a scratch
  clone, then publish it through one path: the new record is staged beside
  the prepared disk's inode, the disk is renamed into place, and the record is
  written. After a crash the next guarded operation (or the owner, or
  `reconcile`) finishes an update whose disk was installed and discards one
  whose disk was not, so disk and record always describe the same update.
  Unreadable or contradictory staged state is an error, never a guess. This
  covers process crashes, not sudden power loss. `commit` publishes a disk
  and its metadata the same way (`disks/.pending-<name>.json`), so an
  interrupted `commit --replace` keeps the old disk with its own metadata.
- **Private state.** The state root and its directories are made 0700 and
  owned by the running user (a symlinked or foreign-owned one is refused);
  the binary runs with umask 077 and clones disks 0600.
- **Serialization.** Every mutation of a sandbox (create, start, set, grow,
  commit, restore, delete) holds that sandbox's guard (`locks/sandbox-<id>.lock`)
  across its stopped check and its change; other sandboxes are unaffected. An
  owner takes the same guard to claim its sandbox, whether `start` or a
  launchd respawn launched it, so a VM never boots while its disk is being
  replaced. A committed disk has its own lock, so a clone never pairs one
  version's disk with another's metadata. All are flock(2) locks, released by
  the kernel when their holder dies; `FileLock` in `Layout.swift` documents
  the order they are taken in. The invariants behind this are in
  [`docs/design/apple-sandbox-transactions.md`](../../docs/design/apple-sandbox-transactions.md).
- **Console log.** The serial console is copied to `boot.log`, capped at
  8 MiB: past the cap the file restarts with a marker line, so a guest
  flooding its console cannot fill the host disk. `logs -n` reads only the
  file's last 256 KiB.
- **Stop.** `stop` asks systemd to halt and forces the VM down after 60 s.
  Killing the owner powers the VM off: journaled ext4 recovers, but unsynced
  guest writes can be lost.
