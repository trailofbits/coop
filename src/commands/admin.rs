//! `coop init` / `validate` / `uninstall` — config scaffolding and teardown.

use std::io::Write as _;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::purge_all_data;
use crate::cmd::Cmd;
use crate::{backend, config, github_pat, prompt, update, workspace};

pub(crate) fn cmd_init(config_path: &Path) -> Result<()> {
    if config_path.exists() {
        bail!(
            "Config file already exists at {}. \
             Edit it directly or remove it first.",
            config_path.display()
        );
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }
    std::fs::write(config_path, include_str!("../../config.example.toml"))
        .with_context(|| format!("Failed to write {}", config_path.display()))?;
    writeln!(
        std::io::stdout(),
        "Created {}. Edit to customize, or leave as-is for defaults.",
        config_path.display()
    )
    .ok();
    Ok(())
}

pub(crate) fn cmd_validate(
    cfg: &config::CoopConfig,
    be: &backend::PlatformBackend,
    probe: bool,
) -> Result<()> {
    writeln!(std::io::stdout(), "Validating config (backend: {be})...",).ok();

    let warnings = cfg.validate()?;

    for w in &warnings {
        writeln!(std::io::stdout(), "  warning: {w}").ok();
    }

    // Per-repo PAT validation
    if let Some(config::GitHubAuth::Pat(pat)) = cfg.github.as_ref() {
        for (repo, entry) in &pat.entries {
            match config::resolve_cmd_value(entry.token.expose()) {
                Ok(token) => {
                    if token.starts_with(github_pat::TOKEN_PREFIX) {
                        writeln!(
                            std::io::stdout(),
                            "  github.pat.\"{repo}\": ok (resolves, fine-grained PAT format)"
                        )
                        .ok();
                    } else {
                        writeln!(
                            std::io::stdout(),
                            "  github.pat.\"{repo}\": warning — token resolves but is not \
                             a fine-grained PAT (no '{prefix}' prefix)",
                            prefix = github_pat::TOKEN_PREFIX,
                        )
                        .ok();
                    }
                    if probe {
                        match github_pat::probe_user_login(&token) {
                            Ok(login) => {
                                writeln!(std::io::stdout(), "    probe: /user as '{login}'").ok();
                            }
                            Err(e) => {
                                writeln!(std::io::stdout(), "    probe: FAILED ({e})").ok();
                            }
                        }
                    }
                }
                Err(e) => {
                    writeln!(
                        std::io::stdout(),
                        "  github.pat.\"{repo}\": FAILED to resolve token ({e})"
                    )
                    .ok();
                }
            }
        }
    }

    writeln!(std::io::stdout(), "Config OK").ok();
    Ok(())
}

pub(crate) struct UninstallOpts {
    pub(crate) yes: bool,
    pub(crate) keep_data: bool,
    pub(crate) purge: bool,
}

pub(crate) fn cmd_uninstall(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    config_path: &Path,
    opts: &UninstallOpts,
) -> Result<()> {
    use std::io::IsTerminal as _;

    let binary_path =
        std::env::current_exe().context("Failed to resolve current executable path for removal")?;

    // `current_exe` reads /proc/self/exe on Linux, which dereferences symlinks.
    // Removing the resolved target leaves any PATH-level symlinks dangling —
    // surface the resolved path so the user knows what's actually about to go.
    tracing::debug!("Resolved binary path: {}", binary_path.display());

    if !opts.yes && !std::io::stdin().is_terminal() {
        bail!(
            "stdin is not a TTY; pass --yes (and optionally --keep-data or --purge) \
             for non-interactive uninstall."
        );
    }

    print_uninstall_summary(cfg, &binary_path);

    if !opts.yes && !prompt::confirm(&format!("Remove coop binary at {}?", binary_path.display()))?
    {
        tracing::info!("Uninstall cancelled");
        return Ok(());
    }

    let remove_data = decide_remove_data(cfg, opts, prompt::confirm)?;

    if remove_data {
        purge_all_data(be, cfg)?;
        wipe_data_dir(&cfg.data_dir);
        if let Err(e) = update::remove_state() {
            tracing::debug!("Failed to remove update-check state (non-fatal): {e}");
        }
        if !config_path_is_under_data_dir(config_path, &cfg.data_dir) && config_path.exists() {
            tracing::info!(
                "Config at {} is outside the data directory and was not removed",
                config_path.display()
            );
        }
    } else {
        if let Err(e) = workspace::remove_all_ssh_config() {
            tracing::debug!("SSH config cleanup failed (non-fatal): {e}");
        }
        if cfg.data_dir.exists() {
            tracing::info!(
                "Keeping {}; reinstall coop to manage existing instances.",
                cfg.data_dir.display()
            );
        }
    }

    remove_self_binary(&binary_path)?;
    tracing::info!(
        "coop uninstalled. To reinstall: curl -fsSL https://raw.githubusercontent.com/trailofbits/coop/main/install.sh | sh"
    );
    Ok(())
}

