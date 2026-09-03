# Editor Integration (VS Code, Zed)

`coop editor` connects VS Code or Zed to a running guest VM over SSH. The same SSH config entry works with JetBrains IDEs, Cursor, and any editor that supports SSH remote development. `coop vscode` remains as an alias.

## Quick start

```bash
coop editor my-instance
```

This command:

1. Writes an SSH config block for the instance into `~/.ssh/config`.
2. Launches VS Code with `code --remote ssh-remote+coop-{name} /workspace`. When it cannot spawn a VS Code strategy, coop tries the VS Code app (`open -a 'Visual Studio Code'`, macOS only), then Zed with `zed ssh://coop-{name}/workspace`.
3. Prints the SSH config entry to stderr for manual use with other editors.

## The `coop editor` command

```
coop editor [NAME] [--project PATH] [--editor code|zed] [--clean]
```

**NAME** is the instance name. Required when multiple instances exist.

**--project** sets the remote directory the editor opens. Defaults to `/workspace`.

**--editor** pins the editor (`code` or `zed`). When omitted, coop tries VS Code first, then Zed. A strategy that cannot be launched at all — binary missing, or present but not executable — counts as a miss, so the chain continues to the next editor. If an editor starts but exits unsuccessfully, coop tries only that editor's remaining strategies and reports the failure instead of opening a different editor.

**--clean** removes the SSH config entry for the specified instance and exits. Useful for manual cleanup without destroying the instance.

```bash
coop editor my-instance --project /workspace/frontend
coop editor my-instance --editor zed
```

### Installing the `code` CLI

The `code` command must be on your PATH. If it is not installed, open VS Code and run:

> Cmd+Shift+P, then "Shell Command: Install 'code' command in PATH"

coop includes this instruction when `--editor code` cannot launch VS Code, or when no editor can be launched in auto-detect mode.

### Zed

Zed connects with `zed ssh://coop-{name}/workspace` (macOS fallback: an `open zed://ssh/...` URL when the `zed` CLI is not on PATH). Zed shells out to the system `ssh`, so it picks up the `coop-{name}` alias — host, port, user, key, and disabled host-key checking — from `~/.ssh/config` with no extra setup. To install the `zed` CLI, open Zed and run:

> Cmd+Shift+P, then "cli: install"

Two Zed-specific caveats:

- On first connect, Zed downloads a `zed-remote-server` binary inside the guest from zed.dev. If your guest has restricted egress, enable `upload_binary_over_ssh` on the `coop-{name}` entry in Zed's `ssh_connections` setting so the binary is uploaded over SSH instead:

  ```json
  {
    "ssh_connections": [
      { "host": "coop-my-instance", "upload_binary_over_ssh": true }
    ]
  }
  ```
- Zed's protocol runs over the SSH channel, so anything the guest shell prints on non-interactive startup corrupts the handshake (Zed hangs at "Starting proxy…"). Keep guest rc files quiet for non-interactive shells.

## SSH config management

coop writes SSH config entries to `~/.ssh/config`, delimited by marker comments:

```
# coop START coop-my-instance
Host coop-my-instance
    HostName 127.0.0.1
    Port 22222
    User ubuntu
    IdentityFile /path/to/.coop/ssh_key
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    LogLevel ERROR
# coop END
```

Each run of `coop editor` replaces the existing block for that instance, or creates one if none exists. To install the same block without launching an editor — for plain `ssh`/`scp`/`rsync` — use [`coop ssh-config`](commands.md#ssh-config).

### Cleanup

- **`coop editor NAME --clean`** (or **`coop ssh-config NAME --clean`**) removes the SSH config entry for the specified instance and exits. This cleans up the config without destroying the instance.
- **`coop destroy`** removes the SSH config block for the destroyed instance.
- **`coop destroy --all`** removes all coop SSH config blocks.
- **`coop stop`** leaves the SSH config block in place. A stale entry has no effect when the VM is not running, and `coop start` refreshes it on the next boot (the Lima SSH port changes per start).

## Other editors

`coop editor` prints the SSH config entry to stderr. Any SSH-capable editor can use it.

### JetBrains (IntelliJ, GoLand, CLion, etc.)

1. Run `coop editor my-instance` to generate the SSH config.
2. In your JetBrains IDE, open **File > Remote Development > SSH Connection**.
3. Select the `coop-{name}` host.
4. Set the project directory to `/workspace`.

### Cursor

Cursor uses the same Remote SSH extension as VS Code. Run `coop editor` and the host appears in Cursor's SSH targets. You can also launch it directly:

```bash
cursor --remote ssh-remote+coop-my-instance /workspace
```

### Manual SSH

The host alias works from any terminal:

```bash
ssh coop-my-instance
```

The SSH config block supplies the hostname, port, user, key, and host-key verification settings.

## Port forwarding

coop does not manage port forwarding. Use SSH directly:

```bash
ssh -L 3000:localhost:3000 coop-my-instance
```

This binds local port 3000 to port 3000 inside the guest. Stack multiple `-L` flags for additional ports:

```bash
ssh -L 3000:localhost:3000 -L 5432:localhost:5432 coop-my-instance
```

VS Code's Remote SSH extension exposes a Ports panel that handles forwarding once connected.

## Interaction with push and pull

`coop push` and `coop pull` sync files between host and guest. Both work while an editor is connected; there is no need to disconnect.

Watch for conflicts. `coop push` overwrites guest files, and `coop pull` overwrites local files. Both commands check for uncommitted git changes and refuse to proceed unless you pass `--force`.
