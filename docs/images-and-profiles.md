# Images and Profiles

coop builds **golden images** (templates) once and copies them to create VM instances. `coop setup` builds the template. `coop up` copies it when creating a project instance. Profiles control which development tools go into the template.

## How templates work

A template is a fully provisioned ext4 root filesystem. The build process:

1. Creates an ext4 disk image (default 8 GiB, configurable with `--template-size`)
2. Provisions a base Ubuntu system (a downloaded Firecracker CI squashfs on Firecracker, Ubuntu 24.04 cloud image on Lima)
3. Installs base packages, Docker, GitHub CLI, and the selected coding agents
4. Applies requested profiles and, on Linux/Firecracker, extra packages
5. On Linux/Firecracker, runs a requested `--post-install` script

coop stores the result under `~/.coop/images/<name>/`. When creating an instance, coop copies the template to create an instance-specific rootfs. The copy uses `cp --reflink=auto` on Linux. On filesystems that support reflinks (btrfs, XFS), this shares storage blocks until written, making the copy fast and space-efficient.

On Linux, image builds reuse the downloaded Firecracker CI squashfs from the
private coop data directory when its source URL and stored SHA-256 checksum
match. Coop keeps one cached download, verifies it on each reuse, and downloads
it again if it is missing, changed, or fails to unpack. Each build attempts to
remove the previous working tree before unpacking. Cleanup failures are
non-fatal, so extraction can retain stale files from an earlier build.

## Choose coding agents

New images include Claude Code, Codex, and Grok Build by default. Repeat
`--agent` to build a smaller image (at least one agent is required):

```bash
coop setup --image codex-dev --agent codex
coop setup --image pair-dev --agent claude --agent codex
coop up . --image codex-dev
```

The order of repeated selectors does not matter; duplicates are ignored.
Omitting `--agent` when setting up an existing image preserves its recorded
selection, including on a rebuild. To change it, pass the new selection with
`--rebuild`, or choose a new image name. An instance keeps the agent set from
the image disk it copied, even if that image is later rebuilt or deleted. To
use a different set in an existing VM, restore it from an image with that set
(`coop restore VM --image IMAGE --reprovision` replaces its guest disk), or
create a new VM from the image. Save guest-only work before restoring.
`coop images` and `coop status` show the recorded set. Commands that launch,
update, or configure an absent agent reject it with recovery guidance. The
set describes agents installed by coop, not tools installed later by hand in
the guest. A committed image inherits its source VM's recorded set.
`--no-agents` at VM start only skips boot-time agent setup; it does not change
the image contents.

## Base packages and selected agents

Every template installs the base packages, Docker, and GitHub CLI regardless
of profile selection. The agent tools below are installed only when selected.

**Base packages:** `openssh-server`, `curl`, `wget`, `git`, `build-essential`, `ca-certificates`, `gnupg`, `lsb-release`, `sudo`, `iproute2`, `iptables`, `kmod`, `procps`, `util-linux`, `jq`, `rsync`, `unzip`, `zip`, `file`, `less`. Images containing Codex also install `dbus-user-session`, `gnome-keyring`, and `libsecret-tools` for account authentication.

**Docker:** `docker-ce`, `docker-ce-cli`, `containerd.io`, `docker-buildx-plugin`, `docker-compose-plugin`

**GitHub CLI:** `gh`

**Claude Code CLI:** installed via the native installer during the template build.

**Codex CLI:** installed with OpenAI's native installer as the guest user during
the template build. The full package, including bundled tools, stays under the
user's home directory. `~/.local/bin/codex` is the native launcher;
`/usr/local/bin/codex` is a compatibility link. The same-release Code Mode
host is exposed at `/usr/local/bin/codex-code-mode-host`; both system links
track native package updates. The guest user can run `codex update` directly
without sudo.
An image containing Codex also installs `/usr/local/bin/codex-account`, a wrapper used by
`[codex] auth = "chatgpt"` to run Codex with a D-Bus session and guest Linux
Secret Service storage. The wrapper and its three supporting packages
(`dbus-user-session`, `gnome-keyring`, `libsecret-tools`) are installed in every
image containing Codex, regardless of the `auth` setting: an image is built once and reused across
configs, so gating them would let a later `auth = "chatgpt"` edit meet an image
that cannot serve it. When that mode is not configured the wrapper simply execs
Codex, so it costs nothing at run time.

**Grok Build CLI:** installed via the official installer
(`https://x.ai/cli/install.sh`) during the template build. The binary lives at
`~/.grok/bin/grok` with a same-file `agent` link; `/usr/local/bin/grok`
points there. The image also installs a `grok-yolo`
shortcut.

The agents are installed at whatever version was current when the template was built, and that version is not part of the staleness hash — a plain `coop setup` does not refresh them. There are two ways to get newer agents:

