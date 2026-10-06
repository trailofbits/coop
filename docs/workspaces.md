# Workspace Sync

coop moves code between the host and guest VM. The normal way to get code in is `coop up`, with `push` and `pull` for ongoing sync.

## Getting Code into the VM

### Project environment (`coop up`)

```bash
coop up ./my-project
coop up ./my-project --mount
coop up --git-repo https://github.com/trailofbits/coop.git
```

`coop up` treats the directory as the project identity. Re-running the same
command finds the existing instance for that directory instead of allocating
another VM. The default transport is copy/sync into `/workspace`; `--mount`
uses mount transport for the project directory. On macOS/Lima this is live
filesystem sharing; on Linux/Firecracker it is a one-time sync. Use
`--extra-mount HOST:GUEST` for additional mounted data directories when
creating the project instance. If the instance already exists, destroy it
first to change creation-time choices such as transport, image, disk size, or
extra mounts.

`coop up` in copy mode tar-pipes the project into `/workspace` inside the
guest over SSH. Both sides independently SHA-256-hash the tar stream. If the
checksums diverge, the transfer aborts. coop persists the host-to-guest path
mapping in `workspace.json` so that later `push` and `pull` calls resolve paths
automatically.

`coop up --mount` mounts the project directory into the guest. Behavior differs
by backend:

- **Lima (macOS)**: Live virtiofs mount. Changes on host are visible in guest immediately and vice versa.
- **Firecracker (Linux)**: One-time rsync sync at boot. Not a live mount. Use
  `coop push` to update the guest, or `coop pull --dir <new-directory>` to
  retrieve guest changes for review.

Additional host data can be mounted at creation time with
`coop up --extra-mount HOST_PATH:GUEST_PATH`. In copy mode, extra mounts must
not target `/workspace`, because the copied project owns that path.

`coop up --git-repo <url>` clones the repository inside the guest at
`/workspace` and records the original URL in `workspace.json`. Because there
is no host workspace path for that source, later `push` and `pull` commands
need an explicit `--dir` if you want to sync files back to the host.

#### Mounting a git repository (live-mount caveat)

When a `--mount` source contains a `.git` entry and the backend is a live mount (Lima), git operations inside the guest can write absolute guest paths into the shared `.git/config`. Common triggers:

- `git worktree add` records `core.worktree = /workspace/...` in the worktree's config.
- `prek install` (and `git config core.hooksPath`) records `core.hooksPath = /workspace/.git/hooks`.

Because the mount is live, those entries appear on the host as well. After the VM exits, every host `git` invocation fails with `fatal: Invalid path '/workspace': No such file or directory`. The workaround is to remove the offending lines from `.git/config` (and `.git/worktrees/*/config`).

coop prints a warning at start time when a live-mount source is a git repo. To avoid the issue, do not run commands inside the guest that record absolute paths in `.git/config` — in particular, `git worktree add` and `prek install` (or any other tool that calls `git config core.hooksPath`).

Copy mode does not create a live filesystem share. Host-to-guest transfers
include `.git/`; guest-to-host pulls attempt to omit common `.git` paths but do
not promise to identify every filesystem alias. Treat the pulled directory as
untrusted even when the ordinary `.git` entry was not copied back.

### Manual via SSH

```bash
coop shell
# then use git clone, scp, or any other tool inside the guest
```

No workspace state is recorded. `push` and `pull` will not work without a `workspace.json`.

## State file: `workspace.json`

Creating a project VM with `coop up` writes a `workspace.json` in the instance directory:

| Field        | Description                                                    |
|-------------|----------------------------------------------------------------|
| `host_path`  | Absolute path on the host for local workspace and mount sources |
| `guest_path` | Path inside the guest VM (always `/workspace`)                 |
| `source`     | How the workspace was created: `workspace`, `mount`, or `git_repo` |

`push` and `pull` read this file to resolve default paths.

## Pushing: host to guest

```bash
coop push                                    # uses host_path from workspace.json
coop push --dir ./other-dir                  # push a specific directory
coop push --force                            # skip guest dirty check
coop push my-instance                        # target a specific instance
coop push my-instance --dir ./src --force    # combined
```

Before overwriting guest files, `push` checks for in-guest work the host doesn't yet know about. Two signals are inspected:

- `git status --porcelain --untracked-files=no` — modifications to tracked files. Untracked files are skipped because they're usually host-side build artifacts that were copied into the guest at start time, not work done by an in-guest agent.
- `git rev-list --count '@{u}..HEAD'` — commits on the current branch that are ahead of its upstream. Catches in-guest commits that a host push would otherwise silently overwrite.