fn print_uninstall_summary(cfg: &config::CoopConfig, binary_path: &Path) {
    let instance_count = cfg.list_instances().map(|v| v.len()).unwrap_or(0);
    let image_count = cfg.list_images().map(|v| v.len()).unwrap_or(0);
    tracing::info!("This will remove:");
    tracing::info!("  binary:    {}", binary_path.display());
    if cfg.data_dir.exists() {
        tracing::info!(
            "  data dir:  {} ({instance_count} instance(s), {image_count} image(s))",
            cfg.data_dir.display()
        );
    } else {
        tracing::info!("  data dir:  {} (already absent)", cfg.data_dir.display());
    }
}

/// Decide whether to wipe the data directory during uninstall.
///
/// `confirm` supplies the interactive answer when no flag forces the outcome —
/// production passes [`prompt::confirm`]; tests pass a deterministic stub so the
/// decision never depends on ambient TTY state or real stdin.
fn decide_remove_data(
    cfg: &config::CoopConfig,
    opts: &UninstallOpts,
    confirm: impl FnOnce(&str) -> Result<bool>,
) -> Result<bool> {
    if opts.keep_data {
        return Ok(false);
    }
    if opts.yes || opts.purge {
        return Ok(true);
    }
    let instance_count = cfg.list_instances().map(|v| v.len()).unwrap_or(0);
    let image_count = cfg.list_images().map(|v| v.len()).unwrap_or(0);
    confirm(&format!(
        "Also remove data directory {} ({instance_count} instance(s), {image_count} image(s))?",
        cfg.data_dir.display()
    ))
}

fn wipe_data_dir(data_dir: &Path) {
    if !data_dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(data_dir) {
        tracing::debug!("User remove_dir_all failed, trying sudo: {e}");
        if let Err(e) = Cmd::new("rm").arg("-rf").arg(data_dir).sudo().run() {
            tracing::warn!(
                "Failed to remove data dir {} (non-fatal): {e}",
                data_dir.display()
            );
        }
    }
}

/// True when `config_path` lives inside `data_dir`.
///
/// Canonicalises both sides so `./`, symlinks, and macOS `/private/var` aliases
/// don't produce false negatives. Falls back to a lexical comparison only when
/// *both* sides fail to canonicalise — mixing canonical with lexical produced
/// platform-dependent wrong answers (e.g. `/private/var/.coop` vs `/var/.coop`).
fn config_path_is_under_data_dir(config_path: &Path, data_dir: &Path) -> bool {
    match (config_path.canonicalize(), data_dir.canonicalize()) {
        (Ok(c), Ok(d)) => c.starts_with(&d),
        (Err(_), Err(_)) => config_path.starts_with(data_dir),
        // Exactly one side resolved — canonicalisation diverged from lexical
        // form, so the comparison would be apples-to-oranges. Treat as
        // "we can't tell" and skip the informational notice rather than print
        // a misleading one.
        _ => true,
    }
}

/// Resolved path is under `cargo` build output — almost certainly a dev build
/// being run via `cargo run -- uninstall`. Nuking the target artifact is
/// rarely what the developer intended.
///
/// Matches consecutive components `target/<debug|release>` so unrelated
/// directories named "target" or "release" do not trigger the guard
/// (e.g. `/opt/release/target/bin/coop` or `~/target-foo/release/coop`).
fn is_dev_target_path(path: &Path) -> bool {
    let components: Vec<_> = path.components().collect();
    components.windows(2).any(|w| {
        w[0].as_os_str() == "target"
            && matches!(w[1].as_os_str().to_str(), Some("debug" | "release"))
    })
}

fn remove_self_binary(binary_path: &Path) -> Result<()> {
    if is_dev_target_path(binary_path) {
        tracing::warn!(
            "Refusing to remove {} — looks like a cargo build artifact. \
             Run `cargo clean` if you really want to delete it.",
            binary_path.display()
        );
        return Ok(());
    }
    std::fs::remove_file(binary_path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            anyhow::anyhow!(
                "Cannot remove {}: {e}. Try `sudo coop uninstall`.",
                binary_path.display()
            )
        } else {
            anyhow::Error::from(e).context(format!(
                "Failed to remove binary at {}",
                binary_path.display()
            ))
        }
    })
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
mod tests {

    fn opts(yes: bool, keep_data: bool, purge: bool) -> super::UninstallOpts {
        super::UninstallOpts {
            yes,
            keep_data,
            purge,
        }
    }

