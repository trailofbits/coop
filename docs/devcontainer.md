# Reading `devcontainer.json`

coop reads a **subset** of [devcontainer.json](https://containers.dev/) and maps recognised keys to its own primitives. This is not full devcontainer support — coop builds its own rootfs and does not run Dockerfiles or compose files. The supported subset is listed below; everything else is reported as `unsupported` and skipped.

## How discovery and apply work

When you run `coop up <dir>` or `coop setup --workspace <dir>`, coop looks for `.devcontainer/devcontainer.json` in that directory (and in each mount host root, with the project directory winning ties). On restart, `coop start --workspace <dir>` can also read the project's devcontainer file for restart-time settings. For `coop start --git-repo <url>`, coop can discover `.devcontainer/devcontainer.json` from common GitHub repository URLs before creating the VM.

If a file is found, coop prompts:

```
Use devcontainer.json at <path>? [Y/n]
```

After your reply (or non-interactive escape hatches, below), coop prints a per-key report showing exactly which devcontainer.json keys took effect, which were overridden by CLI flags, and which are unsupported.

## Non-interactive escape hatches

For CI or scripted use, pass one of:

- `--devcontainer <path>` — use this specific file, skip the prompt
- `--no-devcontainer` — ignore any discovered file
- `--dry-run` — print the report and exit before any VM work
- `coop devcontainer check <path>` — print setup/start translation reports for a file without loading coop config or touching VM state

A non-TTY invocation that discovers a `devcontainer.json` without any of these flags errors out rather than silently choosing. For `--git-repo`, remote discovery is best-effort and currently limited to GitHub URLs that resolve to `owner/repo`; unsupported hosts continue without devcontainer translation unless you pass an explicit local `--devcontainer <path>`.

## Precedence

CLI flags > `devcontainer.json` > defaults. The reporting table marks overrides explicitly so you can see when one of your CLI flags suppressed a devcontainer value.

## Supported keys

| devcontainer.json | coop equivalent | Notes |
|---|---|---|
| `postStartCommand` | `post_start` | String or `[string,...]`; arrays are joined with ` && ` |
| `containerEnv` | `guest_env` (`--env KEY=VALUE`) | CLI `--env` wins on conflict |
| `forwardPorts` | `--forward-port` | Items may be integers or `"GUEST[:HOST]"` strings |
| `features` (`rust`, `node`, `python`, `go`, `c`, `fuzz`) | built-in `--profile` | Only at `coop setup`; ignored during VM start/restart |
| `features` (`ghcr.io/devcontainers/features/*`) | OCI Feature `install.sh` baked into the image | Public GHCR devcontainer Features only; report includes the resolved digest and script hash |
| `features` (anything else) | warn and skip | No silent fallback to a custom profile or unsupported registry |
| `hostRequirements.cpus` | `--vcpus` | |
| `hostRequirements.memory` | `--mem` | Accepts `4GB`/`4GiB`-style values |
| `hostRequirements.storage` | `--disk` | Start-time only; accepts `16GB`/`16GiB`-style values |
| `mounts` | `--mount` | Docker `type=bind,source=...,target=...` strings and objects supported; non-bind types are rejected |
| `remoteUser` | `--guest-user` at setup time | Baked into the image at setup; start reports a mismatch and skips `containerEnv` if the image uses a different guest user |
| `image`, `build`, `dockerComposeFile`, `customizations`, `name` | warn and skip | coop manages its own rootfs |

## JSON with comments

`devcontainer.json` officially allows `//` and `/* */` comments and trailing commas. coop parses these correctly.

## OCI Feature installs

For public `ghcr.io/devcontainers/features/<name>[:tag|@digest]` entries that do not map to a built-in profile, `coop setup --workspace ... --devcontainer ...` fetches the Feature metadata and layer from GHCR, validates that the layer contains `devcontainer-feature.json` and `install.sh`, and bakes the Feature payload into the image setup recipe. Features run in deterministic `features` key order after profile post-install scripts and guest-user setup, before coop's agent setup.

Feature option values must be strings, numbers, booleans, or `null`. They are exposed to the Feature script as uppercased environment variables, along with `_REMOTE_USER` and `_REMOTE_USER_HOME`.

Treat Feature install scripts as untrusted project-provided code. The report prints the resolved digest and `install.sh` SHA-256 before setup continues; use `--dry-run` or `coop devcontainer check <path> --stage setup` to inspect this without building an image.

## Out of scope in v1

- OCI feature registries other than public `ghcr.io/devcontainers/features/*`
- `--git-repo` auto-detection for non-GitHub hosts
- Sticky `--no-devcontainer` (persistent per-workspace state)
- Live re-application on an existing VM (destroy + `coop up` to pick up changes)
