//! Explicit host choice of the PAT entry a VM may receive.
#![expect(clippy::print_stderr, reason = "interactive CLI messages use stderr")]

use std::ffi::OsStr;
use std::io::IsTerminal as _;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config::{CoopConfig, GitHubAuth, Instance};
use crate::github_pat::{self, SetupOpts};
use crate::github_repo::RepoSlug;
use crate::workspace::{WorkspaceSource, WorkspaceState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HintSource {
    WorkspaceOrigin,
    CloneUrl,
    None,
}

#[derive(Debug, Clone)]
pub(crate) struct RepoHint {
    pub(crate) repo: Option<RepoSlug>,
    source: HintSource,
    target: String,
}

impl RepoHint {
    pub(crate) fn needed(cfg: &CoopConfig, inst: &Instance) -> bool {
        !cfg.github_disabled
            && inst.firecracker_network() != crate::config::FirecrackerNetworkMode::Isolated
            && matches!(cfg.github, None | Some(GitHubAuth::Pat(_)))
    }
    pub(crate) fn unrecorded() -> Self {
        Self {
            repo: None,
            source: HintSource::None,
            target: "<unrecorded>".to_string(),
        }
    }

    pub(crate) fn source_label(&self) -> &'static str {
        match self.source {
            HintSource::WorkspaceOrigin => "unverified_workspace_origin",
            HintSource::CloneUrl => "host_supplied_clone_url",
            HintSource::None => "none",
        }
    }

    pub(crate) fn target(&self) -> &str {
        &self.target
    }

    pub(crate) fn for_start(
        repo: Option<RepoSlug>,
        git_repo: Option<&str>,
        workspace: Option<&Path>,
    ) -> Self {
        if let Some(url) = git_repo {
            return Self {
                repo,
                source: HintSource::CloneUrl,
                target: crate::backend::safe_clone_url(url),
            };
        }
        Self {
            repo,
            source: HintSource::WorkspaceOrigin,
            target: workspace.map_or_else(
                || "<unknown>".to_string(),
                |p| format!("{:?}", p.display().to_string()),
            ),
        }
    }

    pub(crate) fn for_instance(inst: &Instance) -> Result<Self> {
        let state = WorkspaceState::try_load(inst)?;
        let Some(state) = state else {
            return Ok(Self::unrecorded());
        };
        match state.source {
            WorkspaceSource::GitRepo { url } => Ok(Self {
                repo: url.slug().cloned(),
                source: HintSource::CloneUrl,
                target: crate::backend::safe_clone_url(url.as_str()),
            }),
            WorkspaceSource::Workspace { host_path } | WorkspaceSource::Mount { host_path } => {
                Ok(Self {
                    repo: crate::github_repo::detect_workspace_repo(&host_path)
                        .ok()
                        .flatten(),
                    source: HintSource::WorkspaceOrigin,
                    target: format!("{:?}", host_path.display().to_string()),
                })
            }
        }
    }

    pub(crate) fn describe(&self) -> String {
        match self.source {
            HintSource::WorkspaceOrigin => format!(
                "Recorded host workspace: {}\nCurrent origin suggests: {} (unverified project metadata; verify this is intended)",
                self.target,
                self.repo.as_ref().map_or("none", RepoSlug::as_str)
            ),
            HintSource::CloneUrl => format!(
                "Clone target: {}\nHost-supplied URL suggests: {}",
                self.target,
                self.repo.as_ref().map_or("none", RepoSlug::as_str)
            ),
            HintSource::None => "No recorded workspace or repository suggestion.".to_string(),
        }
    }
}

