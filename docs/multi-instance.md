# Running Multiple Instances

coop runs multiple VM instances simultaneously. Each instance gets its own name, disk, network identity, and lifecycle.

## Named vs. auto-named instances

Every instance has a name. You provide one explicitly or let coop generate one from the allocated index.

```
# Named instance
coop start my-project

# Auto-named instance (uses the allocated index, e.g. "0", "1")
coop start
```

Named instances pay off when you have several running at once. The name appears in `coop status` output and targets commands at a specific instance.

### Name validation rules

Instance names must satisfy all of the following:

- 1 to 64 characters long
- Characters limited to `a-z`, `A-Z`, `0-9`, `-`, `_`

Names that violate these rules are rejected at creation time.

## Addressing instances

Most commands accept an optional instance name. Resolution follows three cases:

- **Zero instances exist.** The command fails with a message to run `coop start`.
- **Exactly one instance exists.** The name is optional. coop auto-selects it.
- **Multiple instances exist.** The name is required. coop lists the available names in the error if you omit it.

```
# With one instance, these are equivalent:
coop shell
coop shell my-project

# With multiple instances, you must specify:
coop shell my-project
coop shell another-project
```

This applies to `shell`, `stop`, `destroy`, `status <name>`, `logs`, `push`, `pull`, `exec`, `vscode`, `resize`, `claude`, and `codex`.

## Checking status

### All instances

`coop status` with no arguments lists every instance in a table:

```
$ coop status
my-project       running    default    lima       load=0.42 mem=50% disk=25%
another-project  stopped    default    lima
```

Each row shows the instance name, state (`running` or `stopped`), the image it was created from, the backend (`firecracker` or `lima`), and a resource usage summary for running instances: 1-minute load average, memory percentage, and disk percentage.

### Single instance

Pass a name to get detailed information for one instance:

```
$ coop status my-project
```

The output format depends on the backend. It includes the full resource breakdown: load average, memory used/total in MiB, disk used/total in MiB.

## Per-instance image selection

Each instance can use a different golden image. Build images with `coop setup --image <name>`, then select one at start time:

```
coop setup --image python --profile python
coop setup --image node --profile node

coop start py-work --image python
coop start js-work --image node
```

Omitting `--image` selects the image named `default`.

## Per-instance disk sizing

### At creation time

`--disk` sets the instance disk size in GiB:

```
coop start big-project --disk 100
```

The instance disk grows from the template size when the requested size is larger.

### After creation

`coop resize` changes the disk size of a stopped instance:

```
# Absolute size
coop resize --size 150
coop resize my-project --size 150

# Relative size (grow by 20 GiB)
coop resize my-project --size +20
```

The instance must be stopped first. coop rejects the resize and tells you to stop the instance if it is running.

## Independent lifecycle

Each instance is fully independent. Start, stop, and destroy instances without affecting others:

```
coop start frontend
coop start backend
coop stop frontend        # backend keeps running
coop destroy frontend     # backend unaffected
```

To destroy all instances and shared resources (images, kernel, SSH keys) at once:

```
coop destroy --all
```

## Instance directory structure

All instance data lives under `~/.coop/instances/<name>/`. Each instance directory contains:

| File | Purpose |
|------|---------|
| `instance.json` | Instance metadata (name, index, image) |
| `rootfs.ext4` | Instance root filesystem (Firecracker) |
| `firecracker.pid` | PID of the running Firecracker process |
| `firecracker.socket` | Firecracker API socket |
| `firecracker.log` | Serial console log |
| `vm_config.json` | Firecracker VM configuration |
| `workspace.json` | Workspace sync state (host path, guest path, source) |
| `vsock.sock` | Vsock socket for host-guest communication |

On Lima, the directory structure differs because Lima manages its own VM state. `instance.json` and `workspace.json` are always present regardless of backend.

## Concurrent access and file locking

When allocating a new instance, coop acquires an exclusive file lock (`flock`) on the instances directory. This prevents two concurrent `coop start` invocations from claiming the same index or name. The lock is held only during index allocation and released automatically when the operation completes.

## Index allocation

Each instance is assigned a numeric index (0 through 252) that derives its network identity: guest IP address, TAP device name, MAC address, and vsock CID. Allocation starts from the highest existing index plus one. When the ceiling is reached, it wraps around to fill gaps at the low end. The 253-instance limit comes from the available IP range in the `172.16.0.0/24` subnet (addresses 2 through 254, excluding network, host, and broadcast).
