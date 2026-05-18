# Issue #93 — `coop uninstall`

Add a `coop uninstall` subcommand. There is already a `coop update` (and a `coop destroy --all`), but no command that fully reverses an install: removing the binary, the data directory, and the XDG state file. Issue #93 asks for that, with a confirmation prompt before removing data directories.

## Current state (2026-05-17)

- `coop update` (`src/update.rs`) replaces the running binary with the latest release; it owns the `confirm()` helper at `src/update.rs:463`.
- `coop destroy --all` (`src/main.rs:1010-1043`) tears down every instance, removes images / kernel / firecracker binary / jailer / SSH key / instances dir, and strips all coop blocks from `~/.ssh/config`. It does **not** remove `~/.coop` itself, the `config.toml`, the XDG update-check state, or the binary.
- The data directory is `cfg.data_dir` (default `~/.coop`, see `default_data_dir` in `src/config.rs:1217`). The XDG state file lives at `${state_dir or data_local_dir}/coop/update-check.json` (see `state_path` in `src/update.rs:602`).
- `CoopConfig::load` tolerates a missing config file and returns defaults, so uninstall can run cleanly even on a half-installed system.

## CLI surface

```rust
/// Remove the coop binary and (optionally) its data directories
Uninstall {
    /// Skip interactive confirmation prompts
    #[arg(short = 'y', long)]
    yes: bool,
    /// Remove only the binary, keep ~/.coop and update-check state
    #[arg(long, conflicts_with = "purge")]
    keep_data: bool,
    /// Also remove data without prompting (pairs with --yes for CI)
    #[arg(long, conflicts_with = "keep_data")]
    purge: bool,
}
```

Behaviour matrix:

| invocation | binary | data dir | prompts |
|---|---|---|---|
| `coop uninstall` | yes (after confirm) | asked separately | 2 |
| `coop uninstall --yes` | yes | yes | 0 |
| `coop uninstall --yes --keep-data` | yes | no | 0 |
| `coop uninstall --purge` | yes (after confirm) | yes | 1 (binary only) |
| `coop uninstall --yes --purge` | yes | yes | 0 |

`--keep-data` is the safety lever; `--purge` is the "yes to everything" shortcut for scripts. Without `--yes`, the command is fully interactive — two y/N prompts (binary, then data).

## Implementation

### 1. Dispatch (`src/main.rs`)

Handle `Uninstall` *before* `load_and_validate_config` so it works even when no config exists. Load best-effort with `CoopConfig::load(&cli.config).unwrap_or_default()` — `load` already returns defaults for a missing file, and a parse error shouldn't block uninstall.

```rust
if let Commands::Uninstall { yes, keep_data, purge } = cli.command {
    let cfg = config::CoopConfig::load(&cli.config).unwrap_or_default();
    let be = backend::PlatformBackend::new();
    return cmd_uninstall(&be, &cfg, UninstallOpts { yes, keep_data, purge });
}
```

### 2. Refactor: extract `purge_all_data` from `cmd_destroy`

The body of `destroy --all` in `src/main.rs:1010-1043` becomes:

```rust
fn purge_all_data(be: &backend::PlatformBackend, cfg: &config::CoopConfig) -> Result<()> { ... }
```

`cmd_destroy(.., all=true)` and `cmd_uninstall` both call it. No behaviour change for `destroy --all`.

### 3. `cmd_uninstall` flow

1. **Survey** — count instances (`cfg.list_instances()?`), images (`cfg.list_images()?`), and whether `cfg.data_dir` exists. Print a one-shot summary so the user sees what's at stake before either prompt.
2. **Confirm binary removal** unless `--yes`. Print the full path from `env::current_exe()?`.
3. **Decide data removal**:
   - `--keep-data` → `false`
   - `--yes` (with or without `--purge`) → `true`
   - `--purge` (without `--yes`) → still ask once for the binary, then proceed with data
   - otherwise → `confirm("Also remove data directory {data_dir} (N instances, M images)?")`
4. **If removing data**: call `purge_all_data(be, cfg)`, then:
   - `fs::remove_dir_all(&cfg.data_dir)` with a `sudo rm -rf` fallback (mirrors the existing instances-dir cleanup at `src/main.rs:1031`).
   - Remove the XDG update-check state: `${state_dir}/coop/update-check.json` and its parent if empty. Add a small `pub fn remove_state()` to `src/update.rs` so the path computation stays in one place.
5. **Always** call `workspace::remove_all_ssh_config()` — even with `--keep-data`, the blocks reference IPs that won't exist.
6. **Remove the binary**: `fs::remove_file(env::current_exe()?)`. Unix keeps the open inode alive for the rest of the process. Special cases:
   - **Dev-build guard**: if the resolved path contains `/target/debug/` or `/target/release/`, *do not* remove. Warn instead — `cargo run -- uninstall` shouldn't nuke build artifacts.
   - **EPERM** (root-owned install dir): surface as `Cannot remove {path}: {err}. Try \`sudo coop uninstall\`.` No auto-sudo for the binary itself — different blast radius from the data-dir cleanup which already uses sudo for root-owned instance dirs.
7. Final `tracing::info!("coop uninstalled.")` with a one-line install.sh hint.

### 4. Share `confirm()`

Move `confirm()` out of `src/update.rs:463` into a small `src/prompt.rs` module (or fold into `src/cmd.rs`). Same signature, no behaviour change. `update.rs` and `main.rs::cmd_uninstall` both `use crate::prompt::confirm;`.

### 5. Edge cases

- **Dev build**: do *not* refuse like `coop update` does. The `target/` guard above is the only protection.
- **Custom `--config` path outside `data_dir`**: don't touch it. Print a note: "Config at {path} is outside the data directory and was not removed."
- **No instances exist / data dir already absent**: silent no-ops, not errors.
- **Lima backend**: `destroy_shared` only removes the images dir; the rest of `~/.coop` cleanup is platform-agnostic. Already correct.

## Tests

### Unit (`src/main.rs::tests`)

CLI parsing only — filesystem cleanup is integration territory:

- `uninstall_subcommand_parses`
- `uninstall_yes_flag_parses`
- `uninstall_short_y_flag_parses`
- `uninstall_keep_data_flag_parses`
- `uninstall_purge_flag_parses`
- `uninstall_keep_data_and_purge_conflict`

### Integration (`tests/integration.sh`)

Optional, off-by-default phase guarded by `--uninstall` (this nukes everything in `~/.coop` and the staged binary):

1. After the normal lifecycle, run `coop uninstall --yes --purge`.
2. Assert the binary path no longer exists.
3. Assert `~/.coop` no longer exists.
4. Assert `~/.ssh/config` no longer contains coop markers.

Document the flag's destructive nature in `CLAUDE.md` (the "## Testing" section) alongside the existing `--full` flag.

## Files touched

- `src/main.rs` — new `Commands::Uninstall` variant, `cmd_uninstall`, `purge_all_data` (extracted), `UninstallOpts` struct, parsing tests.
- `src/update.rs` — export `confirm()` via a shared module; add `remove_state()` helper.
- `src/prompt.rs` *(new)* — shared `confirm()`.
- `tests/integration.sh` — optional `--uninstall` phase.
- `CLAUDE.md` — note the new test phase.

No new dependencies.
