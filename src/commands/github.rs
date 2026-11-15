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
            let opts = github_pat::SetupOpts {
                repo,
                vm: None,
                config_path,
            };
            github_pat::run_setup_pat(cfg, &opts)
        }
        GithubAction::RotatePat { repo } => {
            let opts = github_pat::SetupOpts {
                repo: Some(repo),
                vm: None,
                config_path,
            };
            github_pat::run_rotate_pat(cfg, &opts)
        }
        GithubAction::AssignPat { vm, repo } => {
            let inst = cfg.resolve_instance(Some(&vm))?;
            crate::github_assignment::Assignment { repo }.save(cfg, &inst)
        }
        GithubAction::UnassignPat { vm } => {
            let inst = cfg.resolve_instance(Some(&vm))?;
            crate::github_assignment::Assignment::remove(&inst)
        }
        GithubAction::Status { probe, json, vm } => {
            let inst = vm
                .as_ref()
                .map(|name| cfg.resolve_instance(Some(name)))
                .transpose()?;
            github_pat::run_status(cfg, probe, json, inst.as_ref())
        }
        GithubAction::ForgetPat { repo } => github_pat::run_forget_pat(cfg, &repo, config_path),
    }
}
