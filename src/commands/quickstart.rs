//! `coop quickstart` — one-shot setup → start → claude.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::lifecycle::{allocate_and_start, find_workspace_instance};
use super::merge_runtime_guest_env;
use super::{
    DevcontainerInput, DevcontainerOpts, StartOpts, cmd_start, open_ssh_session, prepend_binary,
    resolve_devcontainer,
};
use crate::backend::VmBackend as _;
use crate::{backend, config, devcontainer, guest, prompt, setup, signal, ssh};

pub(crate) struct QuickstartOpts {
    pub(crate) no_workspace: bool,
    pub(crate) no_devcontainer: bool,
}

/// One-shot `setup → start → claude`. See `Commands::Quickstart`.
///
/// The flow short-circuits any step that's already done:
/// * skips `setup` when the default template rootfs already exists;
/// * reconnects to a running instance for the current workspace, or restarts
///   a stopped one, instead of allocating fresh.
///
/// `--no-workspace` skips workspace affinity entirely and creates a fresh
/// instance when no running workspace instance can be reused.
pub(crate) fn cmd_quickstart(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    opts: &QuickstartOpts,
) -> Result<()> {
    cfg.validate_and_warn()?;
    let image = config::default_image_name();

    if be.image_is_built(cfg, &image) {
        tracing::debug!("Image '{image}' already built — skipping setup");
    } else {
        tracing::info!("No '{image}' image found — running setup");
        let _guard = signal::install_handlers();
        be.setup(
            cfg,
            &setup::SetupOptions {
                skip_confirm: true,
                rebuild: false,
                profiles: Vec::new(),
                oci_features: Vec::new(),
                extra_packages: Vec::new(),
                post_install: None,
                image: image.clone(),
                guest_user: guest::GuestUser::default(),
                builder_timeout: None,
            },
        )?;
    }

    let workspace_dir = resolve_quickstart_workspace(opts.no_workspace)?;

    let existing = match &workspace_dir {
        Some(ws) => find_workspace_instance(cfg, ws)?,
        None => None,
    };

    let inst = match existing {
        Some(inst) if be.is_running(&inst) => {
            tracing::info!("Reusing running instance '{}'", inst.name);
            inst
        }
        Some(inst) => {
            tracing::info!("Restarting stopped instance '{}'", inst.name);
            // Use the existing instance's image, not the default — the two
            // can diverge if the instance was created with `coop up
            // --image <other>`.
            cmd_start(
                be,
                cfg,
                &StartOpts {
                    name: Some(&inst.name),
                    workspace_dir: None,
                    git_repo: None,
                    no_agents: false,
                    no_prompt: false,
                    disk: None,
                    mounts: Vec::new(),
                    exclude_git: false,
                    forward_ports: Vec::new(),
                    config_path,
                    post_start_override: None,
                    persisted_guest_env: std::collections::BTreeMap::new(),
                    devcontainer_path: None,
                    applied_devcontainer: None,
                },
            )?
        }
        None => quickstart_fresh_start(
            be,
            cfg,
            config_path,
            &image,
            workspace_dir.as_deref(),
            opts.no_devcontainer,
        )?,
    };

    let sess = open_ssh_session(be, cfg, Some(&inst.name))?;
    let claude_bin = guest::GuestUser::new(sess.target.user.as_ref())?.claude_bin();
    ssh::run_interactive(&sess, &prepend_binary(claude_bin.as_ref(), Vec::new()))
}

