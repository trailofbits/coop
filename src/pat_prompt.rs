//! `coop start` pre-flight hook: offer to scope GitHub auth to the resolved
//! repo before the VM boots.
//!
//! Decision logic lives in [`Decision::resolve`]; the side-effecting wrapper
//! [`maybe_prompt`] is what `start_instance` calls.

#![expect(
    clippy::print_stderr,
    reason = "auto-prompt is interactive CLI — stderr is user communication"
)]

use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::Path;

use anyhow::Result;

use crate::config::{CoopConfig, GitHubAuth};
use crate::github_pat::{self, SetupOpts};

/// Effective answer to "should we offer the user a PAT wizard right now?"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do — proceed with the start as-is.
    Skip,
    /// Show the prompt; the user decides.
    Prompt,
}

impl Decision {
    /// Pure decision step — no I/O. Easy to unit-test.
    ///
    /// `repo` is the resolved `owner/repo` slug, if any.
    /// `tty` reflects whether stdin is interactive.
    /// `ci` reflects whether the `CI` env var is set.
    /// `no_prompt_flag` reflects whether `--no-prompt` was passed.
    pub fn resolve(
        cfg: &CoopConfig,
        repo: Option<&str>,
        tty: bool,
        ci: bool,
        no_prompt_flag: bool,
    ) -> Self {
        // No repo → nothing to scope to.
        let Some(repo) = repo else {
            return Self::Skip;
        };
        // Hard suppressors first.
        if no_prompt_flag || ci || !tty || !cfg.setup.prompt_for_pat {
            return Self::Skip;
        }
        match cfg.github.as_ref() {
            // No mode set → defaults to "off" — eligible.
            None | Some(GitHubAuth::Off) => Self::Prompt,
            Some(GitHubAuth::Pat(pc)) => {
                // Already opted in but missing entry for this repo → prompt.
                if pc.skip.iter().any(|s| s == repo) {
                    return Self::Skip;
                }
                if pc.entries.contains_key(repo) {
                    Self::Skip
                } else {
                    Self::Prompt
                }
            }
            // User explicitly chose Auto / Env — respect that choice.
            Some(GitHubAuth::Auto | GitHubAuth::Env) => Self::Skip,
        }
    }
}

/// Wire-up: consult [`Decision::resolve`], interact with the user, run the
/// wizard if asked. Failures inside the wizard fall back to "continue
/// without GitHub auth?" — they never abort the start unilaterally.
///
/// On success this may mutate `cfg.github` in place to reflect whatever
/// the wizard / skip-marker step wrote to disk, so the caller can use
/// the same in-memory `cfg` for downstream token resolution without
/// reloading the whole file (which would erase CLI overrides).
pub fn maybe_prompt(
    cfg: &mut CoopConfig,
    config_path: &Path,
    repo: Option<&str>,
    no_prompt_flag: bool,
) -> Result<()> {
    let tty = std::io::stdin().is_terminal();
    let ci = std::env::var("CI").is_ok();
    let decision = Decision::resolve(cfg, repo, tty, ci, no_prompt_flag);
    match decision {
        Decision::Skip => {
            // Even when we skip the prompt, surface a hint to non-interactive
            // contexts so users discover the wizard exists.
            if let Some(slug) = repo
                && (!tty || ci)
                && matches!(cfg.github.as_ref(), None | Some(GitHubAuth::Off))
            {
                tracing::info!(
                    "tip: run 'coop github setup-pat --repo {slug}' to scope GitHub auth to this repo"
                );
            }
            return Ok(());
        }
        Decision::Prompt => {}
    }

    // Safe because Decision::Prompt is only returned when repo is Some.
    let Some(slug) = repo else {
        return Ok(());
    };

    eprintln!();
    eprintln!(
        "No GitHub credential is configured for {slug} (github = {:?}).",
        cfg.github.as_ref().map_or("off", GitHubAuth::mode_name),
    );
    eprintln!("Pushes and private-repo operations from the guest will fail.");
    eprintln!();
    eprint!("Set up a scoped fine-grained PAT now? [y/N/never] ");
    std::io::stderr().flush().ok();

    let mut buf = String::new();
    std::io::stdin().lock().read_line(&mut buf)?;
    match buf.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => {
            run_wizard_or_recover(cfg, config_path, slug)?;
            refresh_github_from_disk(cfg, config_path)?;
            Ok(())
        }
        "never" => {
            github_pat::add_skip_marker(config_path, slug)?;
            refresh_github_from_disk(cfg, config_path)?;
            tracing::info!("recorded skip marker for {slug}; continuing without GitHub auth");
            Ok(())
        }
        _ => {
            tracing::info!("continuing without GitHub auth for {slug}");
            Ok(())
        }
    }
}

/// Re-read only the `github` field from disk into `cfg`. CLI overrides
/// on other fields stay intact.
fn refresh_github_from_disk(cfg: &mut CoopConfig, config_path: &Path) -> Result<()> {
    let fresh = CoopConfig::load(config_path)?;
    cfg.github = fresh.github;
    Ok(())
}

fn run_wizard_or_recover(cfg: &CoopConfig, config_path: &Path, repo: &str) -> Result<()> {
    let opts = SetupOpts {
        repo: Some(repo),
        config_path,
    };
    match github_pat::run_setup_pat(cfg, &opts) {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!("PAT setup failed ({e}); falling back to unauthenticated start");
            eprint!("continue without GitHub auth? [y/N] ");
            std::io::stderr().flush().ok();
            let mut buf = String::new();
            std::io::stdin().lock().read_line(&mut buf)?;
            if matches!(buf.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PatConfig, PatEntry};

    fn cfg_with(github: Option<GitHubAuth>) -> CoopConfig {
        CoopConfig {
            github,
            ..CoopConfig::default()
        }
    }

    #[test]
    fn skips_when_no_repo() {
        let cfg = cfg_with(None);
        assert_eq!(
            Decision::resolve(&cfg, None, true, false, false),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_no_tty() {
        let cfg = cfg_with(None);
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), false, false, false),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_ci() {
        let cfg = cfg_with(None);
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, true, false),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_no_prompt_flag() {
        let cfg = cfg_with(None);
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, true),
            Decision::Skip
        );
    }

    #[test]
    fn prompts_when_mode_off_and_interactive() {
        let cfg = cfg_with(Some(GitHubAuth::Off));
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, false),
            Decision::Prompt
        );
    }

    #[test]
    fn prompts_when_pat_mode_but_no_entry() {
        let cfg = cfg_with(Some(GitHubAuth::Pat(PatConfig::default())));
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, false),
            Decision::Prompt
        );
    }

    #[test]
    fn skips_when_pat_mode_has_entry() {
        let mut pc = PatConfig::default();
        pc.entries.insert(
            "a/b".to_string(),
            PatEntry {
                token: "cmd:echo x".to_string(),
            },
        );
        let cfg = cfg_with(Some(GitHubAuth::Pat(pc)));
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, false),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_repo_marked_skip() {
        let mut pc = PatConfig::default();
        pc.skip.push("a/b".to_string());
        let cfg = cfg_with(Some(GitHubAuth::Pat(pc)));
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, false),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_mode_auto() {
        let cfg = cfg_with(Some(GitHubAuth::Auto));
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, false),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_mode_env() {
        let cfg = cfg_with(Some(GitHubAuth::Env));
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, false),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_prompt_for_pat_false() {
        let mut cfg = cfg_with(Some(GitHubAuth::Off));
        cfg.setup.prompt_for_pat = false;
        assert_eq!(
            Decision::resolve(&cfg, Some("a/b"), true, false, false),
            Decision::Skip
        );
    }
}