pub(crate) fn unbound_guidance(cfg: &CoopConfig, inst: &Instance, hint: &RepoHint) -> String {
    use std::fmt::Write as _;
    let mut s = format!(
        "VM {} has no PAT binding; coop will not select a configured PAT.\n{}",
        inst.name,
        hint.describe()
    );
    if let Some(key) = &hint.repo {
        if cfg
            .github
            .as_ref()
            .and_then(|auth| auth.pat_entry(key))
            .is_none()
        {
            let _ = write!(
                s,
                "\nIf {key} is intended, first run: coop github setup-pat --repo {key}"
            );
        }
        let _ = write!(
            s,
            "\nIf {key} is the PAT entry you intend for this VM, run: coop github assign-pat --vm {} --repo {key}",
            inst.name
        );
    } else {
        let _ = write!(
            s,
            "\nChoose a configured entry with: coop github assign-pat --vm {} --repo OWNER/REPO",
            inst.name
        );
        if let Some(GitHubAuth::Pat(auth)) = &cfg.github {
            for key in auth.entries.keys() {
                let _ = write!(s, "\n  {key}");
            }
        }
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Skip,
    Prompt,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PromptContext {
    pub(crate) is_tty: bool,
    pub(crate) is_ci: bool,
    pub(crate) no_prompt: bool,
}

impl Decision {
    pub(crate) fn resolve(
        cfg: &CoopConfig,
        hint: &RepoHint,
        bound: bool,
        ctx: PromptContext,
    ) -> Self {
        if cfg.github_disabled
            || bound
            || hint.repo.is_none()
            || ctx.no_prompt
            || ctx.is_ci
            || !ctx.is_tty
            || !cfg.setup.prompt_for_pat
        {
            return Self::Skip;
        }
        match cfg.github.as_ref() {
            None => Self::Prompt,
            Some(GitHubAuth::Pat(auth)) => {
                if hint
                    .repo
                    .as_ref()
                    .is_some_and(|repo| auth.skip.contains(repo))
                {
                    Self::Skip
                } else {
                    Self::Prompt
                }
            }
            Some(GitHubAuth::Off | GitHubAuth::Auto | GitHubAuth::Env) => Self::Skip,
        }
    }
}

fn decision_for_instance(
    cfg: &CoopConfig,
    inst: &Instance,
    hint: &RepoHint,
    bound: bool,
    ctx: PromptContext,
) -> Result<Decision> {
    let decision = Decision::resolve(cfg, hint, bound, ctx);
    if decision == Decision::Skip {
        return Ok(Decision::Skip);
    }
    if prompt_state(inst)?.never {
        return Ok(Decision::Skip);
    }
    Ok(Decision::Prompt)
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptState {
    never: bool,
}

fn prompt_state(inst: &Instance) -> Result<PromptState> {
    let path = inst.dir.join("github_pat_prompt.json");
    crate::fs_util::read_optional_private(&path)?
        .map(|s| serde_json::from_str(&s).context("Invalid VM PAT prompt state"))
        .transpose()
        .map(Option::unwrap_or_default)
}

fn save_never(inst: &Instance) -> Result<()> {
    crate::fs_util::PrivateDir::open_existing(&inst.dir)?.write_atomic_private(
        OsStr::new("github_pat_prompt.json"),
        b"{\"never\":true}",
        0o600,
    )
}

/// Bind before the VM boots. `--pat-entry` is a non-interactive, explicit choice.
pub(crate) fn maybe_prompt(
    cfg: &mut CoopConfig,
    config_path: &Path,
    inst: &Instance,
    hint: &RepoHint,
    no_prompt: bool,
    pat_entry: Option<&RepoSlug>,
) -> Result<()> {
    if let Some(key) = pat_entry {
        if !matches!(cfg.github, Some(GitHubAuth::Pat(_))) {
            bail!("--pat-entry requires github.mode = \"pat\" and an existing PAT entry");
        }
        return crate::github_assignment::assign(cfg, inst, key.clone());
    }
    if inst.firecracker_network() == crate::config::FirecrackerNetworkMode::Isolated {
        return Ok(());
    }
    let bound = matches!(
        crate::github_assignment::select(cfg, inst)?,
        crate::github_assignment::GitHubSelection::Bound(_)
    );
    let ctx = PromptContext {
        is_tty: std::io::stdin().is_terminal(),
        is_ci: std::env::var("CI").is_ok(),
        no_prompt,
    };
    if decision_for_instance(cfg, inst, hint, bound, ctx)? == Decision::Skip {
        if matches!(cfg.github, Some(GitHubAuth::Pat(_))) && !bound {
            eprintln!("{}", unbound_guidance(cfg, inst, hint));
        }
        return Ok(());
    }
    let Some(suggested) = hint.repo.as_ref() else {
        return Ok(());
    };
    eprintln!("\nVM: {}\n{}", inst.name, hint.describe());
    eprintln!(
        "Suggested PAT entry to forward: {suggested} (entry name does not prove token scope)"
    );
    let answer = crate::prompt::read_line(
        "Bind this entry? [y/N/never/other]",
        crate::prompt::CONFIRM_MAX_BYTES,
    )?
    .unwrap_or_default();
    let selected = match answer.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => suggested.clone(),
        "never" => {
            save_never(inst)?;
            eprintln!("{}", unbound_guidance(cfg, inst, hint));
            return Ok(());
        }
        "other" => {
            let raw = crate::prompt::read_line(
                "PAT entry OWNER/REPO to forward",
                crate::prompt::CONFIRM_MAX_BYTES,
            )?
            .unwrap_or_default();
            let key = RepoSlug::parse_cli(&raw)?;
            eprintln!(
                "VM: {}\n{}\nSuggested: {suggested}\nChosen PAT entry to forward: {key}",
                inst.name,
                hint.describe()
            );
            let confirm = crate::prompt::read_line(
                "Bind this chosen entry? [y/N]",
                crate::prompt::CONFIRM_MAX_BYTES,
            )?
            .unwrap_or_default();
            if !matches!(confirm.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                return Ok(());
            }
            key
        }
        _ => {
            eprintln!("{}", unbound_guidance(cfg, inst, hint));
            return Ok(());
        }
    };
    finish_choice(cfg, config_path, inst, selected)
}

fn finish_choice(
    cfg: &mut CoopConfig,
    config_path: &Path,
    inst: &Instance,
    selected: RepoSlug,
) -> Result<()> {
    if cfg
        .github
        .as_ref()
        .and_then(|auth| auth.pat_entry(&selected))
        .is_some()
    {
        return crate::github_assignment::assign(cfg, inst, selected);
    }
    eprintln!(
        "The PAT wizard will create entry {selected} and set global github.mode = \"pat\". Other unbound VMs receive no configured PAT."
    );
    let confirm = crate::prompt::read_line(
        "Create this entry and bind it to this VM? [y/N]",
        crate::prompt::CONFIRM_MAX_BYTES,
    )?
    .unwrap_or_default();
    if !matches!(confirm.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(());
    }
    let opts = SetupOpts {
        repo: Some(selected.clone()),
        vm: Some(&inst.name),
        config_path,
    };
    github_pat::run_setup_pat(cfg, &opts)?;
    bind_after_wizard(cfg, config_path, inst, &selected)
}

fn bind_after_wizard(
    cfg: &mut CoopConfig,
    config_path: &Path,
    inst: &Instance,
    selected: &RepoSlug,
) -> Result<()> {
    cfg.github = CoopConfig::load(config_path)
        .with_context(|| format!("PAT entry {selected} was stored, but VM {} remains unbound. After repairing config, run coop github assign-pat --vm {} --repo {selected}", inst.name, inst.name))?
        .github;
    if let Err(error) = crate::github_assignment::assign(cfg, inst, selected.clone()) {
        return Err(error.context(format!("PAT entry {selected} exists, but VM {} remains unbound. Retry: coop github assign-pat --vm {} --repo {selected}", inst.name, inst.name)));
    }
    Ok(())
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "tests")]
mod tests {
    use super::*;

    #[test]
    fn explicit_off_and_noninteractive_context_skip() {
        let hint = RepoHint::for_start(
            Some(RepoSlug::new("a/b").unwrap()),
            None,
            Some(Path::new("/project")),
        );
        let ctx = PromptContext {
            is_tty: true,
            is_ci: false,
            no_prompt: false,
        };
        assert_eq!(
            Decision::resolve(
                &CoopConfig {
                    github: Some(GitHubAuth::Off),
                    ..CoopConfig::default()
                },
                &hint,
                false,
                ctx
            ),
            Decision::Skip
        );
        assert_eq!(
            Decision::resolve(
                &CoopConfig::default(),
                &hint,
                false,
                PromptContext {
                    is_tty: false,
                    ..ctx
                }
            ),
            Decision::Skip
        );
        assert_eq!(
            Decision::resolve(&CoopConfig::default(), &hint, false, ctx),
            Decision::Prompt
        );
        assert_eq!(
            Decision::resolve(&CoopConfig::default(), &hint, true, ctx),
            Decision::Skip
        );
        assert_eq!(
            Decision::resolve(
                &CoopConfig::default(),
                &hint,
                false,
                PromptContext { is_ci: true, ..ctx }
            ),
            Decision::Skip
        );
        assert_eq!(
            Decision::resolve(
                &CoopConfig::default(),
                &hint,
                false,
                PromptContext {
                    no_prompt: true,
                    ..ctx
                }
            ),
            Decision::Skip
        );
        let mut legacy = CoopConfig::default();
        let mut pat = crate::config::PatConfig::default();
        pat.skip.push(RepoSlug::new("a/b").unwrap());
        legacy.github = Some(GitHubAuth::Pat(pat));
        assert_eq!(
            Decision::resolve(&legacy, &hint, false, ctx),
            Decision::Skip
        );
    }

    #[test]
    fn headless_entry_binds_before_boot_and_never_is_per_vm() {
        let tmp = crate::test_support::private_tempdir().unwrap();
        let mut cfg: CoopConfig = toml::from_str(
            "[github]\nmode = 'pat'\n[github.pat.'a/b']\ntoken = 'github_pat_example'\n",
        )
        .unwrap();
        cfg.data_dir = crate::config::ConfigPath::new(tmp.path());
        let image = crate::config::ImageName::new("default").unwrap();
        let first = cfg
            .allocate_instance(
                Some(&crate::config::InstanceName::new("first").unwrap()),
                &image,
                None,
            )
            .unwrap();
        let second = cfg
            .allocate_instance(
                Some(&crate::config::InstanceName::new("second").unwrap()),
                &image,
                None,
            )
            .unwrap();
        save_never(&first).unwrap();
        assert!(prompt_state(&first).unwrap().never);
        assert!(!prompt_state(&second).unwrap().never);
        let hint = RepoHint::for_start(Some(RepoSlug::new("a/b").unwrap()), None, None);
        let interactive = PromptContext {
            is_tty: true,
            is_ci: false,
            no_prompt: false,
        };
        assert_eq!(
            decision_for_instance(&cfg, &first, &hint, false, interactive).unwrap(),
            Decision::Skip
        );
        assert_eq!(
            decision_for_instance(&cfg, &second, &hint, false, interactive).unwrap(),
            Decision::Prompt
        );
        let key = RepoSlug::new("a/b").unwrap();
        maybe_prompt(
            &mut cfg,
            &tmp.path().join("unused.toml"),
            &first,
            &RepoHint::unrecorded(),
            true,
            Some(&key),
        )
        .unwrap();
        assert!(matches!(
            crate::github_assignment::select(&cfg, &first).unwrap(),
            crate::github_assignment::GitHubSelection::Bound(_)
        ));
        assert!(matches!(
            crate::github_assignment::select(&cfg, &second).unwrap(),
            crate::github_assignment::GitHubSelection::Unbound
        ));
        std::fs::write(second.dir.join("github_pat_prompt.json"), "{").unwrap();
        cfg.github_disabled = true;
        maybe_prompt(
            &mut cfg,
            &tmp.path().join("unused.toml"),
            &second,
            &RepoHint::unrecorded(),
            true,
            None,
        )
        .unwrap();
    }

    #[test]
    fn completed_wizard_without_binding_reports_exact_recovery() {
        let tmp = crate::test_support::private_tempdir().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[github]\nmode = 'pat'\n[github.pat.'a/b']\ntoken = 'github_pat_example'\n",
        )
        .unwrap();
        let mut cfg = CoopConfig::load(&config_path).unwrap();
        cfg.data_dir = crate::config::ConfigPath::new(tmp.path());
        let inst = cfg
            .allocate_instance(
                Some(&crate::config::InstanceName::new("lost").unwrap()),
                &crate::config::ImageName::new("default").unwrap(),
                None,
            )
            .unwrap();
        std::fs::remove_dir_all(&inst.dir).unwrap();
        let error = bind_after_wizard(
            &mut cfg,
            &config_path,
            &inst,
            &RepoSlug::new("a/b").unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("VM lost remains unbound"));
        assert!(error.contains("coop github assign-pat --vm lost --repo a/b"));
        assert!(!inst.dir.exists());
    }
}