If either signal finds anything, push prints it and exits. `--force` overrides both.

Transfer method selection is automatic:

1. **rsync** if the guest has it. Uses `--delete` to mirror the host directory exactly. Reads `.gitignore` files via `--filter=':- .gitignore'`.
2. **tar-pipe** otherwise. Streams a tar archive over SSH with end-to-end SHA-256 verification.

## Pulling: guest to host

```bash
coop pull                                       # uses host_path; must be empty unless --force
coop pull --dir ./local-copy                    # pull into a specific directory
coop pull --force                               # allow a nonempty destination
coop pull my-instance                           # target a specific instance
coop pull my-instance --dir ./local-copy        # combined
```

Pull never invokes Git on the host. It cannot safely use `git status` to inspect
a directory that may contain files or repository metadata from an untrusted
guest. Pull therefore accepts a missing or empty destination by default and
refuses a nonempty destination unless `--force` is supplied. `--force`
authorizes overwriting matching destination files; it does not disable transfer
checks or make the received files trusted.

Everything returned by pull is controlled by the guest and may be malicious.
Review the result before executing it or interpreting it with Git, an editor, a
build tool, a shell, or another host application. Upgrading coop does not repair
repositories pulled by an affected older release; recreate their Git metadata
from a trusted source before using host Git on them.

The transports attempt to exclude common `.git` names, including nested ASCII
case variants. Both pull paths receive into an empty staging directory and then
apply the filter again during a trusted host-side installation; the tar path
also filters during host extraction. This is useful defense-in-depth, not a
guarantee that every Git-administration alias will be recognized across all
transport and filesystem implementations. Pull has no `--exclude-git` option;
the filtering behavior is unconditional and best effort.

## Default exclusions

All transfers (rsync and tar-pipe) exclude these reproducible build and cache directories:

- `node_modules/`
- `target/`
- `__pycache__/`
- `.venv/`
- `.coop/`

When coop uses tar on a macOS host, host-side tar archive creation runs with
`COPYFILE_DISABLE=1`. This suppresses tar-generated AppleDouble (`._*`) entries
for resource forks or extended attributes while packing (the setting has no
effect on extraction). Without it, those metadata entries can land in a Linux
guest as ordinary files, including inside `.git/`, where they can break Git's
pack/ref discovery.

Host-to-guest transfers (`coop up` and `coop push`) include `.git/` by default
so agents receive full history and branches. Pass `--exclude-git` to skip it.
Guest-to-host pulls attempt to exclude common `.git` paths as defense-in-depth.
Do not rely on pull to sanitize guest content or make it safe for host Git.

## .gitignore integration

When rsync is available, transfers pass `--filter=':- .gitignore'`. Rsync reads `.gitignore` files at each directory level and skips matching paths.

The tar-pipe fallback on Linux uses GNU tar's `--exclude-vcs-ignores` for the same effect. On macOS, BSD tar lacks this flag, so only the default exclusions above apply.

### `.git/` and .gitignore

A repo whose `.gitignore` lists `.git/` (rare, but legal — sometimes seen in dotfile repos or repos vendoring other repos) gets special handling on host-to-guest transfers so the include-by-default behaviour is not silently undone:

- **rsync push**: a protective `--filter=+ /.git/***` is prepended before the per-directory `.gitignore` merge, so `.git/` and its contents are transferred unless `--exclude-git` is passed.
- **GNU tar push (Linux)**: `--exclude-vcs-ignores` is all-or-nothing. If your `.gitignore` lists `.git/`, the tar-pipe transport will skip it. Pass `--exclude-git` explicitly if that is what you want, or remove the entry from `.gitignore`.
- **BSD tar (macOS)**: not affected — it doesn't read `.gitignore` at all.
- **Pulls**: transports attempt to exclude common `.git` entries independently
  of `.gitignore`; this is not a security guarantee.

## Checksum verification

Host-to-guest tar-pipe transfers hash the archive with SHA-256 on both the
sending and receiving sides. A mismatch fails the transfer and reports both
hash values. Guest-to-host pull does not add an application-level checksum: it
relies on SSH transport integrity and checks both the remote tar and local
extraction status. A checksum supplied by the untrusted guest would not make
guest-authored content trustworthy.

Rsync handles integrity internally. No additional checksumming is layered on top.