- **A live instance:** run `codex update` or `grok update` inside the VM, or `coop agent update [--agent <agent>]...` from the host (see [`agent update`](commands.md#agent-update)). Claude Code and Grok Build also auto-update themselves in the background.
- **The golden image:** `coop setup --rebuild` rebuilds the template from a fresh base, so every new instance ships the latest agents.

## Built-in profiles

Profiles layer language-specific toolchains on top of the base install. Pass them to `--profile` during setup:

```bash
coop setup --profile python
coop setup --profile python,node,rust
```

| Profile | Packages and scripts |
|---------|---------------------|
| `python` | `python3`, `python3-pip`, `python3-venv` |
| `node` | `nodejs` (NodeSource repository added via pre-install script) |
| `c` | `clang`, `llvm`, `gdb`, `valgrind`, `cmake`. Declares Claude plugin `clangd-lsp@claude-plugins-official` (installed on Lima when Claude is in the image) |
| `fuzz` | `clang`, `llvm`, `afl++`, `lcov` |
| `rust` | Rust toolchain via post-install script (not apt). Declares Claude plugin `rust-analyzer-lsp@claude-plugins-official` (installed on Lima when Claude is in the image) |
| `go` | `golang` |

On Lima, setup installs profile plugins into images that include Claude.
Firecracker currently does not install plugins declared by selected profiles:
selecting `c` or `rust` there installs the toolchain but not its Claude plugin.
Firecracker first-boot installation reads `[claude].marketplaces` and
`[claude].plugins` from `config.toml`; list a profile plugin there explicitly
to install it. Images without Claude skip Claude plugins on either backend.

## Custom profiles

Define profiles in `config.toml` under `[profiles]`:

```toml
[profiles.ml]
apt_packages = ["python3", "python3-pip", "python3-venv"]
pre_install = "add-apt-repository -y ppa:some/repo"
post_install = "pip3 install torch numpy pandas"
marketplaces = ["https://registry.example.com/plugins"]
plugins = ["my-linter@my-marketplace"]
```

Five fields per profile:

- **`apt_packages`**: apt packages to install.
- **`pre_install`**: shell script that runs before `apt-get install`. Use this to add package repositories.
- **`post_install`**: shell script that runs after `apt-get install`. Use this for pip installs, binary downloads, or other setup.
- **`marketplaces`**: plugin marketplace sources (URLs or local paths) to register.
- **`plugins`**: plugins to install from the registered marketplaces.

Custom profiles override built-in ones. A custom profile named `python` replaces the built-in `python` profile entirely.

Use them the same way:

```bash
coop setup --profile ml
coop setup --profile ml,node
```

## Build-on-demand from `up`

For profile-only workflows, `coop up --profile <list>` builds the matching
image automatically before starting a new instance. The image name is derived
from the sorted profile list, so these commands all target the same
`node-python` image:

```bash
coop up --profile python,node
coop up --profile node,python
```

coop runs the same recipe-hash staleness check used by `coop setup`. If the
derived image is missing or stale, it is built or rebuilt first; if it is
current, startup reuses it. A new profile-derived image includes all three
agents; an existing one keeps its recorded selection. For a predictable
subset, use `coop setup --image NAME --agent AGENT` and `coop up --image NAME`.

Explicitly named images are not affected by this shorthand. To choose the
image name yourself, build it with `setup` and start a project from that image:

```bash
coop setup --image ml-dev --profile python,node
coop up . --image ml-dev
```

## Extra packages

On Linux/Firecracker, use `--extra-packages` for one-off additions without a
full profile:

```bash
coop setup --extra-packages ripgrep,fd-find,bat
```

These are installed via apt during the template build. coop tracks them in the
template config and includes them in staleness detection. Changing the list
triggers a rebuild. The Lima backend refuses `--extra-packages`.

## Post-install scripts

On Linux/Firecracker, pass a shell script with `--post-install` for
provisioning beyond apt packages:

```bash
coop setup --profile python --post-install ./my-setup.sh
```

The script runs inside the template's chroot after all packages are installed.
It has root access and network connectivity. coop hashes the script content for
staleness detection; modifying the script causes the next `coop setup` to
rebuild automatically for a current, complete template. An interrupted image
publication or an older Firecracker image without the vsock listener capability
requires an explicit `--rebuild`. The Lima backend refuses `--post-install`.

## Named images

By default, coop builds an image called `default`. Build multiple images with different configurations using `--image`:

```bash
coop setup --profile python --image py-dev
coop setup --profile python,node,rust --image polyglot

coop up ./my-project --image py-dev
coop up ./my-project --new-instance --name polyglot-vm --image polyglot
```

Each named image lives in its own directory under `~/.coop/images/<name>/` with independent versioning and staleness tracking.

## Managing images

List all images:

```
$ coop images
default              profiles: python, node              created: 2026-03-20T14:30:00Z     size: 4.2 GiB
polyglot             profiles: python, node, rust        created: 2026-03-22T09:15:00Z     size: 6.1 GiB
```

Output includes the image name, installed agents and profiles, creation timestamp, and disk size.

Delete a named image:

```bash
coop images --delete polyglot
```

## Committing an instance to an image

`coop commit` captures a stopped instance's filesystem as a new image, the inverse of the template-to-instance copy `coop up` performs. Like `docker container commit`, it saves files — not live memory.

```bash
coop stop my-project
coop commit my-project --image my-project-baseline
```

The committed image is an ordinary coop image stored under `~/.coop/images/<name>/`: it carries over the source image's `template-config.json` (profiles, guest user, hashes) with a fresh creation timestamp, so `coop images` lists it and `coop up --image <name>` launches new instances from it. The instance must be stopped first for filesystem consistency. Committing onto an existing image name requires `--force`.

`coop restore` rolls a stopped instance back to an image's filesystem in place:

```bash
coop commit my-project --image safe-point   # checkpoint
# ... a risky run trashes the environment ...
coop stop my-project
coop restore my-project --image safe-point   # back to the checkpoint
coop start my-project
```

Restore keeps the instance's name, index, IP, and workspace association — only the disk is replaced and the instance's recorded image is updated. That makes it the ergonomic choice over `destroy` + `up --image` for the destructive-undo loop, which would allocate a different instance.

The `coop start` in that recipe does not re-sync `/workspace` or reinstall plugins, which is correct for a checkpoint — the restored disk already carries both. Restoring a **base** image is different: nothing on that disk to preserve, so `start` would leave an empty `/workspace` and no plugins. Use `coop restore --reprovision` to reset an instance onto a base image; see [commands.md](commands.md#--reprovision).

## Template versioning and staleness

coop records what went into each template in a `template-config.json` file alongside the image. This config contains:

- **Version number**: metadata recording the base install logic version. Changing this number alone does not trigger a rebuild.
- **Install script hash**: SHA-256 of the composed install recipe (base + profile + Linux/Firecracker extra packages). Changing profiles or extra packages changes this hash.
- **Post-install script hash**: SHA-256 of the Linux/Firecracker post-install script content, if one was provided.
- **Agent set, profile list, and extra packages**: the exact inputs used to build the template; explicit extra packages are Linux/Firecracker only.
- **Marketplaces and plugins**: the marketplace sources and plugins that were baked into the template. On VM startup, coop compares this list against the current config and only installs the delta (marketplaces or plugins added since the template was built).
- **Creation timestamp**: when the template was built.

On Firecracker, `coop setup` compares the install recipe and post-install script hashes with the stored config. On Lima, it compares the provision-script hash, guest user, selected agents, and baked marketplace and plugin lists. A change detected by these checks rebuilds a current, complete base image; the version number is not compared. Use `--rebuild` for changes outside these checks. A missing or unreadable config file for an existing disk requires an explicit `--rebuild`, because coop cannot recover its agent set. Plain setup refuses an interrupted publication on either backend and an older Firecracker image whose metadata lacks the vsock listener capability, regardless of recipe hashes. For a base image, use `coop setup --image NAME --rebuild`; rebuilding a committed checkpoint discards its contents. An interrupted commit can instead be retried from its source instance with `coop commit --image NAME --force`.

On Linux/Firecracker, when you omit `--profile` and `--extra-packages`, `coop
setup` reuses the values from the existing template config. Running `coop setup`
with no flags only rebuilds if the underlying install logic changed.

## Rebuilding

Force a rebuild regardless of staleness:

```bash
coop setup --rebuild
coop setup --rebuild --image py-dev
```

coop builds the new disk at a staging path, then replaces the live image. A staging failure leaves a previously complete image usable. Once publication begins, an interruption leaves an incomplete-publication marker; coop refuses the image until the interrupted commit is retried or the image is explicitly rebuilt.

## Instance creation from templates

`coop up` creates an instance from a template when no project instance exists:

1. Copies the template rootfs to an instance-specific path
2. Resizes the disk if `--disk` exceeds the template size
3. Patches guest network configuration for the instance's assigned IP
4. Boots the VM

```bash
coop up .
coop up . --disk 50
coop up . --image py-dev --disk 100
```

If `--disk` is smaller than the template size, coop logs a warning and uses the template size (shrinking is not supported). If larger, coop extends the image with `truncate` and grows the filesystem with `resize2fs`.

Each instance gets its own copy of the rootfs. Changes in one instance do not affect the template or other instances.
