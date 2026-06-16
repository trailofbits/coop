//! `coop github` — per-repo PAT setup, rotation, status, and forget.

use std::path::Path;

use anyhow::Result;

use crate::{GithubAction, config, github_pat};

pub(crate) fn cmd_github(
    cfg: &config::CoopConfig,
    config_path: &Path,
    action: GithubAction,
) -> Result<()> {
    match action {
        GithubAction::SetupPat { repo } => {
            let opts = github_pat::SetupOpts { repo, config_path };
            github_pat::run_setup_pat(cfg, &opts)
        }
        GithubAction::RotatePat { repo } => {
            let opts = github_pat::SetupOpts {
                repo: Some(repo),
                config_path,
            };
            github_pat::run_rotate_pat(cfg, &opts)
        }
        GithubAction::Status { probe } => {
            github_pat::run_status(cfg, probe);
            Ok(())
        }
        GithubAction::ForgetPat { repo } => github_pat::run_forget_pat(cfg, &repo, config_path),
    }
}
