//! VM startup pre-flight hook: offer to scope GitHub auth to the resolved repo
//! before the VM boots.
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
use crate::github_repo::RepoSlug;

/// Effective answer to "should we offer the user a PAT wizard right now?"
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing to do — proceed with the start as-is.
    Skip,
    /// Show the prompt; the user decides.
    Prompt,
}

/// Runtime context that influences whether the PAT wizard prompt is
/// offered. Grouped into a struct so call sites name each flag at the
/// keyword position rather than relying on positional booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptContext {
    /// Whether stdin is attached to an interactive terminal.
    pub is_tty: bool,
    /// Whether the `CI` env var is set (non-interactive batch run).
    pub is_ci: bool,
    /// Whether the caller passed `--no-prompt`.
    pub no_prompt_flag: bool,
}

impl Decision {
    /// Pure decision step — no I/O. Easy to unit-test.
    ///
    /// `repo` is the resolved `owner/repo` slug, if any.
    /// `ctx` carries the TTY / CI / `--no-prompt` flags.
    pub fn resolve(cfg: &CoopConfig, repo: Option<&RepoSlug>, ctx: PromptContext) -> Self {
        // No repo → nothing to scope to.
        let Some(repo) = repo else {
            return Self::Skip;
        };
        // Hard suppressors first.
        if ctx.no_prompt_flag || ctx.is_ci || !ctx.is_tty || !cfg.setup.prompt_for_pat {
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
    repo: Option<&RepoSlug>,
    no_prompt_flag: bool,
) -> Result<()> {
    let ctx = PromptContext {
        is_tty: std::io::stdin().is_terminal(),
        is_ci: std::env::var("CI").is_ok(),
        no_prompt_flag,
    };
    let decision = Decision::resolve(cfg, repo, ctx);
    match decision {
        Decision::Skip => {
            // Even when we skip the prompt, surface a hint to non-interactive
            // contexts so users discover the wizard exists.
            if let Some(slug) = repo
                && (!ctx.is_tty || ctx.is_ci)
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

fn run_wizard_or_recover(cfg: &CoopConfig, config_path: &Path, repo: &RepoSlug) -> Result<()> {
    let opts = SetupOpts {
        repo: Some(repo.as_str()),
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
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;
    use crate::config::{PatConfig, PatEntry};

    fn cfg_with(github: Option<GitHubAuth>) -> CoopConfig {
        CoopConfig {
            github,
            ..CoopConfig::default()
        }
    }

    fn slug(s: &str) -> RepoSlug {
        RepoSlug::new(s).unwrap()
    }

    /// Default interactive context: TTY on, not in CI, no `--no-prompt`.
    fn interactive() -> PromptContext {
        PromptContext {
            is_tty: true,
            is_ci: false,
            no_prompt_flag: false,
        }
    }

    #[test]
    fn skips_when_no_repo() {
        let cfg = cfg_with(None);
        assert_eq!(Decision::resolve(&cfg, None, interactive()), Decision::Skip);
    }

    #[test]
    fn skips_when_no_tty() {
        let cfg = cfg_with(None);
        let ctx = PromptContext {
            is_tty: false,
            ..interactive()
        };
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), ctx),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_ci() {
        let cfg = cfg_with(None);
        let ctx = PromptContext {
            is_ci: true,
            ..interactive()
        };
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), ctx),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_no_prompt_flag() {
        let cfg = cfg_with(None);
        let ctx = PromptContext {
            no_prompt_flag: true,
            ..interactive()
        };
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), ctx),
            Decision::Skip
        );
    }

    #[test]
    fn prompts_when_mode_off_and_interactive() {
        let cfg = cfg_with(Some(GitHubAuth::Off));
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), interactive()),
            Decision::Prompt
        );
    }

    #[test]
    fn prompts_when_pat_mode_but_no_entry() {
        let cfg = cfg_with(Some(GitHubAuth::Pat(PatConfig::default())));
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), interactive()),
            Decision::Prompt
        );
    }

    #[test]
    fn skips_when_pat_mode_has_entry() {
        let mut pc = PatConfig::default();
        pc.entries.insert(
            slug("a/b"),
            PatEntry {
                token: crate::config::Secret::new("cmd:echo x".to_string()),
            },
        );
        let cfg = cfg_with(Some(GitHubAuth::Pat(pc)));
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), interactive()),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_repo_marked_skip() {
        let mut pc = PatConfig::default();
        pc.skip.push(slug("a/b"));
        let cfg = cfg_with(Some(GitHubAuth::Pat(pc)));
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), interactive()),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_mode_auto() {
        let cfg = cfg_with(Some(GitHubAuth::Auto));
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), interactive()),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_mode_env() {
        let cfg = cfg_with(Some(GitHubAuth::Env));
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), interactive()),
            Decision::Skip
        );
    }

    #[test]
    fn skips_when_prompt_for_pat_false() {
        let mut cfg = cfg_with(Some(GitHubAuth::Off));
        cfg.setup.prompt_for_pat = false;
        assert_eq!(
            Decision::resolve(&cfg, Some(&slug("a/b")), interactive()),
            Decision::Skip
        );
    }
}
