# Images and Profiles

coop builds **golden images** (templates) once and copies them to create VM instances. `coop setup` builds the template. `coop start` copies it. Profiles control which development tools go into the template.

## How templates work

A template is a fully provisioned ext4 root filesystem. The build process:

1. Creates an ext4 disk image (default 8 GiB, configurable with `--template-size`)
2. Provisions a base Ubuntu system (debootstrap on Firecracker, Ubuntu 24.04 cloud image on Lima)
3. Installs base packages, Docker, GitHub CLI, Claude Code, and Codex
4. Applies requested profiles and extra packages
5. Runs post-install scripts if provided

coop stores the result under `~/.coop/images/<name>/`. On `coop start`, it copies the template to create an instance-specific rootfs. The copy uses `cp --reflink=auto` on Linux. On filesystems that support reflinks (btrfs, XFS), this shares storage blocks until written, making the copy fast and space-efficient.

## What every template includes

Every template installs these packages regardless of profile selection.

**Base packages:** `openssh-server`, `curl`, `wget`, `git`, `build-essential`, `ca-certificates`, `gnupg`, `lsb-release`, `sudo`, `iproute2`, `iptables`, `kmod`, `procps`, `jq`, `rsync`, `tmux`, `unzip`, `zip`, `file`, `less`

**Docker:** `docker-ce`, `docker-ce-cli`, `containerd.io`, `docker-compose-plugin`

**GitHub CLI:** `gh`

**Claude Code CLI:** installed via the native installer during the template build.

**Codex CLI:** installed as a standalone binary during the template build.

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
| `c` | `clang`, `llvm`, `gdb`, `valgrind`, `cmake`. Installs plugin: `clangd-lsp@claude-plugins-official` |
| `fuzz` | `clang`, `llvm`, `afl++`, `lcov` |
| `rust` | Rust toolchain via post-install script (not apt). Installs plugin: `rust-analyzer-lsp@claude-plugins-official` |
| `go` | `golang` |

Plugins listed above are baked into the golden image during `coop setup` and do not need to be listed separately in config.

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

## Extra packages

For one-off additions without a full profile, use `--extra-packages`:

```bash
coop setup --extra-packages ripgrep,fd-find,bat
```

These are installed via apt during the template build. coop tracks them in the template config and includes them in staleness detection. Changing the list triggers a rebuild.

## Post-install scripts

For provisioning beyond apt packages, pass a shell script with `--post-install`:

```bash
coop setup --profile python --post-install ./my-setup.sh
```

The script runs inside the template's chroot after all packages are installed. It has root access and network connectivity. coop hashes the script content for staleness detection; modifying the script causes the next `coop setup` to rebuild automatically.

## Named images

By default, coop builds an image called `default`. Build multiple images with different configurations using `--image`:

```bash
coop setup --profile python --image py-dev
coop setup --profile python,node,rust --image polyglot

coop start --image py-dev
coop start --image polyglot
```

Each named image lives in its own directory under `~/.coop/images/<name>/` with independent versioning and staleness tracking.

## Managing images

List all images:

```
$ coop images
default              profiles: python, node              created: 2026-03-20T14:30:00Z     size: 4.2 GiB
polyglot             profiles: python, node, rust        created: 2026-03-22T09:15:00Z     size: 6.1 GiB
```

Output includes the image name, installed profiles, creation timestamp, and disk size.

Delete a named image:

```bash
coop images --delete polyglot
```

## Template versioning and staleness

coop records what went into each template in a `template-config.json` file alongside the image. This config contains:

- **Version number**: a monotonic counter that increments when base install logic changes. A newer coop version triggers a rebuild.
- **Install script hash**: SHA-256 of the composed install recipe (base + profile + extra packages). Changing profiles or extra packages changes this hash.
- **Post-install script hash**: SHA-256 of the post-install script content, if one was provided.
- **Profile list and extra packages**: the exact inputs used to build the template.
- **Marketplaces and plugins**: the marketplace sources and plugins that were baked into the template. On `coop start`, coop compares this list against the current config and only installs the delta (marketplaces or plugins added since the template was built).
- **Creation timestamp**: when the template was built.

On every `coop setup`, coop computes the current recipe hash and compares it to the stored config. If the hashes differ, the template is stale and coop rebuilds it. A missing config file (orphaned image) also triggers a rebuild.

When you omit `--profile` and `--extra-packages`, `coop setup` reuses the values from the existing template config. Running `coop setup` with no flags only rebuilds if the underlying install logic changed.

## Rebuilding

Force a rebuild regardless of staleness:

```bash
coop setup --rebuild
coop setup --rebuild --image py-dev
```

The build is crash-safe. coop writes the new template to a staging path (`rootfs-template.ext4.new`), then swaps it into place with `mv`. If the build fails or is interrupted, the previous template stays intact.

## Instance creation from templates

`coop start` creates an instance from a template:

1. Copies the template rootfs to an instance-specific path
2. Resizes the disk if `--disk` exceeds the template size
3. Patches guest network configuration for the instance's assigned IP
4. Boots the VM

```bash
coop start
coop start --disk 50
coop start --image py-dev --disk 100
```

If `--disk` is smaller than the template size, coop logs a warning and uses the template size (shrinking is not supported). If larger, coop extends the image with `truncate` and grows the filesystem with `resize2fs`.

Each instance gets its own copy of the rootfs. Changes in one instance do not affect the template or other instances.