    fn cfg_with_data_dir(dir: std::path::PathBuf) -> super::config::CoopConfig {
        super::config::CoopConfig {
            data_dir: dir,
            ..super::config::CoopConfig::default()
        }
    }

    #[test]
    fn dev_target_path_requires_consecutive_components() {
        use std::path::Path;
        // True: target immediately followed by debug/release
        assert!(super::is_dev_target_path(Path::new(
            "/home/u/repo/target/debug/coop"
        )));
        assert!(super::is_dev_target_path(Path::new(
            "/home/u/repo/target/release/coop"
        )));
        // False: real install paths
        assert!(!super::is_dev_target_path(Path::new(
            "/home/u/.local/bin/coop"
        )));
        assert!(!super::is_dev_target_path(Path::new("/usr/local/bin/coop")));
        // False: `target` and `release`/`debug` present but not adjacent
        assert!(!super::is_dev_target_path(Path::new(
            "/opt/release/target/bin/coop"
        )));
        assert!(!super::is_dev_target_path(Path::new(
            "/home/u/target-foo/release/coop"
        )));
        assert!(!super::is_dev_target_path(Path::new(
            "/srv/debug/lib/target/coop"
        )));
    }

    /// Runs `decide_remove_data` with a confirmer that records whether it was
    /// invoked, then asserts it stayed untouched — proving the flag-driven
    /// branches short-circuit before consulting the interactive prompt. Returns
    /// the decision so callers can assert on it too.
    fn decide_without_consulting_confirmer(
        cfg: &super::config::CoopConfig,
        opts: &super::UninstallOpts,
    ) -> bool {
        let consulted = std::cell::Cell::new(false);
        let decision = super::decide_remove_data(cfg, opts, |_| {
            consulted.set(true);
            Ok(true)
        })
        .unwrap();
        assert!(
            !consulted.get(),
            "confirm must not be called when a flag forces the outcome"
        );
        decision
    }

    #[test]
    fn decide_remove_data_keeps_data_when_keep_data_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(!decide_without_consulting_confirmer(
            &cfg,
            &opts(true, true, false)
        ));
        assert!(!decide_without_consulting_confirmer(
            &cfg,
            &opts(false, true, false)
        ));
    }

    #[test]
    fn decide_remove_data_removes_when_yes_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(decide_without_consulting_confirmer(
            &cfg,
            &opts(true, false, false)
        ));
    }

    #[test]
    fn decide_remove_data_removes_when_purge_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(decide_without_consulting_confirmer(
            &cfg,
            &opts(false, false, true)
        ));
        assert!(decide_without_consulting_confirmer(
            &cfg,
            &opts(true, false, true)
        ));
    }

    #[test]
    fn decide_remove_data_interactive_returns_confirmer_value() {
        // With no flag set, the decision comes straight from the injected
        // confirmer — hermetic, never touching real stdin or TTY state.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(
            !super::decide_remove_data(&cfg, &opts(false, false, false), |_| Ok(false)).unwrap()
        );
        assert!(super::decide_remove_data(&cfg, &opts(false, false, false), |_| Ok(true)).unwrap());
    }

    #[test]
    fn config_path_under_data_dir_recognises_nested() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path();
        let config = data.join("config.toml");
        std::fs::write(&config, "").unwrap();
        assert!(super::config_path_is_under_data_dir(&config, data));
    }

    #[test]
    fn config_path_under_data_dir_rejects_sibling() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        let other = tmp.path().join("elsewhere");
        std::fs::create_dir(&data).unwrap();
        std::fs::create_dir(&other).unwrap();
        let config = other.join("config.toml");
        std::fs::write(&config, "").unwrap();
        assert!(!super::config_path_is_under_data_dir(&config, &data));
    }

    #[test]
    fn config_path_under_data_dir_falls_back_lexically_when_both_missing() {
        // Neither side exists — function falls back to lexical starts_with.
        let config = std::path::Path::new("/nonexistent/data/config.toml");
        let data = std::path::Path::new("/nonexistent/data");
        assert!(super::config_path_is_under_data_dir(config, data));

        let other = std::path::Path::new("/nonexistent/other/config.toml");
        assert!(!super::config_path_is_under_data_dir(other, data));
    }

    #[test]
    fn config_path_under_data_dir_skips_notice_on_half_canonical() {
        // Data dir exists, config doesn't — historically this returned a wrong
        // answer by mixing canonical/lexical forms. The function now treats it
        // as "can't tell" and returns true to suppress the (potentially wrong)
        // informational notice.
        let tmp = tempfile::tempdir().unwrap();
        let config = std::path::Path::new("/nonexistent/path/config.toml");
        assert!(super::config_path_is_under_data_dir(config, tmp.path()));
    }
}
