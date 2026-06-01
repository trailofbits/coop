# Reading `devcontainer.json`

coop reads a **subset** of [devcontainer.json](https://containers.dev/) and maps recognised keys to its own primitives. This is not full devcontainer support — coop builds its own rootfs and does not run Dockerfiles, compose files, or arbitrary OCI feature images. The supported subset is listed below; everything else is reported as `unsupported` and skipped.

## How discovery and apply work

When you run `coop up <dir>` or `coop setup --workspace <dir>`, coop looks for `.devcontainer/devcontainer.json` in that directory (and in each mount host root, with the project directory winning ties). On restart, `coop start --workspace <dir>` can also read the project's devcontainer file for restart-time settings.

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

A non-TTY invocation that discovers a `devcontainer.json` without any of these flags errors out rather than silently choosing.

## Precedence

CLI flags > `devcontainer.json` > defaults. The reporting table marks overrides explicitly so you can see when one of your CLI flags suppressed a devcontainer value.

## Supported keys

| devcontainer.json | coop equivalent | Notes |
|---|---|---|
| `postStartCommand` | `post_start` | String or `[string,...]`; arrays are joined with ` && ` |
| `containerEnv` | `guest_env` (`--env KEY=VALUE`) | CLI `--env` wins on conflict |
| `forwardPorts` | `--forward-port` | Items may be integers or `"GUEST[:HOST]"` strings |
| `features` (`rust`, `node`, `python`, `go`, `c`, `fuzz`) | built-in `--profile` | Only at `coop setup`; ignored at `coop start` |
| `features` (anything else) | warn and skip | No silent fallback to a custom profile |
| `hostRequirements.cpus` | `--vcpus` | |
| `hostRequirements.memory` | `--mem` | Accepts `4GB`/`4GiB`-style values |
| `hostRequirements.storage` | `--disk` | Start-time only; accepts `16GB`/`16GiB`-style values |
| `mounts` | `--mount` | Docker `type=bind,source=...,target=...` strings and objects supported; non-bind types are rejected |
| `remoteUser` | `--guest-user` at setup time | Baked into the image at setup; start reports a mismatch and skips `containerEnv` if the image uses a different guest user |
| `image`, `build`, `dockerComposeFile`, `customizations`, `name` | warn and skip | coop manages its own rootfs |

## JSON with comments

`devcontainer.json` officially allows `//` and `/* */` comments and trailing commas. coop parses these correctly.

## Out of scope in v1

- OCI feature registry pulls (`ghcr.io/devcontainers/features/*` features other than the built-in name match)
- `--git-repo` auto-detection — the file lives inside the repo before coop has cloned it
- Sticky `--no-devcontainer` (persistent per-workspace state)
- Live re-application on an existing VM (destroy + `coop up` to pick up changes)
