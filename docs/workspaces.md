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

#### Mounting a git repository (live-mount caveat)

When a `--mount` source contains a `.git` entry and the backend is a live mount (Lima), git operations inside the guest can write absolute guest paths into the shared `.git/config`. Common triggers:

- `git worktree add` records `core.worktree = /workspace/...` in the worktree's config.
- `prek install` (and `git config core.hooksPath`) records `core.hooksPath = /workspace/.git/hooks`.

Because the mount is live, those entries appear on the host as well. After the VM exits, every host `git` invocation fails with `fatal: Invalid path '/workspace': No such file or directory`. The workaround is to remove the offending lines from `.git/config` (and `.git/worktrees/*/config`).

coop prints a warning at start time when a live-mount source is a git repo. To avoid the issue, do not run commands inside the guest that record absolute paths in `.git/config` — in particular, `git worktree add` and `prek install` (or any other tool that calls `git config core.hooksPath`).

Switching from `--mount` to `--workspace` does not on its own fix this: `--workspace` copies the repo into the guest, but `.git/` is included by default and is brought back by `coop pull`, so corrupted config entries written inside the guest still reach the host. Either avoid the offending commands or pass `--exclude-git` on `coop pull`.

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
coop pull                                       # uses host_path from workspace.json
coop pull --dir ./local-copy                    # pull into a specific directory
coop pull --force                               # skip local dirty check
coop pull my-instance                           # target a specific instance
coop pull my-instance --dir ./local-copy        # combined
```

Before overwriting the local destination, `pull` runs `git status --porcelain` against it. If the directory has a `.git` and any uncommitted changes (tracked or untracked), pull refuses unless you pass `--force`. Unlike push's guest-side check, the local check does not inspect unpushed commits — committing your local work first is enough to satisfy it.

The destination directory is created if absent. Transport selection follows the same rsync-then-tar-pipe order. The tar-pipe fallback verifies SHA-256 checksums end-to-end.

## Default exclusions

All transfers (rsync and tar-pipe) exclude these reproducible build and cache directories:

- `node_modules/`
- `target/`
- `__pycache__/`
- `.venv/`
- `.coop/`

`.git/` is **included** by default so agents in the guest get full history, branches, and the ability to make commits that survive a `coop pull`. Pass `--exclude-git` to `coop start`, `coop push`, or `coop pull` to skip it on a per-transfer basis (useful for very large repos where transfer time dominates).

## .gitignore integration

When rsync is available, transfers pass `--filter=':- .gitignore'`. Rsync reads `.gitignore` files at each directory level and skips matching paths.

The tar-pipe fallback on Linux uses GNU tar's `--exclude-vcs-ignores` for the same effect. On macOS, BSD tar lacks this flag, so only the default exclusions above apply.

### `.git/` and .gitignore

A repo whose `.gitignore` lists `.git/` (rare, but legal — sometimes seen in dotfile repos or repos vendoring other repos) gets special handling so the new include-by-default behaviour is not silently undone:

- **rsync**: a protective `--filter=+ /.git/***` is prepended before the per-directory `.gitignore` merge, so `.git/` and its contents are always transferred unless `--exclude-git` is passed.
- **GNU tar (Linux)**: `--exclude-vcs-ignores` is all-or-nothing. If your `.gitignore` lists `.git/`, the tar-pipe transport will skip it. Pass `--exclude-git` explicitly if that is what you want, or remove the entry from `.gitignore`.
- **BSD tar (macOS)**: not affected — it doesn't read `.gitignore` at all.

## Checksum verification

Every tar-pipe transfer hashes the archive with SHA-256 on both the sending and receiving sides. A mismatch fails the transfer and reports both hash values. This detects truncated streams, network corruption, and disk errors.

Rsync handles integrity internally. No additional checksumming is layered on top.
