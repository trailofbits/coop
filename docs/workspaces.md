# Workspace Sync

coop moves code between the host and guest VM. There are three ways to get code in, and two commands, `push` and `pull`, for ongoing sync.

## Getting Code into the VM

### Local directory (`--workspace`)

```bash
coop start --workspace ./my-project
```

This tar-pipes the contents of `./my-project` into `/workspace` inside the guest over SSH. Both sides independently SHA-256-hash the tar stream. If the checksums diverge, the transfer aborts.

coop persists the host-to-guest path mapping in `workspace.json` so that later `push` and `pull` calls resolve paths automatically.

### Git clone (`--git-repo`)

```bash
coop start --git-repo https://github.com/org/repo.git
```

Clones the repository inside the guest at `/workspace/repo`. Nothing transfers from the host. The resulting `workspace.json` has a null `host_path`, so `push` and `pull` require an explicit directory argument.

For private GitHub repositories, coop resolves a host-side token (`gh auth token` first, then `GITHUB_TOKEN`) and forwards it to git in the guest via a one-shot credential helper for this clone only. The token never appears on argv and never persists in the guest. Without a token, the clone runs unauthenticated and will fail for private repos.

### Host mount (`--mount`)

```bash
coop start --mount ~/data
coop start --mount ~/data:/mnt/data
```

Mounts a host directory into the guest. Behavior differs by backend:

- **Lima (macOS)**: Live virtiofs mount. Changes on host are visible in guest immediately and vice versa.
- **Firecracker (Linux)**: One-time rsync sync at boot. Not a live mount. Use `coop push` / `coop pull` to sync changes afterward.

If GUEST_PATH is omitted, defaults to `/workspace`. Conflicts with `--workspace` and `--git-repo`.

### Manual via SSH

```bash
coop shell
# then use git clone, scp, or any other tool inside the guest
```

No workspace state is recorded. `push` and `pull` will not work without a `workspace.json`.

## State file: `workspace.json`

Starting a VM with `--workspace`, `--git-repo`, or `--mount` writes a `workspace.json` in the instance directory:

| Field        | Description                                                    |
|-------------|----------------------------------------------------------------|
| `host_path`  | Absolute path on the host (null for `--git-repo` workspaces)  |
| `guest_path` | Path inside the guest VM (always `/workspace`)                 |
| `source`     | How the workspace was created: `workspace`, `git_repo`, or `mount` |

`push` and `pull` read this file to resolve default paths.

## Pushing: host to guest

```bash
coop push                     # uses host_path from workspace.json
coop push ./other-dir         # push a specific directory
coop push --force             # skip guest dirty check
coop push --name my-instance  # target a specific instance
```

Before overwriting guest files, `push` runs `git status --porcelain` inside the guest workspace. If there are uncommitted changes, push prints them and exits. `--force` overrides this.

Transfer method selection is automatic:

1. **rsync** if the guest has it. Uses `--delete` to mirror the host directory exactly. Reads `.gitignore` files via `--filter=':- .gitignore'`.
2. **tar-pipe** otherwise. Streams a tar archive over SSH with end-to-end SHA-256 verification.

## Pulling: guest to host

```bash
coop pull                     # uses host_path from workspace.json
coop pull ./local-copy        # pull into a specific directory
coop pull --force             # skip local dirty check
coop pull --name my-instance  # target a specific instance
```

Same dirty-check logic as push, applied to the local destination. If the local directory has a `.git` and uncommitted changes, pull refuses unless you pass `--force`.

The destination directory is created if absent. Transport selection follows the same rsync-then-tar-pipe order. The tar-pipe fallback verifies SHA-256 checksums end-to-end.

## Default exclusions

All transfers (rsync and tar-pipe) unconditionally exclude:

- `.git/`
- `node_modules/`
- `target/`
- `__pycache__/`
- `.venv/`
- `.coop/`

These are not configurable.

## .gitignore integration

When rsync is available, transfers pass `--filter=':- .gitignore'`. Rsync reads `.gitignore` files at each directory level and skips matching paths.

The tar-pipe fallback on Linux uses GNU tar's `--exclude-vcs-ignores` for the same effect. On macOS, BSD tar lacks this flag, so only the default exclusions above apply.

## Checksum verification

Every tar-pipe transfer hashes the archive with SHA-256 on both the sending and receiving sides. A mismatch fails the transfer and reports both hash values. This detects truncated streams, network corruption, and disk errors.

Rsync handles integrity internally. No additional checksumming is layered on top.