/// Drives a fresh start with `--workspace <ws>` defaults (no mounts, no
/// `--env`, no forwards, no `--post-start`), folding any discovered
/// `devcontainer.json` into the start. Returns the started instance.
///
/// This allocates directly rather than going through `cmd_start`; quickstart
/// creates project environments while `start` only restarts stopped instances.
// Shell-out orchestration (devcontainer resolve + allocate_and_start). exclude_re
// cannot suppress its `delete field … from struct devcontainer::TranslatorInputs`
// mutant (see the note in .cargo/mutants.toml), so skip the whole body here.
#[mutants::skip]
fn quickstart_fresh_start(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    image: &config::ImageName,
    workspace_dir: Option<&Path>,
    no_devcontainer: bool,
) -> Result<config::Instance> {
    let inputs = devcontainer::TranslatorInputs {
        cli_workspace_or_git_repo: workspace_dir.is_some(),
        ..devcontainer::TranslatorInputs::default()
    };
    let dc_input = DevcontainerInput::from_flags(None, no_devcontainer);
    let translation = resolve_devcontainer(
        &DevcontainerOpts {
            input: &dc_input,
            dry_run: false,
            workspace: workspace_dir,
            mounts: &[],
            git_repo: None,
            github_auth: cfg.github.as_ref(),
            preference_path: Some(&cfg.devcontainer_preferences_path()),
        },
        &inputs,
        devcontainer::Stage::Start,
    )?;

    if let Some(t) = &translation {
        devcontainer::apply_to_config(cfg, t)?;
    }

    // `cfg.forward_ports` is folded in by `start_instance` itself, so it
    // doesn't need to be merged in here.
    let forward_ports = translation
        .as_ref()
        .map(|t| devcontainer::merge_into_forward_ports(&t.forward_ports, &[]))
        .unwrap_or_default();

    let persisted_guest_env =
        merge_runtime_guest_env(cfg, &[], translation.as_ref().map(|t| &t.guest_env));

    let default_translation = devcontainer::Translation::default();
    let effective_disk =
        devcontainer::effective_disk(None, translation.as_ref().unwrap_or(&default_translation));
    let post_start_override = translation.as_ref().and_then(|t| t.post_start.clone());

    let final_mounts = crate::workspace::ValidatedMounts::assemble(
        crate::workspace::WorkspaceMountRule::ProjectMountedOrNone,
        translation
            .as_ref()
            .map(|t| t.mounts.clone())
            .unwrap_or_default(),
    )?
    .into_vec();

    let workspace_str = workspace_dir
        .map(|p| {
            p.to_str()
                .with_context(|| format!("Workspace path is not valid UTF-8: {}", p.display()))
        })
        .transpose()?;

    let start_opts = StartOpts {
        name: None,
        workspace_dir: workspace_str,
        git_repo: None,
        no_agents: false,
        no_prompt: false,
        disk: effective_disk,
        mounts: final_mounts,
        exclude_git: false,
        forward_ports,
        config_path,
        post_start_override: post_start_override.as_deref(),
        persisted_guest_env,
        devcontainer_path: None,
        applied_devcontainer: translation.as_ref().and_then(|t| t.applied.clone()),
    };

    allocate_and_start(be, cfg, None, image, workspace_dir, &start_opts)
}

/// Resolve the workspace directory for `coop quickstart`.
///
/// Returns `None` when `--no-workspace` is set or when the user declines a
/// `$HOME` / `/` prompt; `Some(cwd)` otherwise. Non-TTY callers in a
/// sensitive directory get an explicit bail rather than a silent mount.
fn resolve_quickstart_workspace(no_workspace: bool) -> Result<Option<PathBuf>> {
    if no_workspace {
        return Ok(None);
    }

    let cwd = std::env::current_dir().context("Failed to read current directory")?;

    let home = std::env::var_os("HOME").map(PathBuf::from);
    if is_sensitive_workspace(&cwd, home.as_deref()) {
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            bail!(
                "Current directory {} looks like your home or root — refusing to mount silently.\n\
                 Pass --no-workspace to skip the mount, or run from a project directory.",
                cwd.display(),
            );
        }
        let prompt = format!("Mount {} into the guest? This may be large.", cwd.display());
        if !prompt::confirm(&prompt)? {
            tracing::info!("Skipping workspace mount (declined at prompt)");
            return Ok(None);
        }
    }

    Ok(Some(cwd))
}

/// True when `p` is the user's `$HOME` (per the `home` argument) or the root
/// directory `/`.
///
/// `home` is passed in rather than read from the process env so the function
/// is pure and testable without env mutation. The comparison is byte-equality
/// — symlinks (e.g. macOS `/var` → `/private/var`) and trailing slashes are
/// intentionally *not* normalised, so this is a best-effort guardrail rather
/// than a hard safety check. The fallback behaviour (proceed with the cwd
/// mount) is benign for any user who deliberately runs in a normalised
/// project directory; users who land here from an unusual cwd can still
/// opt out with `--no-workspace`.
fn is_sensitive_workspace(p: &Path, home: Option<&Path>) -> bool {
    if p == Path::new("/") {
        return true;
    }
    home.is_some_and(|h| p == h)
}

#[cfg(test)]
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {

    #[test]
    fn is_sensitive_workspace_detects_root() {
        let home = std::path::Path::new("/home/alice");
        assert!(super::is_sensitive_workspace(
            std::path::Path::new("/"),
            Some(home),
        ));
    }

    #[test]
    fn is_sensitive_workspace_detects_home() {
        let home = std::path::Path::new("/home/alice");
        assert!(super::is_sensitive_workspace(home, Some(home)));
    }

    #[test]
    fn is_sensitive_workspace_passes_through_project_dir() {
        let home = std::path::Path::new("/home/alice");
        let project = std::path::Path::new("/home/alice/projects/coop");
        assert!(!super::is_sensitive_workspace(project, Some(home)));
    }

    #[test]
    fn is_sensitive_workspace_handles_missing_home() {
        // When HOME is unset, only `/` should be flagged.
        let project = std::path::Path::new("/tmp/work");
        assert!(!super::is_sensitive_workspace(project, None));
        assert!(super::is_sensitive_workspace(
            std::path::Path::new("/"),
            None,
        ));
    }

    #[test]
    fn resolve_quickstart_workspace_returns_none_when_opted_out() {
        // --no-workspace takes precedence over everything; doesn't even
        // touch the filesystem.
        let result = super::resolve_quickstart_workspace(true).expect("ok");
        assert_eq!(result, None);
    }
}
