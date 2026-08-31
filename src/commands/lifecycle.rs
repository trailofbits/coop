//! VM lifecycle commands: up / start / stop / destroy / shell / exec / status / list / resize.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::json;
use super::{
    DevcontainerInput, DevcontainerOpts, resolve_devcontainer, resolve_devcontainer_collect,
};
use super::{merge_runtime_guest_env, purge_all_data};
use crate::backend::VmBackend as _;
use crate::{
    backend, config, devcontainer, github_repo, guest, guest_env_state, model_state, pat_prompt,
    port_forward, proxy, proxy_state, setup, signal, ssh, workspace,
};

pub(crate) struct UpOpts<'a> {
    pub(crate) dir: Option<&'a str>,
    pub(crate) name: Option<&'a config::InstanceName>,
    pub(crate) transport: ProjectTransport,
    pub(crate) extra_mount: Vec<config::Mount>,
    pub(crate) git_repo: Option<&'a str>,
    pub(crate) vcpus: Option<u8>,
    pub(crate) mem: Option<config::VmMemory>,
    pub(crate) disk: Option<config::GiB>,
    /// Explicit `--image NAME`, or `None` when unset. `None` selects the
    /// profile-derived image when `--profile` is given, else the default.
    pub(crate) image: Option<config::ImageName>,
    pub(crate) profile_target: Option<ProfileImageTarget>,
    pub(crate) runtime: UpRuntimeOpts,
    pub(crate) devcontainer: UpDevcontainerOpts,
}

impl UpOpts<'_> {
    /// The image a newly-created instance should use: the profile-derived
    /// image if `--profile` was given, else the explicit `--image`, else the
    /// default image.
    fn effective_image(&self) -> config::ImageName {
        self.profile_target
            .as_ref()
            .map(|t| t.image.clone())
            .or_else(|| self.image.clone())
            .unwrap_or_else(config::default_image_name)
    }
}

pub(crate) struct UpRuntimeOpts {
    pub(crate) no_agents: bool,
    pub(crate) exclude_git: bool,
    pub(crate) no_prompt: bool,
    pub(crate) forward_ports: Vec<config::PortForward>,
    pub(crate) post_start: Option<String>,
    pub(crate) guest_env: Vec<(guest_env_state::EnvVarName, String)>,
}

pub(crate) struct UpDevcontainerOpts {
    pub(crate) input: DevcontainerInput,
    pub(crate) dry_run: bool,
    /// Emit the dry-run plan as JSON on stdout (only meaningful with `dry_run`).
    pub(crate) json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProjectTransport {
    Copy,
    Mount,
}

pub(crate) struct ProfileImageTarget {
    profiles: Vec<String>,
    image: config::ImageName,
}

impl ProfileImageTarget {
    pub(crate) fn new(profiles: &[String]) -> Result<Self> {
        let profiles = canonical_profile_list(profiles);
        let image = config::ImageName::new(&profiles.join("-")).with_context(|| {
            format!(
                "Cannot derive an image name from profile list: {}",
                profiles.join(", ")
            )
        })?;
        Ok(Self { profiles, image })
    }
}

fn canonical_profile_list(profiles: &[String]) -> Vec<String> {
    let mut names = profiles.to_vec();
    names.sort();
    names.dedup();
    names
}

/// Project-oriented start: ensure DIR has a single matching environment.
///
/// Unlike `start`, `up` treats the project directory as identity and keeps
/// transport explicit. Existing instances are found by their recorded
/// `workspace.json` host path; creation-only inputs are only applied when no
/// matching instance exists.
pub(crate) fn cmd_up(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    opts: &UpOpts<'_>,
) -> Result<()> {
    if let Some(repo_url) = opts.git_repo {
        return cmd_up_git_repo(be, cfg, config_path, opts, repo_url);
    }

    let transport = opts.transport;
    let project_dir = resolve_project_dir(opts.dir)?;

    let project_mount = config::Mount {
        host_path: project_dir.clone(),
        guest_path: workspace::default_workspace_path(),
    };
    let discovery_mounts = if transport == ProjectTransport::Mount {
        std::slice::from_ref(&project_mount)
    } else {
        &[]
    };

    if opts.devcontainer.dry_run {
        return emit_up_dry_run(
            cfg,
            opts,
            &DevcontainerOpts {
                input: &opts.devcontainer.input,
                dry_run: true,
                workspace: Some(&project_dir),
                mounts: discovery_mounts,
                git_repo: None,
                github_auth: cfg.github.as_ref(),
                preference_path: Some(&cfg.devcontainer_preferences_path()),
            },
        );
    }

    if let Some(inst) = find_workspace_instance(cfg, &project_dir)? {
        ensure_up_project_name_matches(&inst, &project_dir, opts)?;
        ensure_up_existing_inputs_are_compatible(&inst, transport, opts)?;
        if be.is_running(&inst) {
            devcontainer::warn_if_applied_devcontainer_changed(&inst);
            reject_running_up_restart_inputs(&inst, opts)?;
            tracing::info!(
                "Instance '{}' is already running for {}",
                inst.name,
                project_dir.display()
            );
            return Ok(());
        }

        let mut restart_opts = runtime_start_opts_from_up(opts, config_path);
        apply_runtime_guest_env(cfg, &opts.runtime.guest_env, None, &mut restart_opts);
        restart_instance(be, cfg, &inst, &restart_opts)?;
        return Ok(());
    }

    ensure_profile_image(be, cfg, opts.profile_target.as_ref())?;

    create_up_instance(
        be,
        cfg,
        config_path,
        opts,
        &project_dir,
        &project_mount,
        discovery_mounts,
    )
}

fn cmd_up_git_repo(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    opts: &UpOpts<'_>,
    repo_url: &str,
) -> Result<()> {
    if opts.devcontainer.dry_run {
        return emit_up_dry_run(
            cfg,
            opts,
            &DevcontainerOpts {
                input: &opts.devcontainer.input,
                dry_run: true,
                workspace: None,
                mounts: &[],
                git_repo: Some(repo_url),
                github_auth: cfg.github.as_ref(),
                preference_path: Some(&cfg.devcontainer_preferences_path()),
            },
        );
    }

    if let Some(inst) = find_git_repo_instance(cfg, repo_url)? {
        ensure_up_git_repo_name_matches(&inst, repo_url, opts)?;
        ensure_up_existing_inputs_are_compatible_for_git_repo(&inst, opts)?;
        if be.is_running(&inst) {
            reject_running_up_restart_inputs(&inst, opts)?;
            tracing::info!("Instance '{}' is already running for {repo_url}", inst.name);
            return Ok(());
        }

        let mut restart_opts = runtime_start_opts_from_up(opts, config_path);
        apply_runtime_guest_env(cfg, &opts.runtime.guest_env, None, &mut restart_opts);
        restart_instance(be, cfg, &inst, &restart_opts)?;
        return Ok(());
    }

    ensure_profile_image(be, cfg, opts.profile_target.as_ref())?;
    create_git_repo_instance(be, cfg, config_path, opts, repo_url)
}

fn up_translator_inputs(
    cfg: &config::CoopConfig,
    opts: &UpOpts<'_>,
) -> devcontainer::TranslatorInputs {
    devcontainer::TranslatorInputs {
        cli_vcpus: opts.vcpus,
        cli_mem_mib: opts.mem,
        cli_disk_gib: opts.disk,
        cli_post_start: opts.runtime.post_start.clone(),
        cli_guest_env_keys: opts
            .runtime
            .guest_env
            .iter()
            .map(|(k, _)| k.clone())
            .collect(),
        cli_forward_ports: opts.runtime.forward_ports.clone(),
        cli_mounts: opts.extra_mount.clone(),
        cli_profiles: opts
            .profile_target
            .as_ref()
            .map(|target| target.profiles.clone())
            .unwrap_or_default(),
        persisted_guest_user: Some(backend::persisted_guest_user(cfg, &opts.effective_image())),
        cli_workspace_or_git_repo: true,
        ..devcontainer::TranslatorInputs::default()
    }
}

/// Run the shared up dry-run: resolve the devcontainer for `Stage::Start`,
/// then either render the human report to stderr or emit the resolved
/// [`json::DryRunPlan`] to stdout.
fn emit_up_dry_run(
    cfg: &config::CoopConfig,
    opts: &UpOpts<'_>,
    dc_opts: &DevcontainerOpts<'_>,
) -> Result<()> {
    let inputs = up_translator_inputs(cfg, opts);
    if !opts.devcontainer.json {
        resolve_devcontainer(dc_opts, &inputs, devcontainer::Stage::Start)?;
        return Ok(());
    }
    let translation = resolve_devcontainer_collect(dc_opts, &inputs, devcontainer::Stage::Start)?;
    // Features that map to profiles are baked into the image at `coop setup`
    // time, not selected per-start, so a Start-stage translation contributes
    // no profiles; the effective set is exactly the CLI `--profile` list.
    let profiles = opts
        .profile_target
        .as_ref()
        .map_or(&[][..], |t| t.profiles.as_slice());
    let guest_user = backend::persisted_guest_user(cfg, &opts.effective_image());
    let plan = json::DryRunPlan {
        report: translation.as_ref().map(|t| &t.report),
        profiles,
        guest_user: &guest_user,
        vm: json::VmOverrides {
            vcpus: opts.vcpus,
            mem_mib: opts.mem.map(config::VmMemory::get),
            disk_gib: opts.disk,
        },
    };
    json::render_json(&plan)
}

fn create_up_instance(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    opts: &UpOpts<'_>,
    project_dir: &Path,
    project_mount: &config::Mount,
    discovery_mounts: &[config::Mount],
) -> Result<()> {
    let inputs = up_translator_inputs(cfg, opts);
    let translation = resolve_devcontainer(
        &DevcontainerOpts {
            input: &opts.devcontainer.input,
            dry_run: false,
            workspace: Some(project_dir),
            mounts: discovery_mounts,
            git_repo: None,
            github_auth: cfg.github.as_ref(),
            preference_path: Some(&cfg.devcontainer_preferences_path()),
        },
        &inputs,
        devcontainer::Stage::Start,
    )?;

    apply_vm_overrides(cfg, opts.vcpus, opts.mem, None)?;
    if let Some(t) = &translation {
        devcontainer::apply_to_config(cfg, t)?;
    }

    let mut forward_ports = opts.runtime.forward_ports.clone();
    if let Some(t) = &translation {
        forward_ports = devcontainer::merge_into_forward_ports(&t.forward_ports, &forward_ports);
    }

    let persisted_guest_env = merge_runtime_guest_env(
        cfg,
        &opts.runtime.guest_env,
        translation.as_ref().map(|t| &t.guest_env),
    );

    let default_translation = devcontainer::Translation::default();
    let effective_disk = devcontainer::effective_disk(
        opts.disk,
        translation.as_ref().unwrap_or(&default_translation),
    );
    let post_start_override = opts
        .runtime
        .post_start
        .clone()
        .or_else(|| translation.as_ref().and_then(|t| t.post_start.clone()));

    let mut mounts = translation
        .as_ref()
        .map(|t| t.mounts.clone())
        .unwrap_or_default();
    mounts.extend(opts.extra_mount.clone());

    let (workspace_dir, rule) = match opts.transport {
        ProjectTransport::Copy => (
            Some(project_dir_to_str(project_dir)?),
            workspace::WorkspaceMountRule::CopyProject,
        ),
        ProjectTransport::Mount => {
            mounts.insert(0, project_mount.clone());
            (None, workspace::WorkspaceMountRule::ProjectMountedOrNone)
        }
    };
    let mounts = workspace::ValidatedMounts::assemble(rule, mounts)?.into_vec();

    let start_opts = StartOpts {
        name: None,
        workspace_dir: workspace_dir.as_deref(),
        git_repo: None,
        no_agents: opts.runtime.no_agents,
        no_prompt: opts.runtime.no_prompt,
        disk: effective_disk,
        mounts,
        exclude_git: opts.runtime.exclude_git,
        forward_ports,
        config_path,
        post_start_override: post_start_override.as_deref(),
        persisted_guest_env,
        devcontainer_path: None,
        applied_devcontainer: translation.as_ref().and_then(|t| t.applied.clone()),
    };

    allocate_and_start(
        be,
        cfg,
        opts.name,
        &opts.effective_image(),
        Some(project_dir),
        &start_opts,
    )
    .map(|_| ())
}

fn create_git_repo_instance(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    opts: &UpOpts<'_>,
    repo_url: &str,
) -> Result<()> {
    let inputs = up_translator_inputs(cfg, opts);
    let translation = resolve_devcontainer(
        &DevcontainerOpts {
            input: &opts.devcontainer.input,
            dry_run: false,
            workspace: None,
            mounts: &[],
            git_repo: Some(repo_url),
            github_auth: cfg.github.as_ref(),
            preference_path: Some(&cfg.devcontainer_preferences_path()),
        },
        &inputs,
        devcontainer::Stage::Start,
    )?;

    apply_vm_overrides(cfg, opts.vcpus, opts.mem, None)?;
    if let Some(t) = &translation {
        devcontainer::apply_to_config(cfg, t)?;
    }

    let mut forward_ports = opts.runtime.forward_ports.clone();
    if let Some(t) = &translation {
        forward_ports = devcontainer::merge_into_forward_ports(&t.forward_ports, &forward_ports);
    }

    let persisted_guest_env = merge_runtime_guest_env(
        cfg,
        &opts.runtime.guest_env,
        translation.as_ref().map(|t| &t.guest_env),
    );

    let default_translation = devcontainer::Translation::default();
    let effective_disk = devcontainer::effective_disk(
        opts.disk,
        translation.as_ref().unwrap_or(&default_translation),
    );
    let post_start_override = opts
        .runtime
        .post_start
        .clone()
        .or_else(|| translation.as_ref().and_then(|t| t.post_start.clone()));

    let mut mounts = translation
        .as_ref()
        .map(|t| t.mounts.clone())
        .unwrap_or_default();
    mounts.extend(opts.extra_mount.clone());
    let mounts =
        workspace::ValidatedMounts::assemble(workspace::WorkspaceMountRule::GitRepoClone, mounts)?
            .into_vec();

    let start_opts = StartOpts {
        name: None,
        workspace_dir: None,
        git_repo: Some(repo_url),
        no_agents: opts.runtime.no_agents,
        no_prompt: opts.runtime.no_prompt,
        disk: effective_disk,
        mounts,
        exclude_git: opts.runtime.exclude_git,
        forward_ports,
        config_path,
        post_start_override: post_start_override.as_deref(),
        persisted_guest_env,
        devcontainer_path: None,
        applied_devcontainer: translation.as_ref().and_then(|t| t.applied.clone()),
    };

    let derived_name = opts
        .name
        .cloned()
        .or_else(|| github_repo::git_repo_default_instance_name(repo_url));
    allocate_and_start(
        be,
        cfg,
        derived_name.as_ref(),
        &opts.effective_image(),
        None,
        &start_opts,
    )
    .map(|_| ())
}

fn ensure_profile_image(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    target: Option<&ProfileImageTarget>,
) -> Result<()> {
    let Some(target) = target else {
        return Ok(());
    };
    let resolved_profiles = guest::resolve_profiles(&target.profiles, &cfg.profiles)?;
    let _guard = signal::install_handlers();
    be.setup(
        cfg,
        &setup::SetupOptions {
            skip_confirm: true,
            rebuild: false,
            profiles: resolved_profiles,
            oci_features: Vec::new(),
            extra_packages: Vec::new(),
            post_install: None,
            image: target.image.clone(),
            guest_user: guest::GuestUser::default(),
            builder_timeout: None,
        },
    )
}

fn resolve_project_dir(dir: Option<&str>) -> Result<PathBuf> {
    let path = match dir {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir().context("Failed to read current directory")?,
    };
    let canonical = path
        .canonicalize()
        .with_context(|| format!("Failed to resolve project directory {}", path.display()))?;
    anyhow::ensure!(
        canonical.is_dir(),
        "Project directory is not a directory: {}",
        canonical.display()
    );
    Ok(canonical)
}

fn project_dir_to_str(project_dir: &Path) -> Result<String> {
    project_dir
        .to_str()
        .map(ToOwned::to_owned)
        .with_context(|| format!("Project path is not valid UTF-8: {}", project_dir.display()))
}

fn runtime_start_opts_from_up<'a>(opts: &'a UpOpts<'_>, config_path: &'a Path) -> StartOpts<'a> {
    StartOpts {
        name: None,
        workspace_dir: None,
        git_repo: None,
        no_agents: opts.runtime.no_agents,
        no_prompt: opts.runtime.no_prompt,
        disk: None,
        mounts: Vec::new(),
        exclude_git: false,
        forward_ports: opts.runtime.forward_ports.clone(),
        config_path,
        post_start_override: opts.runtime.post_start.as_deref(),
        persisted_guest_env: std::collections::BTreeMap::new(),
        devcontainer_path: None,
        applied_devcontainer: None,
    }
}

pub(crate) fn apply_runtime_guest_env(
    cfg: &mut config::CoopConfig,
    cli_guest_env: &[(guest_env_state::EnvVarName, String)],
    dc_guest_env: Option<&std::collections::BTreeMap<guest_env_state::EnvVarName, String>>,
    start_opts: &mut StartOpts<'_>,
) {
    start_opts.persisted_guest_env = merge_runtime_guest_env(cfg, cli_guest_env, dc_guest_env);
}

fn ensure_up_existing_inputs_are_compatible(
    inst: &config::Instance,
    transport: ProjectTransport,
    opts: &UpOpts<'_>,
) -> Result<()> {
    if let Some(image) = &opts.image
        && inst.image != *image
    {
        bail!(
            "Instance '{}' already exists for this project using image '{}'. \
             `coop up --image {}` only applies when creating a new instance.\n\
             Use `coop destroy {}` first to recreate it with a different image.",
            inst.name,
            inst.image,
            image,
            inst.name,
        );
    }
    if let Some(target) = &opts.profile_target
        && inst.image != target.image
    {
        bail!(
            "Instance '{}' already exists for this project using image '{}'. \
             `coop up --profile {}` would use image '{}', but profiles only apply when creating a new instance.\n\
             Use `coop destroy {}` first to recreate it with those profiles.",
            inst.name,
            inst.image,
            target.profiles.join(","),
            target.image,
            inst.name,
        );
    }
    if opts.disk.is_some()
        || opts.vcpus.is_some()
        || opts.mem.is_some()
        || !opts.extra_mount.is_empty()
        || opts.runtime.exclude_git
        || matches!(opts.devcontainer.input, DevcontainerInput::Explicit(_))
    {
        bail!(
            "Instance '{}' already exists for this project. \
             --vcpus, --mem, --disk, --extra-mount, --exclude-git, and \
             --devcontainer only apply when creating a new instance.\n\
             To change memory, vCPUs, or disk on the existing instance, \
             stop it and run `coop resize`. Otherwise `coop destroy {}` \
             first to recreate it with those options.",
            inst.name,
            inst.name,
        );
    }

    let Some(state) =
        workspace::try_load_or_warn(inst, "project transport check will skip this instance")
    else {
        return Ok(());
    };
    let existing = match state.source {
        workspace::WorkspaceSource::Workspace { .. } => ProjectTransport::Copy,
        workspace::WorkspaceSource::Mount { .. } => ProjectTransport::Mount,
        workspace::WorkspaceSource::GitRepo { .. } => return Ok(()),
    };
    if existing != transport {
        let existing = match existing {
            ProjectTransport::Copy => "copy",
            ProjectTransport::Mount => "mount",
        };
        let requested = match transport {
            ProjectTransport::Copy => "copy",
            ProjectTransport::Mount => "mount",
        };
        bail!(
            "Instance '{}' already exists for this project using {existing} transport, \
             but this command requested {requested}.\n\
             Re-run with the original transport, or `coop destroy {}` first to recreate it.",
            inst.name,
            inst.name,
        );
    }
    Ok(())
}

fn ensure_up_existing_inputs_are_compatible_for_git_repo(
    inst: &config::Instance,
    opts: &UpOpts<'_>,
) -> Result<()> {
    if let Some(image) = &opts.image
        && inst.image != *image
    {
        bail!(
            "Instance '{}' already exists for this git repo using image '{}'. \
             `coop up --image {}` only applies when creating a new instance.\n\
             Use `coop destroy {}` first to recreate it with a different image.",
            inst.name,
            inst.image,
            image,
            inst.name,
        );
    }
    if let Some(target) = &opts.profile_target
        && inst.image != target.image
    {
        bail!(
            "Instance '{}' already exists for this git repo using image '{}'. \
             `coop up --profile {}` would use image '{}', but profiles only apply when creating a new instance.\n\
             Use `coop destroy {}` first to recreate it with those profiles.",
            inst.name,
            inst.image,
            target.profiles.join(","),
            target.image,
            inst.name,
        );
    }
    if opts.disk.is_some()
        || opts.vcpus.is_some()
        || opts.mem.is_some()
        || !opts.extra_mount.is_empty()
        || matches!(opts.devcontainer.input, DevcontainerInput::Explicit(_))
    {
        bail!(
            "Instance '{}' already exists for this git repo. \
             --vcpus, --mem, --disk, --extra-mount, and --devcontainer only \
             apply when creating a new instance.\n\
             To change memory, vCPUs, or disk on the existing instance, \
             stop it and run `coop resize`. Otherwise `coop destroy {}` \
             first to recreate it with those options.",
            inst.name,
            inst.name,
        );
    }
    Ok(())
}

fn ensure_up_project_name_matches(
    inst: &config::Instance,
    project_dir: &Path,
    opts: &UpOpts<'_>,
) -> Result<()> {
    if let Some(name) = opts.name {
        anyhow::ensure!(
            &inst.name == name,
            "Project {} is already associated with instance '{}', not '{}'.",
            project_dir.display(),
            inst.name,
            name,
        );
    }
    Ok(())
}

fn ensure_up_git_repo_name_matches(
    inst: &config::Instance,
    repo_url: &str,
    opts: &UpOpts<'_>,
) -> Result<()> {
    if let Some(name) = opts.name {
        anyhow::ensure!(
            &inst.name == name,
            "Git repo {repo_url} is already associated with instance '{}', not '{}'.",
            inst.name,
            name,
        );
    }
    Ok(())
}

fn up_has_restart_only_inputs(opts: &UpOpts<'_>) -> bool {
    opts.runtime.no_agents
        || !opts.runtime.forward_ports.is_empty()
        || opts.runtime.post_start.is_some()
        || !opts.runtime.guest_env.is_empty()
}

fn reject_running_up_restart_inputs(inst: &config::Instance, opts: &UpOpts<'_>) -> Result<()> {
    if up_has_restart_only_inputs(opts) {
        bail!(
            "Instance '{}' is already running for this project. \
             --no-agents, --forward-port, --post-start, and --env only take \
             effect during start or restart.\n\
             Run `coop stop {}` first, then repeat `coop up` with those options.",
            inst.name,
            inst.name,
        );
    }
    Ok(())
}

/// Find the (single) instance whose persisted workspace state's `host_path`
/// matches `workspace` (after canonicalisation). Returns `None` when no
/// instance has been started for this directory; bails when multiple do
/// (the caller has to pick one explicitly).
pub(super) fn find_workspace_instance(
    cfg: &config::CoopConfig,
    workspace: &Path,
) -> Result<Option<config::Instance>> {
    let canonical = workspace
        .canonicalize()
        .with_context(|| format!("Failed to resolve workspace path {}", workspace.display()))?;
    let instances = cfg.list_instances()?;
    let mut matching: Vec<config::Instance> = instances
        .into_iter()
        .filter(|inst| {
            workspace::try_load_or_warn(inst, "workspace-affinity matching will skip this instance")
                .and_then(|s| s.source.host_path().map(Path::to_path_buf))
                .is_some_and(|hp| hp == canonical)
        })
        .collect();
    match matching.len() {
        0 => Ok(None),
        1 => Ok(Some(matching.swap_remove(0))),
        _ => {
            let names: Vec<_> = matching.iter().map(|i| i.name.as_str()).collect();
            bail!(
                "Multiple instances share workspace {}:\n  {}\n\
                 Pick one explicitly with `coop start <name>` (for a stopped\n\
                 instance) or `coop claude <name>` (for a running one).",
                canonical.display(),
                names.join(", "),
            )
        }
    }
}

fn find_git_repo_instance(
    cfg: &config::CoopConfig,
    repo_url: &str,
) -> Result<Option<config::Instance>> {
    let instances = cfg.list_instances()?;
    let mut matching: Vec<config::Instance> = instances
        .into_iter()
        .filter(|inst| {
            workspace::try_load_or_warn(inst, "git-repo matching will skip this instance")
                .is_some_and(|s| {
                    matches!(
                        s.source,
                        workspace::WorkspaceSource::GitRepo { ref url } if url.as_str() == repo_url
                    )
                })
        })
        .collect();
    match matching.len() {
        0 => Ok(None),
        1 => Ok(Some(matching.swap_remove(0))),
        _ => {
            let names: Vec<_> = matching.iter().map(|i| i.name.as_str()).collect();
            bail!(
                "Multiple instances share git repo {repo_url}:\n  {}\n\
                 Pick one explicitly with `coop start <name>` (for a stopped\n\
                 instance) or `coop claude <name>` (for a running one).",
                names.join(", "),
            )
        }
    }
}

pub(crate) fn apply_vm_overrides(
    cfg: &mut config::CoopConfig,
    vcpus: Option<u8>,
    mem: Option<config::VmMemory>,
    template_size: Option<config::GiB>,
) -> Result<()> {
    if let Some(v) = vcpus {
        cfg.vm.vcpu_count = std::num::NonZeroU8::new(v).context("--vcpus must be > 0")?;
    }
    if let Some(m) = mem {
        cfg.vm.mem_size_mib = m;
    }
    if let Some(ts) = template_size {
        cfg.vm.template_size_gib = ts;
    }
    Ok(())
}

pub(crate) struct StartOpts<'a> {
    pub(crate) name: Option<&'a config::InstanceName>,
    pub(crate) workspace_dir: Option<&'a str>,
    pub(crate) git_repo: Option<&'a str>,
    pub(crate) no_agents: bool,
    /// Skip the interactive PAT auto-prompt unconditionally.
    pub(crate) no_prompt: bool,
    pub(crate) disk: Option<config::GiB>,
    pub(crate) mounts: Vec<config::Mount>,
    pub(crate) exclude_git: bool,
    /// Per-start forwards from `--forward-port`. Merged with
    /// `cfg.forward_ports` at start time (CLI overrides on guest-port
    /// collision).
    pub(crate) forward_ports: Vec<config::PortForward>,
    /// Path to the on-disk config file. Re-read after the auto-prompt
    /// in case the wizard added a new `[github.pat."..."]` entry.
    pub(crate) config_path: &'a Path,
    /// CLI override for `post_start` from `config.toml`. `None` means
    /// "use the configured value (if any)"; `Some` always wins.
    pub(crate) post_start_override: Option<&'a str>,
    pub(crate) persisted_guest_env: std::collections::BTreeMap<guest_env_state::EnvVarName, String>,
    /// Explicit `--devcontainer` is creation-only. `start --dry-run` handles
    /// translation before this struct is built; normal `start` only uses this
    /// marker to reject silently ignored creation options on restart.
    pub(crate) devcontainer_path: Option<&'a Path>,
    /// Devcontainer path/hash that was applied to a newly-created instance.
    /// Empty on restarts and when no devcontainer was used.
    pub(crate) applied_devcontainer: Option<devcontainer::AppliedDevcontainer>,
}

fn restart_has_ignored_creation_flags(opts: &StartOpts<'_>) -> bool {
    let workspace_was_restart_key = opts.name.is_none() && opts.workspace_dir.is_some();
    opts.devcontainer_path.is_some() || (opts.workspace_dir.is_some() && !workspace_was_restart_key)
}

fn no_stopped_instance_message(opts: &StartOpts<'_>, workspace_path: Option<&Path>) -> String {
    let mut msg = if let Some(name) = opts.name {
        format!("No stopped instance named '{name}' exists.")
    } else if let Some(path) = workspace_path {
        format!(
            "No stopped instance is associated with workspace {}.",
            path.display()
        )
    } else {
        "No stopped instances exist.".to_string()
    };

    if opts.devcontainer_path.is_some() {
        msg.push_str(
            "\n`coop start` only starts stopped instances; creation options belong to `coop up`.",
        );
    }

    if let Some(path) = workspace_path {
        msg.push_str("\nCreate or reconnect to this project with:\n  coop up ");
        msg.push_str(&path.display().to_string());
    } else {
        msg.push_str("\nCreate or reconnect to a project with:\n  coop up [DIR]");
    }
    msg.push_str("\nUse `coop list` to see existing instances.");
    msg
}

fn creation_options_rejected_message(inst: &config::Instance) -> String {
    format!(
        "Instance '{}' already exists (stopped). These creation options \
         would be silently \
         ignored on restart.\n\
         To apply new options, destroy the instance first:\n  \
         coop destroy {0}\n  coop up [DIR]",
        inst.name,
    )
}

pub(crate) fn preflight_start_target(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    opts: &StartOpts<'_>,
) -> Result<()> {
    let ws_path = opts.workspace_dir.map(Path::new);

    let Some(inst) = find_stopped_instance(be, cfg, opts.name, ws_path)? else {
        bail!("{}", no_stopped_instance_message(opts, ws_path));
    };

    if restart_has_ignored_creation_flags(opts) {
        bail!("{}", creation_options_rejected_message(&inst));
    }

    Ok(())
}

pub(crate) fn cmd_start(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    opts: &StartOpts<'_>,
) -> Result<config::Instance> {
    let ws_path = opts.workspace_dir.map(Path::new);

    let Some(inst) = find_stopped_instance(be, cfg, opts.name, ws_path)? else {
        bail!("{}", no_stopped_instance_message(opts, ws_path));
    };

    let has_ignored_flags = restart_has_ignored_creation_flags(opts);
    if has_ignored_flags {
        bail!("{}", creation_options_rejected_message(&inst));
    }

    restart_instance(be, cfg, &inst, opts)?;
    Ok(inst)
}

/// Allocate a fresh instance and run the first-boot start, cleaning up any
/// partial state on error. Used by project-oriented flows that create
/// instances (`coop up` and `coop quickstart`).
pub(super) fn allocate_and_start(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    name: Option<&config::InstanceName>,
    image: &config::ImageName,
    workspace_path: Option<&Path>,
    opts: &StartOpts<'_>,
) -> Result<config::Instance> {
    let inst = cfg.allocate_instance(name, image, workspace_path)?;
    tracing::info!("Starting instance '{}' (index {})", inst.name, inst.index);

    let _guard = signal::install_handlers();
    let result = start_instance(be, &mut *cfg, &inst, opts);

    if let Err(e) = &result {
        tracing::error!("Failed to start instance '{}': {e}", inst.name);
        if let Ok(target) = be.ssh_target(cfg, &inst) {
            port_forward::teardown_ssh_forwards(&inst, &target);
        }
        crate::proxy::stop(&inst);
        if let Err(cleanup_err) = be.destroy_instance(cfg, &inst) {
            tracing::debug!("Cleanup failed (non-fatal): {cleanup_err}");
        }
        if let Err(ssh_err) = workspace::remove_ssh_config(&inst) {
            tracing::debug!("SSH config cleanup failed (non-fatal): {ssh_err}");
        }
    }

    result.map(|()| inst)
}

/// Find a stopped instance to restart, if applicable.
///
/// With a name: returns the instance if it exists and is stopped,
/// errors if it's running, returns None if it doesn't exist.
///
/// With a workspace path (no name): looks up instances by their stored
/// workspace `host_path`. If a match is found and running, errors with
/// a helpful message. If stopped, returns it for restart. If no match,
/// returns None so the caller can report that `start` is restart-only.
///
/// With neither: returns the single stopped instance if exactly one
/// exists, errors if multiple stopped instances exist, returns None
/// if none exist.
fn find_stopped_instance(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    workspace_dir: Option<&Path>,
) -> Result<Option<config::Instance>> {
    let mut instances = cfg.list_instances()?;

    if let Some(name) = name {
        let Some(inst) = instances.into_iter().find(|i| &i.name == name) else {
            return Ok(None);
        };
        if be.is_running(&inst) {
            bail!(
                "Instance '{name}' is already running.\n\
                 Use `coop shell {name}` to connect, or \
                 `coop stop {name}` first."
            );
        }
        return Ok(Some(inst));
    }

    // Workspace affinity: find instance by stored workspace path
    if let Some(ws_dir) = workspace_dir {
        let canonical = ws_dir
            .canonicalize()
            .with_context(|| format!("Failed to resolve workspace path {}", ws_dir.display()))?;

        let matching: Vec<usize> = instances
            .iter()
            .enumerate()
            .filter(|(_, inst)| {
                workspace::try_load_or_warn(
                    inst,
                    "workspace-affinity matching will skip this instance",
                )
                .and_then(|s| s.source.host_path().map(Path::to_path_buf))
                .is_some_and(|hp| hp == canonical)
            })
            .map(|(i, _)| i)
            .collect();

        match matching.len() {
            0 => {} // No match — fall through to allocate new instance
            1 => {
                let idx = matching[0];
                if be.is_running(&instances[idx]) {
                    let name = &instances[idx].name;
                    bail!(
                        "Instance '{name}' is already running with this workspace.\n\
                         Use `coop shell {name}` to connect."
                    );
                }
                return Ok(Some(instances.swap_remove(idx)));
            }
            _ => {
                let names: Vec<_> = matching
                    .iter()
                    .map(|&i| instances[i].name.as_str())
                    .collect();
                bail!(
                    "Multiple instances share workspace {}:\n  {}\n\
                     Specify which to restart: coop start <name>",
                    canonical.display(),
                    names.join(", "),
                );
            }
        }

        // No existing instance for this workspace — return None to allocate new
        return Ok(None);
    }

    let stopped: Vec<_> = instances
        .into_iter()
        .filter(|i| !be.is_running(i))
        .collect();

    match stopped.len() {
        0 => Ok(None),
        1 => Ok(stopped.into_iter().next()),
        _ => {
            let names: Vec<_> = stopped.iter().map(|i| i.name.as_str()).collect();
            bail!(
                "Multiple stopped instances exist: {}\n\
                 Specify which to restart: coop start <name>",
                names.join(", "),
            );
        }
    }
}

/// Restart a stopped instance: boot VM, wait for SSH, re-bootstrap guest tools.
fn restart_instance(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    inst: &config::Instance,
    opts: &StartOpts<'_>,
) -> Result<()> {
    tracing::info!("Restarting stopped instance '{}'", inst.name);
    devcontainer::warn_if_applied_devcontainer_changed(inst);

    let _guard = signal::install_handlers();

    // Pre-flight: same auto-prompt as a fresh start. Uses the instance's
    // recorded workspace-state to recover the repo slug.
    let repo = backend::detect_instance_repo(inst);
    pat_prompt::maybe_prompt(cfg, opts.config_path, repo.as_ref(), opts.no_prompt)?;

    // Re-apply the forward set the instance was last started with.
    // CLI `--forward-port` on a restart appends/overrides; otherwise the
    // saved set carries forward untouched.
    let saved = port_forward::ForwardsState::try_load(inst)?
        .map(|s| s.forwards)
        .unwrap_or_default();
    let forwards = config::merge_forward_ports(&saved, &opts.forward_ports);
    port_forward::check_host_port_collisions(&forwards)?;

    // Re-apply the persisted guest-env set from the initial start
    // (CLI `--env` ∪ devcontainer `containerEnv`). New start-time
    // entries on restart override per-key; the merged result is what
    // gets persisted (and forwarded for this restart's bootstrap).
    let saved_guest_env = guest_env_state::GuestEnvState::try_load(inst)?
        .map(|s| s.entries)
        .unwrap_or_default();
    let mut merged_guest_env = saved_guest_env;
    for (key, value) in &opts.persisted_guest_env {
        merged_guest_env.insert(key.clone(), value.clone());
    }
    for (key, value) in &merged_guest_env {
        cfg.guest_env.insert(key.clone(), value.clone());
    }

    be.start_existing(cfg, inst)?;

    signal::check_shutdown()?;

    let target = be.ssh_target(cfg, inst)?;
    target
        .wait_until_ready(std::time::Duration::from_secs(30))
        .context("Guest booted but SSH is not accepting connections")?;

    signal::check_shutdown()?;

    // Keep an already-installed `coop-<name>` alias current. On Lima the
    // forwarded port changes across stop/start; this is a no-op rewrite on
    // Firecracker. Never installs a block the user didn't ask for.
    if let Err(e) = workspace::refresh_ssh_config_if_present(&target, inst) {
        tracing::warn!("Failed to refresh SSH config for '{}': {e}", inst.name);
    }

    port_forward::ForwardsState {
        forwards: forwards.clone(),
    }
    .save(inst)?;
    port_forward::spawn_ssh_forwards(inst, &target, &forwards)?;

    guest_env_state::GuestEnvState {
        entries: merged_guest_env,
    }
    .save(inst)?;

    bootstrap_and_post_start(
        be,
        cfg,
        inst,
        &target,
        repo.as_ref(),
        opts,
        backend::BootMode::Restart,
    )?;

    tracing::info!(
        "Instance '{}' restarted — SSH: {}:{}",
        inst.name,
        target.host,
        target.port,
    );
    Ok(())
}

/// Best-effort GitHub repo resolution for `coop start`.
///
/// Order: `--git-repo` URL → `--workspace` `.git/config` origin → first
/// `--mount` host path's `.git/config` origin → `None`.
///
/// The mount fallback matches the "first mount is the workspace"
/// convention used by `push`/`pull`, so the slug a user sees here is the
/// same one `detect_instance_repo` will recover on `coop shell` / `exec`
/// / `restart`.
fn resolve_start_repo(opts: &StartOpts<'_>) -> Result<Option<github_repo::RepoSlug>> {
    if let Some(url) = opts.git_repo
        && let Some(slug) = github_repo::parse_repo_slug_from_url(url)
    {
        return Ok(Some(slug));
    }
    if let Some(ws_dir) = opts.workspace_dir {
        let path = Path::new(ws_dir);
        if path.is_dir()
            && let Some(slug) = github_repo::detect_workspace_repo(path)?
        {
            return Ok(Some(slug));
        }
    }
    if let Some(m) = opts.mounts.first()
        && let Some(slug) = github_repo::detect_workspace_repo(&m.host_path)?
    {
        return Ok(Some(slug));
    }
    Ok(None)
}

fn start_instance(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    inst: &config::Instance,
    opts: &StartOpts<'_>,
) -> Result<()> {
    // Derive the GitHub repo slug as early as possible so the auto-prompt
    // can fire before any VM cost is incurred, and so pat-mode token
    // forwarding works at bootstrap time.
    let repo = resolve_start_repo(opts)?;
    pat_prompt::maybe_prompt(cfg, opts.config_path, repo.as_ref(), opts.no_prompt)?;

    // Forwards are checked up-front so an in-use host port fails fast,
    // before any VM cost is incurred. The actual `-L` tunnels are
    // established after SSH is ready (below).
    let forwards = config::merge_forward_ports(&cfg.forward_ports, &opts.forward_ports);
    port_forward::check_host_port_collisions(&forwards)?;

    be.create_and_start(cfg, inst, opts.disk, &opts.mounts)?;

    signal::check_shutdown()?;

    let target = be.ssh_target(cfg, inst)?;
    target
        .wait_until_ready(std::time::Duration::from_secs(30))
        .context("Guest booted but SSH is not accepting connections")?;

    signal::check_shutdown()?;

    port_forward::ForwardsState {
        forwards: forwards.clone(),
    }
    .save(inst)?;
    port_forward::spawn_ssh_forwards(inst, &target, &forwards)?;

    // Persist start-time guest-env entries (CLI `--env` ∪ devcontainer
    // `containerEnv`) so later commands targeting this instance — which
    // reload `config.toml` from scratch and do not re-parse
    // `--devcontainer` — still forward these values via SSH `SendEnv`.
    // The in-memory `cfg.guest_env` already contains them for this
    // process's bootstrap pass.
    guest_env_state::GuestEnvState {
        entries: opts.persisted_guest_env.clone(),
    }
    .save(inst)?;
    if let Some(applied) = &opts.applied_devcontainer {
        devcontainer::DevcontainerState {
            applied: applied.clone(),
        }
        .save(inst)?;
    }

    bootstrap_and_post_start(
        be,
        cfg,
        inst,
        &target,
        repo.as_ref(),
        opts,
        backend::BootMode::FirstBoot,
    )?;

    signal::check_shutdown()?;

    // Workspace sync: tar-pipe for --workspace, git clone for --git-repo.
    // Mounts may be additional data directories or the workspace source
    // itself. Only mount-only instances record the first mount as the
    // workspace identity.
    let workspace_state = if let Some(ws_dir) = opts.workspace_dir {
        let ws_path = std::path::Path::new(ws_dir);
        anyhow::ensure!(
            ws_path.is_dir(),
            "Workspace path {ws_dir} is not a directory"
        );

        let abs_path = ws_path
            .canonicalize()
            .with_context(|| format!("Failed to resolve {ws_dir}"))?;

        workspace::tar_pipe_transfer(&target, &abs_path, opts.exclude_git)?;

        let state = workspace::WorkspaceState {
            guest_path: workspace::default_workspace_path(),
            source: workspace::WorkspaceSource::Workspace {
                host_path: abs_path,
            },
        };
        state.save(inst)?;
        Some(state)
    } else if let Some(repo_url) = opts.git_repo {
        backend::clone_git_repo(&target, cfg.github.as_ref(), repo_url)?;

        let state = workspace::WorkspaceState {
            guest_path: workspace::default_workspace_path(),
            source: workspace::WorkspaceSource::GitRepo {
                url: github_repo::GitRepoUrl::new(repo_url),
            },
        };
        state.save(inst)?;
        Some(state)
    } else {
        None
    };

    if !opts.mounts.is_empty() {
        if be.mounts_are_live() {
            // Lima: virtiofs already serves the host directory live. No
            // sync step, but we still record state so `push`/`pull` and
            // PAT slug detection work for follow-up commands.
            if workspace_state.is_none() {
                workspace::record_mount_state(inst, &opts.mounts)?;
            }
            warn_on_live_git_mounts(&opts.mounts);
        } else {
            if workspace_state.is_some() {
                workspace::sync_mount_contents(&target, &opts.mounts, opts.exclude_git)?;
            } else {
                workspace::sync_mounts(&target, inst, &opts.mounts, opts.exclude_git)?;
            }
            tracing::warn!(
                "Firecracker mounts use one-time sync, not live filesystem sharing. \
                 Use `coop push` / `coop pull` to sync changes."
            );
        }
    }

    tracing::info!(
        "Instance '{}' started — SSH: {}:{}",
        inst.name,
        target.host,
        target.port,
    );
    Ok(())
}

/// Emit a warning for each live-mounted host directory that contains a
/// `.git` entry. Git operations inside the guest (worktree creation,
/// `prek install`, etc.) may write absolute `/workspace`-prefixed paths
/// into `.git/config`, which the host then sees and chokes on. See
/// issue #102.
fn warn_on_live_git_mounts(mounts: &[config::Mount]) {
    for m in mounts.iter().filter(|m| m.host_is_git_repo()) {
        tracing::warn!(
            "Live-mounting git repo '{}' at guest path '{}'. Avoid running \
             commands inside the guest that record absolute paths in \
             .git/config (in particular `git worktree add` and \
             `prek install`): they write '{}/...' values for \
             core.worktree / core.hooksPath that are visible on the host \
             through the live mount and break host-side `git` after the \
             VM exits.",
            m.host_path.display(),
            m.guest_path,
            m.guest_path,
        );
    }
}

/// Resolve an instance that must be running.
///
/// With a name: finds that instance and verifies it's running.
/// Without a name: auto-selects if exactly one instance is running.
/// Provides contextual error messages for every failure mode.
pub(crate) fn resolve_running(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
) -> Result<backend::RunningInstance> {
    let instances = cfg.list_instances()?;

    if let Some(name) = name {
        let inst = instances
            .into_iter()
            .find(|i| &i.name == name)
            .with_context(|| {
                format!(
                    "No instance named '{name}'.\n\
                     Create one with: coop up . --name {name}"
                )
            })?;
        let Some(running) = be.as_running(cfg, inst)? else {
            bail!(
                "Instance '{name}' is not running.\n\
                 Start it with: coop start {name}"
            );
        };
        return Ok(running);
    }

    let (running, stopped): (Vec<_>, Vec<_>) =
        instances.into_iter().partition(|i| be.is_running(i));

    match running.len() {
        1 => {
            let inst = running
                .into_iter()
                .next()
                .context("Instance list unexpectedly empty")?;
            be.as_running(cfg, inst)?
                .context("Instance stopped unexpectedly while resolving")
        }
        0 if stopped.len() == 1 => {
            let name = &stopped[0].name;
            bail!(
                "Instance '{name}' exists but is stopped.\n\
                 Start it with: coop start {name}"
            )
        }
        0 if stopped.is_empty() => {
            bail!(
                "No instances found.\n\
                 Create one with: coop up\n\
                 (Run `coop setup` first if you haven't \
                 built an image yet.)"
            )
        }
        0 => {
            let names: Vec<_> = stopped.iter().map(|i| i.name.as_str()).collect();
            bail!(
                "No running instances. Stopped: {}\n\
                 Start one with: coop start <name>",
                names.join(", "),
            )
        }
        _ => {
            let names: Vec<_> = running.iter().map(|i| i.name.as_str()).collect();
            bail!(
                "Multiple running instances. Specify one: {}",
                names.join(", "),
            )
        }
    }
}

pub(crate) fn cmd_shell(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    command: &[String],
) -> Result<()> {
    let session = open_ssh_session(be, cfg, name)?;
    if command.is_empty() {
        ssh::run_interactive(&session, &[])
    } else {
        ssh::run_command(&session, command)
    }
}

/// Build a remote command by prepending `binary` to `args`.
pub(crate) fn prepend_binary(binary: &str, args: Vec<String>) -> Vec<String> {
    let mut command = Vec::with_capacity(1 + args.len());
    command.push(binary.to_string());
    command.extend(args);
    command
}

/// Codex's flag for running fully unrestricted (no sandbox, no approvals).
const CODEX_BYPASS_FLAG: &str = "--dangerously-bypass-approvals-and-sandbox";

/// Codex subcommands that manage credentials rather than start an agent
/// session. They reject the sandbox-bypass flag, so it must not be prepended.
const CODEX_AUTH_SUBCOMMANDS: &[&str] = &["login", "logout"];

/// Prepend Codex's sandbox-bypass flag unless the user opted into approvals.
///
/// The VM is the isolation boundary, so Codex's own sandbox is redundant — and
/// broken in the guest, which lacks a working bubblewrap. Bypassing by default
/// gives `coop codex` parity with `coop claude`'s `bypassPermissions`. `ask`
/// (from `--ask`) keeps Codex's sandbox and approval prompts.
///
/// `codex login` / `codex logout` never start a session, and Codex rejects the
/// bypass flag on them, so they are launched bare regardless of `ask` — this is
/// what makes `coop codex -- login --device-auth` work without `--ask`.
pub(crate) fn codex_launch_args(ask: bool, mut args: Vec<String>) -> Vec<String> {
    let is_auth_subcommand = args
        .first()
        .is_some_and(|arg| CODEX_AUTH_SUBCOMMANDS.contains(&arg.as_str()));
    if !ask && !is_auth_subcommand {
        args.insert(0, CODEX_BYPASS_FLAG.to_string());
    }
    args
}

pub(crate) fn cmd_exec(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    command: &[String],
) -> Result<()> {
    let session = open_ssh_session(be, cfg, name)?;
    ssh::exec_command(&session, command)
}

/// Resolve a running instance and prepare an SSH session with env
/// forwarding in one step. Returns an owned session ready to pass to
/// `ssh::*` and `backend::*` helpers. Callers that also need the
/// `Instance` (e.g. for workspace push/pull) should call
/// `resolve_running` directly.
pub(crate) fn open_ssh_session(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
) -> Result<backend::SshSession> {
    let running = resolve_running(be, cfg, name)?;
    let repo = backend::detect_instance_repo(running.instance());
    let (inst, target) = running.into_parts();
    prepare_session_from_target(cfg, Some(&inst), target, repo.as_ref())
}

/// Build an `SshSession` from an already-resolved target.
///
/// Symmetric with `open_ssh_session`, for paths that resolve the
/// target without going through `resolve_running` — namely the
/// post-boot bootstrap in fresh start and restart, where the
/// instance isn't yet registered as running.
///
/// When `inst` is `Some`, any persisted `--env` snapshot for that
/// instance is overlaid onto the resolved env-forward set so values
/// passed at `coop start --env KEY=VAL` survive across the
/// per-invocation config reload. Bootstrap callers inside fresh
/// `start_instance` pass `None` because the in-memory `cfg.guest_env`
/// is already authoritative for that one process; restart and every
/// post-start command pass `Some` because the on-disk snapshot is
/// the only place the original `--env` set still lives.
/// Open a session and run the post-boot agent bootstrap plus any
/// `postStartCommand`, honoring `--no-agents`. Shared by fresh start and
/// restart, which differ only in the [`backend::BootMode`].
fn bootstrap_and_post_start(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
    target: &backend::SshTarget,
    repo: Option<&github_repo::RepoSlug>,
    opts: &StartOpts<'_>,
    mode: backend::BootMode,
) -> Result<()> {
    let post_start = opts.post_start_override.or(cfg.post_start.as_deref());
    let proxy_configured =
        proxy_state::effective_upstream(inst, proxy::Provider::Anthropic, &cfg.proxy)?.is_some()
            || proxy_state::effective_upstream(inst, proxy::Provider::Openai, &cfg.proxy)?
                .is_some();
    if opts.no_agents && proxy_configured {
        // Proxy mode suppresses the raw API keys on every session, but the
        // proxy + guest base-URL override are only set up during agent
        // bootstrap — which --no-agents skips. Warn so the loud auth failure
        // isn't a mystery (contradictory config: proxy is about agent creds).
        tracing::warn!(
            "proxy mode is configured but --no-agents skips agent bootstrap; \
             agents will not be able to authenticate in this VM"
        );
    }
    if opts.no_agents && post_start.is_none() {
        tracing::info!("Skipping guest agent bootstrap (--no-agents)");
        return Ok(());
    }
    // Pass `Some(inst)` so proxy-mode key suppression (and the guest-env /
    // Codex-local overlays) apply to the bootstrap session too — otherwise the
    // raw ANTHROPIC_API_KEY would be forwarded via SendEnv during bootstrap,
    // defeating proxy-mode non-exposure (issue #411).
    let session = prepare_session_from_target(cfg, Some(inst), target.clone(), repo)?;
    if opts.no_agents {
        tracing::info!("Skipping guest agent bootstrap (--no-agents)");
    } else {
        let guest_host = be.guest_host_address(&cfg.network);
        backend::bootstrap_agents(&session, cfg, inst, mode, &guest_host)?;
    }
    if let Some(cmd) = post_start {
        // Agent bootstrap may have just minted the per-instance capability
        // token (proxy mode), which is forwarded to sessions via `SendEnv`
        // (Codex's `COOP_LOCAL_API_KEY`). The session above was built before
        // the token existed, so re-prepare it here — otherwise a `post_start`
        // that runs Codex in proxy mode would lack the token and fail to
        // authenticate. Under --no-agents no proxy started, so nothing new to
        // pick up; keep the original session.
        let session = if opts.no_agents {
            session
        } else {
            prepare_session_from_target(cfg, Some(inst), target.clone(), repo)?
        };
        backend::run_post_start(&session, cmd);
    }
    Ok(())
}

pub(crate) fn prepare_session_from_target(
    cfg: &config::CoopConfig,
    inst: Option<&config::Instance>,
    target: backend::SshTarget,
    repo: Option<&github_repo::RepoSlug>,
) -> Result<backend::SshSession> {
    // Load the per-instance model selection once: it decides both the
    // proxy-mode key suppression and the Codex local-key forwarding below.
    let model = match inst {
        Some(inst) => Some(model_state::ModelState::load_or_default(inst)?),
        None => None,
    };

    // In proxy mode (issue #411: an effective upstream — `[proxy.<provider>]`
    // default or a per-VM override — and the VM in remote model mode) the raw
    // key must never be forwarded into the guest; the host-side proxy holds it
    // and the guest gets only the capability token. Overrides live in the
    // instance state, so this is resolved per provider and needs `inst`.
    let remote = model
        .as_ref()
        .is_some_and(|m| m.mode == model_state::ModelMode::Remote);
    let (proxy_anthropic, proxy_openai) = match inst {
        Some(inst) if remote => (
            proxy_state::effective_upstream(inst, proxy::Provider::Anthropic, &cfg.proxy)?
                .is_some(),
            proxy_state::effective_upstream(inst, proxy::Provider::Openai, &cfg.proxy)?.is_some(),
        ),
        _ => (false, false),
    };
    let codex_account_auth = cfg.codex.auth.uses_chatgpt_account();
    let suppress_openai_key = proxy_openai || codex_account_auth;
    // A per-VM proxy override can pair an OpenAI upstream with ChatGPT account
    // auth even though `CoopConfig::validate` rejects that combination in the
    // config file. Only Codex is unusable then, so warn here and let the
    // Codex entry points (`coop codex`, agent bootstrap) fail hard — a shell,
    // exec, or `coop claude` session on the same VM is unaffected.
    if codex_account_auth && proxy_openai {
        tracing::warn!("{}", backend::codex_chatgpt_proxy_conflict_message());
    }

    let mut env = backend::prepare_env_forwarding(cfg, repo, proxy_anthropic, suppress_openai_key)?;
    if let Some(inst) = inst {
        if let Some(state) = guest_env_state::GuestEnvState::try_load(inst)? {
            for (name, value) in &state.entries {
                // In proxy mode a raw provider key snapshotted via `coop start
                // --env` must not re-enter the guest — the host-side proxy
                // holds it. Skip and warn, matching `prepare_env_forwarding`.
                if (proxy_anthropic && name.as_str() == "ANTHROPIC_API_KEY")
                    || (suppress_openai_key && name.as_str() == "OPENAI_API_KEY")
                {
                    let reason = if name.as_str() == "OPENAI_API_KEY" && codex_account_auth {
                        "codex.auth = \"chatgpt\""
                    } else {
                        "proxy mode"
                    };
                    tracing::warn!("{reason}: ignoring runtime --env entry '{}'", name.as_str());
                    continue;
                }
                env.set(name.as_str(), value.as_str());
            }
        }
        // Codex reads its provider key from the env var named by `env_key`. In
        // local-model mode that is the local endpoint's token; in remote proxy
        // mode it is the per-instance capability token (the raw key stays on
        // the host). Claude's token rides in settings.json instead. The two
        // modes are mutually exclusive (mode is Local or Remote).
        if let Some(model) = &model {
            if model.mode == model_state::ModelMode::Local
                && let Some(ep) = model.resolved_codex(&cfg.codex)
            {
                env.set(model_state::CODEX_LOCAL_ENV_KEY, ep.auth_token_or_default());
            } else if proxy_openai
                && let Some(token) = proxy::read_capability_token(inst, proxy::Provider::Openai)
            {
                env.set(model_state::CODEX_LOCAL_ENV_KEY, token);
            }
        }
    }
    Ok(backend::SshSession { target, env })
}

pub(crate) fn cmd_stop(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
) -> Result<()> {
    tracing::info!("Stopping instance '{}'", inst.name);
    // Probe live state once. The `RunningInstance` proof flows into
    // `be.stop`, so the type system witnesses that we only ask the
    // backend to stop something that was actually running.
    if let Ok(Some(running)) = be.as_running(cfg, inst.clone()) {
        // Tear down forwards before shutting down the VM so the
        // control master can exit cleanly while SSH is still
        // reachable.
        port_forward::teardown_ssh_forwards(running.instance(), running.target());
        be.stop(cfg, running)?;
    } else {
        tracing::debug!("Instance '{}' is not running — nothing to stop", inst.name);
        // Stale forwards may still exist even when the VM is gone.
        if let Ok(target) = be.ssh_target(cfg, inst) {
            port_forward::teardown_ssh_forwards(inst, &target);
        }
    }
    // Tear down the credential proxy (issue #411) — best-effort, no-op when
    // proxy mode was never on.
    crate::proxy::stop(inst);
    // The `coop-<name>` SSH alias is left in place across stop: a stale
    // entry has no effect while the VM is down, and `coop start` refreshes
    // it (the Lima port changes per boot). `destroy`/`ssh-config --clean`
    // remove it.
    tracing::info!("Instance '{}' stopped", inst.name);
    Ok(())
}

pub(crate) fn cmd_destroy(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    all: bool,
) -> Result<()> {
    if all {
        purge_all_data(be, cfg)?;
        tracing::info!("All resources cleaned up");
    } else {
        let inst = cfg.resolve_instance(name)?;
        tracing::info!("Destroying instance '{}'", inst.name);
        if let Ok(target) = be.ssh_target(cfg, &inst) {
            port_forward::teardown_ssh_forwards(&inst, &target);
        }
        crate::proxy::stop(&inst);
        be.destroy_instance(cfg, &inst)?;
        workspace::remove_ssh_config(&inst)?;
        tracing::info!("Instance '{}' destroyed", inst.name);
    }

    Ok(())
}

pub(crate) fn cmd_list(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    json_out: bool,
) -> Result<()> {
    let mut instances = cfg.list_instances()?;
    instances.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
    let summaries: Vec<json::InstanceSummary<'_>> = instances
        .iter()
        .map(|inst| json::InstanceSummary {
            name: &inst.name,
            state: json::InstanceState::from_running(be.is_running(inst)),
        })
        .collect();

    if json_out {
        return json::render_json(&summaries);
    }

    if summaries.is_empty() {
        writeln!(std::io::stdout(), "No instances found")
            .map_err(|e| anyhow::anyhow!("Failed to write list: {e}"))?;
        return Ok(());
    }
    writeln!(std::io::stdout(), "{:<16} STATE", "NAME")
        .map_err(|e| anyhow::anyhow!("Failed to write list: {e}"))?;
    for summary in &summaries {
        writeln!(
            std::io::stdout(),
            "{:<16} {}",
            summary.name.as_str(),
            summary.state.label()
        )
        .map_err(|e| anyhow::anyhow!("Failed to write list: {e}"))?;
    }
    Ok(())
}

/// Assemble the common JSON status shape for one instance. Runs the same
/// state/usage queries as the human path, so the shared fields agree.
fn instance_status<'a>(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &'a config::Instance,
) -> Result<json::InstanceStatus<'a>> {
    let (state, usage) = match be.as_running(cfg, inst.clone())? {
        Some(running) => (
            json::InstanceState::Running,
            backend::query_resource_usage(running.target()),
        ),
        None => (json::InstanceState::Stopped, None),
    };
    Ok(json::InstanceStatus {
        name: &inst.name,
        state,
        image: &inst.image,
        backend: json::BackendKind::of(be),
        usage,
    })
}

pub(crate) fn cmd_status(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    json_out: bool,
) -> Result<()> {
    if let Some(name) = name {
        let inst = cfg.resolve_instance(Some(name))?;
        if json_out {
            return json::render_json(&instance_status(be, cfg, &inst)?);
        }
        let report = match be.as_running(cfg, inst.clone())? {
            Some(running) => be.status(cfg, &running)?,
            None => format!(
                "Instance '{}' (stopped)\n  Backend: {be}\n  Image: {}",
                inst.name, inst.image,
            ),
        };
        writeln!(std::io::stdout(), "{report}")
            .map_err(|e| anyhow::anyhow!("Failed to write status: {e}"))?;
    } else {
        let instances = cfg.list_instances()?;
        if json_out {
            let statuses = instances
                .iter()
                .map(|inst| instance_status(be, cfg, inst))
                .collect::<Result<Vec<_>>>()?;
            return json::render_json(&statuses);
        }
        if instances.is_empty() {
            writeln!(std::io::stdout(), "No instances found")
                .map_err(|e| anyhow::anyhow!("Failed to write status: {e}"))?;
            return Ok(());
        }
        for inst in &instances {
            let (state, usage_str) = match be.as_running(cfg, inst.clone())? {
                Some(running) => {
                    let usage = backend::query_resource_usage(running.target())
                        .map(|u| format!("  {}", u.summary()))
                        .unwrap_or_default();
                    ("running", usage)
                }
                None => ("stopped", String::new()),
            };
            writeln!(
                std::io::stdout(),
                "{:<16} {:<10} {:<10} {}{usage_str}",
                inst.name,
                state,
                inst.image,
                be,
            )
            .map_err(|e| anyhow::anyhow!("Failed to write status: {e}"))?;
        }
    }
    Ok(())
}

/// Inputs to `coop resize`. At least one of `disk`/`mem`/`vcpus` is set;
/// that invariant is enforced at the CLI boundary by the `resize_targets`
/// arg group, so this is a plain data bundle (kept together to stay under
/// the positional-parameter limit, like [`UpOpts`]/[`StartOpts`]).
pub(crate) struct ResizeOpts<'a> {
    pub(crate) name: Option<&'a config::InstanceName>,
    pub(crate) disk: Option<config::DiskSize>,
    pub(crate) mem: Option<config::VmMemory>,
    pub(crate) vcpus: Option<std::num::NonZeroU8>,
    pub(crate) start: bool,
}

pub(crate) fn cmd_resize(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    opts: &ResizeOpts<'_>,
) -> Result<()> {
    // A below-minimum `--mem` is rejected at the CLI boundary by
    // `VmMemory::parse_cli`, so `opts.mem` is already provably bootable
    // here; no half-applied instance can result from a bad value. (The CLI
    // ArgGroup guarantees at least one of size/mem/vcpus is present.)
    let inst = cfg.resolve_instance(opts.name)?;
    let stopped = be.as_stopped(inst)?;

    if let Some(disk_size) = opts.disk {
        let current = current_disk_gib(be, stopped.instance())?;
        let new_size = disk_size.resolve(current)?;
        be.resize_disk(cfg, &stopped, new_size)?;
    }

    if opts.mem.is_some() || opts.vcpus.is_some() {
        // The backend applies mem/vcpu and, for `--start`, boots the
        // instance itself (Lima must boot to validate regardless).
        be.set_machine_resources(cfg, &stopped, opts.mem, opts.vcpus, opts.start)?;
    } else if opts.start {
        be.start_existing(cfg, stopped.instance())?;
    }

    Ok(())
}

/// Save a stopped instance's filesystem as a reusable image.
///
/// The inverse of `coop up --image`: the committed image is an ordinary
/// coop image (`coop images` lists it, `coop up --image` relaunches it).
/// The instance must be stopped for filesystem consistency; overwriting
/// an existing image name requires `force`.
pub(crate) fn cmd_commit(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    image: &config::ImageName,
    force: bool,
) -> Result<()> {
    let inst = cfg.resolve_instance(name)?;
    let source_image = inst.image.clone();

    // Refuse to clobber an existing image unless asked. Checked before
    // gating on stopped state so a typo'd or already-taken name fails fast.
    if be.image_is_built(cfg, image) && !force {
        bail!("Image '{image}' already exists. Pass --force to overwrite it.");
    }

    let stopped = be.as_stopped(inst)?;

    // Carry the source image's template config over to the new image with a
    // fresh creation timestamp. This is the backend-agnostic half of the
    // image (guest_user, profiles, hashes) that `coop up` reads back. Load it
    // *before* writing any disk artifact so a missing or unreadable source
    // config fails fast, leaving no half-written image behind.
    let mut template_config =
        setup::TemplateConfig::load_for(cfg, &source_image).with_context(|| {
            format!("Failed to load template config for source image '{source_image}'")
        })?;

    be.commit_disk(cfg, &stopped, image)?;

    template_config.created = setup::utc_timestamp();
    template_config.save_for(cfg, image)?;

    tracing::info!(
        "Committed instance '{}' to image '{image}'. \
         Relaunch with `coop up --image {image}`.",
        stopped.instance().name,
    );
    Ok(())
}

/// Roll a stopped instance back to image `image`'s filesystem in place.
///
/// The instance keeps its name, index, IP, and workspace association —
/// only the disk is replaced. Its recorded origin image is updated so the
/// guest-user lookup and status lineage track the restored image. Run
/// `coop start` afterwards to bring the instance back up.
pub(crate) fn cmd_restore(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    image: &config::ImageName,
) -> Result<()> {
    let inst = cfg.resolve_instance(name)?;

    if !be.image_is_built(cfg, image) {
        bail!("No image '{image}' found. Run `coop images` to list available images.");
    }

    let stopped = be.as_stopped(inst)?;
    be.restore_disk(cfg, &stopped, image)?;

    // Persist the new lineage after the disk swap: if `restore_disk` fails,
    // the recorded image still matches the (untouched) disk. The only
    // residual window is a failed `instance.json` write after a successful
    // swap, which re-running `restore` corrects.
    let mut restored = stopped.instance().clone();
    restored.set_image(image.clone())?;

    tracing::info!(
        "Restored instance '{name}' from image '{image}'. \
         Run `coop start {name}` to bring it back up.",
        name = restored.name,
    );
    Ok(())
}

fn current_disk_gib(be: &backend::PlatformBackend, inst: &config::Instance) -> Result<config::GiB> {
    let path = be.disk_path(inst)?;
    let bytes = std::fs::metadata(&path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .len();
    config::GiB::new(bytes_to_gib(bytes))
        .with_context(|| format!("Disk at {} is smaller than 1 GiB", path.display()))
}

/// Whole gibibytes in `bytes`, rounding down (1 GiB = 1024³ bytes).
///
/// The truncating `as u32` is safe for any disk size coop creates: the
/// template caps well under 4 TiB (`u32::MAX` GiB), so the quotient fits.
#[expect(clippy::cast_possible_truncation, reason = "disk GiB fits in u32")]
fn bytes_to_gib(bytes: u64) -> u32 {
    (bytes / (1024 * 1024 * 1024)) as u32
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {

    fn cfg_with_data_dir(dir: std::path::PathBuf) -> super::config::CoopConfig {
        super::config::CoopConfig {
            data_dir: super::config::ConfigPath::new(dir),
            ..super::config::CoopConfig::default()
        }
    }

    fn run_git(repo: &std::path::Path, args: &[&str]) {
        // Clear inherited env and restore only what `git` needs. This
        // protects against parent contexts that export GIT_DIR /
        // GIT_WORK_TREE / GIT_INDEX_FILE (e.g. a pre-commit hook
        // running `cargo test`), which would otherwise hijack the
        // tempdir-scoped operations below.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git command runs");
        assert!(status.success(), "git {args:?} failed");
    }

    fn start_opts(
        mounts: Vec<super::config::Mount>,
        config_path: &std::path::Path,
    ) -> super::StartOpts<'_> {
        super::StartOpts {
            name: None,
            workspace_dir: None,
            git_repo: None,
            no_agents: false,
            no_prompt: true,
            disk: None,
            mounts,
            exclude_git: false,
            forward_ports: Vec::new(),
            config_path,
            post_start_override: None,
            persisted_guest_env: std::collections::BTreeMap::new(),
            devcontainer_path: None,
            applied_devcontainer: None,
        }
    }

    fn up_opts_for_tests(dir: Option<&str>) -> super::UpOpts<'_> {
        super::UpOpts {
            dir,
            name: None,
            transport: super::ProjectTransport::Copy,
            extra_mount: Vec::new(),
            git_repo: None,
            vcpus: None,
            mem: None,
            disk: None,
            image: None,
            profile_target: None,
            runtime: super::UpRuntimeOpts {
                no_agents: false,
                exclude_git: false,
                no_prompt: true,
                forward_ports: Vec::new(),
                post_start: None,
                guest_env: Vec::new(),
            },
            devcontainer: super::UpDevcontainerOpts {
                input: super::DevcontainerInput::Disabled,
                dry_run: false,
                json: false,
            },
        }
    }

    fn write_workspace_state(inst: &super::config::Instance, host_path: &std::path::Path) {
        let state = super::workspace::WorkspaceState {
            guest_path: super::workspace::default_workspace_path(),
            source: super::workspace::WorkspaceSource::Workspace {
                host_path: host_path.to_path_buf(),
            },
        };
        state.save(inst).expect("save workspace state");
    }

    #[test]
    fn prepend_binary_prefixes_command() {
        let cmd = super::prepend_binary("/usr/bin/claude", vec!["--model".into(), "opus".into()]);
        assert_eq!(cmd, vec!["/usr/bin/claude", "--model", "opus"]);
    }

    #[test]
    fn prepend_binary_with_no_args() {
        let cmd = super::prepend_binary("/usr/bin/codex", Vec::new());
        assert_eq!(cmd, vec!["/usr/bin/codex"]);
    }

    #[test]
    fn codex_launch_args_bypasses_sandbox_by_default() {
        let args = super::codex_launch_args(false, vec!["--model".into(), "gpt-5".into()]);
        assert_eq!(
            args,
            vec![
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "gpt-5"
            ]
        );
    }

    #[test]
    fn codex_launch_args_leaves_auth_subcommands_bare() {
        // `coop codex -- login --device-auth` is the ChatGPT account sign-in
        // path; Codex rejects the bypass flag on it, so `--ask` must not be
        // required to get a working invocation.
        for subcommand in ["login", "logout"] {
            let args =
                super::codex_launch_args(false, vec![subcommand.into(), "--device-auth".into()]);
            assert_eq!(args, vec![subcommand, "--device-auth"]);
        }
    }

    #[test]
    fn codex_launch_args_still_bypasses_when_login_is_not_first() {
        // Only the leading token selects a subcommand — a `login` appearing
        // as a value must not disarm the bypass flag.
        let args = super::codex_launch_args(false, vec!["--model".into(), "login".into()]);
        assert_eq!(
            args,
            vec![
                "--dangerously-bypass-approvals-and-sandbox",
                "--model",
                "login"
            ]
        );
    }

    #[test]
    fn codex_launch_args_with_ask_keeps_sandbox() {
        let args = super::codex_launch_args(true, vec!["--model".into(), "gpt-5".into()]);
        assert_eq!(args, vec!["--model", "gpt-5"]);
    }

    #[test]
    fn codex_launch_args_bypass_flag_leads_empty_args() {
        let args = super::codex_launch_args(false, Vec::new());
        assert_eq!(args, vec!["--dangerously-bypass-approvals-and-sandbox"]);
    }

    #[test]
    fn profile_image_target_sorts_and_deduplicates_profiles() {
        let profiles = vec![
            "python".to_string(),
            "node".to_string(),
            "python".to_string(),
        ];
        let target = super::ProfileImageTarget::new(&profiles).expect("image target");
        assert_eq!(target.image.as_str(), "node-python");
        assert_eq!(target.profiles, vec!["node", "python"]);
    }

    /// `prepare_session_from_target` must overlay the on-disk
    /// `GuestEnvState` for the running instance on top of values
    /// resolved from `cfg.guest_env` and forwarded host env vars.
    /// This is the regression test for issue #131: `coop start --env`
    /// values must survive across a fresh config reload by later
    /// `coop shell`/`exec` invocations.
    #[test]
    fn prepare_session_overlays_persisted_guest_env() {
        use std::num::NonZeroU16;

        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = super::config::Instance {
            name: super::config::InstanceName::new("test").expect("valid name"),
            index: super::config::InstanceIndex::new(0).expect("0 is in range"),
            dir: tmp.path().to_path_buf(),
            image: super::config::ImageName::new(super::config::DEFAULT_IMAGE)
                .expect("DEFAULT_IMAGE is valid"),
        };
        let mut state = super::guest_env_state::GuestEnvState::default();
        state.entries.insert(
            super::guest_env_state::EnvVarName::new("FROM_CLI").expect("valid env var"),
            "saved-value".to_string(),
        );
        state.save(&inst).expect("save snapshot");

        let mut cfg = super::config::CoopConfig::default();
        // Sanity: an entry in cfg without a CLI override should still
        // appear (so the overlay is additive, not replacing).
        cfg.guest_env.insert(
            super::guest_env_state::EnvVarName::new("FROM_CFG").expect("valid env var"),
            "cfg-value".to_string(),
        );

        let target = super::backend::SshTarget {
            host: super::backend::Hostname::new("127.0.0.1").expect("valid host"),
            port: NonZeroU16::new(22).expect("non-zero"),
            user: super::backend::SshUser::new("ubuntu").expect("valid user"),
            key_path: tmp.path().join("id_test"),
        };

        let session =
            super::prepare_session_from_target(&cfg, Some(&inst), target, None).expect("session");

        let envs = session.env.as_envs();
        assert_eq!(
            envs.get("FROM_CLI").map(String::as_str),
            Some("saved-value"),
            "persisted CLI --env entry must reach SshSession",
        );
        assert_eq!(
            envs.get("FROM_CFG").map(String::as_str),
            Some("cfg-value"),
            "config.toml [guest_env] entries must still flow through",
        );
    }

    /// In proxy mode a raw `ANTHROPIC_API_KEY` persisted via `coop start
    /// --env` must be dropped from the overlay, not re-injected into the guest
    /// (issue #411). The VM defaults to remote model mode, so a configured
    /// `[proxy.anthropic]` upstream makes proxy mode active.
    #[test]
    fn prepare_session_suppresses_persisted_proxy_key() {
        use std::num::NonZeroU16;

        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = super::config::Instance {
            name: super::config::InstanceName::new("test").expect("valid name"),
            index: super::config::InstanceIndex::new(0).expect("0 is in range"),
            dir: tmp.path().to_path_buf(),
            image: super::config::ImageName::new(super::config::DEFAULT_IMAGE)
                .expect("DEFAULT_IMAGE is valid"),
        };
        let mut state = super::guest_env_state::GuestEnvState::default();
        state.entries.insert(
            super::guest_env_state::EnvVarName::new("ANTHROPIC_API_KEY").expect("valid env var"),
            "sk-ant-realkey".to_string(),
        );
        state.entries.insert(
            super::guest_env_state::EnvVarName::new("FROM_CLI").expect("valid env var"),
            "saved-value".to_string(),
        );
        state.save(&inst).expect("save snapshot");

        let mut cfg = super::config::CoopConfig::default();
        cfg.proxy.anthropic = Some(super::config::ProxyUpstream {
            credential: super::config::Secret::new("cmd:true".to_string()),
            auth: super::config::ProxyAuthScheme::ApiKey,
        });

        let target = super::backend::SshTarget {
            host: super::backend::Hostname::new("127.0.0.1").expect("valid host"),
            port: NonZeroU16::new(22).expect("non-zero"),
            user: super::backend::SshUser::new("ubuntu").expect("valid user"),
            key_path: tmp.path().join("id_test"),
        };

        let session =
            super::prepare_session_from_target(&cfg, Some(&inst), target, None).expect("session");

        let envs = session.env.as_envs();
        assert!(
            !envs.contains_key("ANTHROPIC_API_KEY"),
            "proxy mode re-injected the raw key from the persisted --env overlay",
        );
        assert_eq!(
            envs.get("FROM_CLI").map(String::as_str),
            Some("saved-value"),
            "non-suppressed --env entries must still flow through",
        );
    }

    #[test]
    fn prepare_session_suppresses_persisted_openai_key_for_chatgpt_auth() {
        use std::num::NonZeroU16;

        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = super::config::Instance {
            name: super::config::InstanceName::new("test").expect("valid name"),
            index: super::config::InstanceIndex::new(0).expect("0 is in range"),
            dir: tmp.path().to_path_buf(),
            image: super::config::ImageName::new(super::config::DEFAULT_IMAGE)
                .expect("DEFAULT_IMAGE is valid"),
        };
        let mut state = super::guest_env_state::GuestEnvState::default();
        state.entries.insert(
            super::guest_env_state::EnvVarName::new("OPENAI_API_KEY").expect("valid env var"),
            "sk-openai-realkey".to_string(),
        );
        state.entries.insert(
            super::guest_env_state::EnvVarName::new("FROM_CLI").expect("valid env var"),
            "saved-value".to_string(),
        );
        state.save(&inst).expect("save snapshot");

        let mut cfg = super::config::CoopConfig::default();
        cfg.codex.auth = super::config::CodexAuthMode::ChatGpt;

        let target = super::backend::SshTarget {
            host: super::backend::Hostname::new("127.0.0.1").expect("valid host"),
            port: NonZeroU16::new(22).expect("non-zero"),
            user: super::backend::SshUser::new("ubuntu").expect("valid user"),
            key_path: tmp.path().join("id_test"),
        };

        let session =
            super::prepare_session_from_target(&cfg, Some(&inst), target, None).expect("session");

        let envs = session.env.as_envs();
        assert!(
            !envs.contains_key("OPENAI_API_KEY"),
            "ChatGPT account auth re-injected the raw key from the persisted --env overlay",
        );
        assert_eq!(
            envs.get("FROM_CLI").map(String::as_str),
            Some("saved-value"),
            "non-suppressed --env entries must still flow through",
        );
    }

    #[test]
    fn prepare_session_survives_chatgpt_auth_with_openai_proxy() {
        use std::num::NonZeroU16;

        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = super::config::Instance {
            name: super::config::InstanceName::new("test").expect("valid name"),
            index: super::config::InstanceIndex::new(0).expect("0 is in range"),
            dir: tmp.path().to_path_buf(),
            image: super::config::ImageName::new(super::config::DEFAULT_IMAGE)
                .expect("DEFAULT_IMAGE is valid"),
        };

        let mut cfg = super::config::CoopConfig::default();
        cfg.codex.auth = super::config::CodexAuthMode::ChatGpt;
        cfg.proxy.openai = Some(super::config::ProxyUpstream {
            credential: super::config::Secret::new("cmd:true".to_string()),
            auth: super::config::ProxyAuthScheme::Bearer,
        });

        let target = super::backend::SshTarget {
            host: super::backend::Hostname::new("127.0.0.1").expect("valid host"),
            port: NonZeroU16::new(22).expect("non-zero"),
            user: super::backend::SshUser::new("ubuntu").expect("valid user"),
            key_path: tmp.path().join("id_test"),
        };

        // The conflict makes Codex unusable, not the VM: a shell/exec/claude
        // session on the same instance must still come up. `coop codex` and
        // the agent bootstrap are what fail hard, via
        // `backend::ensure_codex_remote_auth_consistent`.
        let session = super::prepare_session_from_target(&cfg, Some(&inst), target, None)
            .expect("session must still be usable for non-Codex commands");

        assert!(
            !session.env.as_envs().contains_key("OPENAI_API_KEY"),
            "the raw key must stay on the host in both suppression modes",
        );
    }

    #[test]
    fn prepare_session_skips_overlay_without_instance() {
        use std::num::NonZeroU16;

        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = super::config::CoopConfig::default();
        let target = super::backend::SshTarget {
            host: super::backend::Hostname::new("127.0.0.1").expect("valid host"),
            port: NonZeroU16::new(22).expect("non-zero"),
            user: super::backend::SshUser::new("ubuntu").expect("valid user"),
            key_path: tmp.path().join("id_test"),
        };

        let session =
            super::prepare_session_from_target(&cfg, None, target, None).expect("session");

        assert!(
            !session.env.contains("FROM_CLI"),
            "without an instance, no persisted overlay should be applied",
        );
    }

    #[test]
    fn resolve_start_repo_uses_first_mount_origin() {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q"]);
        run_git(
            tmp.path(),
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/trailofbits/coop.git",
            ],
        );
        let mount = super::config::Mount {
            host_path: tmp.path().to_path_buf(),
            guest_path: super::workspace::default_workspace_path(),
        };
        let cfg_path = tmp.path().join("config.toml");
        let opts = start_opts(vec![mount], &cfg_path);
        let slug = super::resolve_start_repo(&opts).expect("ok");
        assert_eq!(
            slug.as_ref().map(super::github_repo::RepoSlug::as_str),
            Some("trailofbits/coop")
        );
    }

    #[test]
    fn resolve_start_repo_returns_none_for_non_github_mount() {
        let tmp = tempfile::tempdir().expect("tempdir");
        run_git(tmp.path(), &["init", "-q"]);
        run_git(
            tmp.path(),
            &["remote", "add", "origin", "https://gitlab.com/x/y.git"],
        );
        let mount = super::config::Mount {
            host_path: tmp.path().to_path_buf(),
            guest_path: super::workspace::default_workspace_path(),
        };
        let cfg_path = tmp.path().join("config.toml");
        let opts = start_opts(vec![mount], &cfg_path);
        let slug = super::resolve_start_repo(&opts).expect("ok");
        assert!(slug.is_none(), "got {slug:?}");
    }

    #[test]
    fn resolve_start_repo_returns_none_for_no_mounts() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("config.toml");
        let opts = start_opts(Vec::new(), &cfg_path);
        let slug = super::resolve_start_repo(&opts).expect("ok");
        assert!(slug.is_none());
    }

    #[test]
    fn restart_ignored_flags_allows_workspace_affinity_key() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("config.toml");
        let mut opts = start_opts(Vec::new(), &cfg_path);
        opts.workspace_dir = Some("/some/workspace");
        assert!(!super::restart_has_ignored_creation_flags(&opts));
    }

    #[test]
    fn restart_ignored_flags_rejects_workspace_with_explicit_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("config.toml");
        let name = super::config::InstanceName::new("explicit").expect("valid name");
        let mut opts = start_opts(Vec::new(), &cfg_path);
        opts.name = Some(&name);
        opts.workspace_dir = Some("/some/workspace");
        assert!(super::restart_has_ignored_creation_flags(&opts));
    }

    #[test]
    fn no_stopped_instance_message_points_creation_to_up() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("config.toml");
        let mut opts = start_opts(Vec::new(), &cfg_path);
        opts.devcontainer_path = Some(std::path::Path::new("/tmp/devcontainer.json"));
        let msg = super::no_stopped_instance_message(&opts, None);
        assert!(msg.contains("only starts stopped instances"));
        assert!(msg.contains("coop up [DIR]"));
    }

    #[test]
    fn find_git_repo_instance_returns_none_when_unmatched() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let found = super::find_git_repo_instance(&cfg, "https://example.com/org/absent.git")
            .expect("lookup");
        assert!(found.is_none());
    }

    #[test]
    fn find_git_repo_instance_errors_on_multiple_matches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let repo_url = "https://github.com/trailofbits/coop.git";
        for _ in 0..2 {
            let inst = cfg.allocate_instance(None, &img, None).expect("inst");
            let state = super::workspace::WorkspaceState {
                guest_path: super::workspace::default_workspace_path(),
                source: super::workspace::WorkspaceSource::GitRepo {
                    url: super::github_repo::GitRepoUrl::new(repo_url),
                },
            };
            state.save(&inst).expect("save workspace state");
        }
        let err = super::find_git_repo_instance(&cfg, repo_url).expect_err("expected ambiguity");
        assert!(format!("{err}").contains("Multiple instances"));
    }

    #[test]
    fn ensure_up_git_repo_name_matches_rejects_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let inst = cfg.allocate_instance(None, &img, None).expect("inst");
        let repo_url = "https://github.com/trailofbits/coop.git";

        let mut opts = up_opts_for_tests(None);
        let requested = super::config::InstanceName::new("other-name").expect("name");
        opts.name = Some(&requested);

        let err = super::ensure_up_git_repo_name_matches(&inst, repo_url, &opts)
            .expect_err("expected name mismatch");
        assert!(format!("{err}").contains("already associated"));
    }

    #[test]
    fn up_copy_rejects_extra_mount_at_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).expect("data");
        let mounts = vec![super::config::Mount::parse(data.to_str().unwrap()).expect("mount")];

        let err = crate::workspace::ValidatedMounts::assemble(
            crate::workspace::WorkspaceMountRule::CopyProject,
            mounts,
        )
        .expect_err("expected /workspace collision");
        assert!(format!("{err}").contains("/workspace"));
    }

    #[test]
    fn effective_image_prefers_profile_then_explicit_then_default() {
        let mut opts = up_opts_for_tests(None);
        assert_eq!(opts.effective_image(), super::config::default_image_name());

        let explicit = super::config::ImageName::new("custom").expect("image");
        opts.image = Some(explicit.clone());
        assert_eq!(opts.effective_image(), explicit);

        let target = super::ProfileImageTarget::new(&["node".to_string(), "python".to_string()])
            .expect("profile target");
        let profile_image = target.image.clone();
        opts.profile_target = Some(target);
        assert_eq!(opts.effective_image(), profile_image);
    }

    #[test]
    fn up_existing_rejects_creation_only_inputs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let inst = cfg
            .allocate_instance(None, &img, Some(&project))
            .expect("inst");
        let mut opts = up_opts_for_tests(project.to_str());
        opts.disk = super::config::GiB::new(20);

        let err = super::ensure_up_existing_inputs_are_compatible(
            &inst,
            super::ProjectTransport::Copy,
            &opts,
        )
        .expect_err("expected incompatible");
        assert!(format!("{err}").contains("--disk"));
    }

    #[test]
    fn up_existing_rejects_mismatched_profile_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let inst = cfg
            .allocate_instance(None, &img, Some(&project))
            .expect("inst");
        let mut opts = up_opts_for_tests(project.to_str());
        opts.profile_target = Some(
            super::ProfileImageTarget::new(&["python".to_string(), "node".to_string()])
                .expect("profile target"),
        );

        let err = super::ensure_up_existing_inputs_are_compatible(
            &inst,
            super::ProjectTransport::Copy,
            &opts,
        )
        .expect_err("expected profile mismatch rejection");
        let msg = format!("{err:#}");
        assert!(msg.contains("coop up --profile node,python"));
        assert!(msg.contains("profiles only apply when creating a new instance"));
    }

    #[test]
    fn up_existing_rejects_mismatched_explicit_name() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let inst = cfg
            .allocate_instance(None, &img, Some(&project))
            .expect("inst");
        let other = super::config::InstanceName::new("other").expect("valid name");
        let mut opts = up_opts_for_tests(project.to_str());
        opts.name = Some(&other);

        let err = super::ensure_up_project_name_matches(&inst, &project, &opts)
            .expect_err("expected mismatched explicit name");
        assert!(format!("{err}").contains("already associated"));
    }

    #[test]
    fn up_existing_git_repo_rejects_creation_only_inputs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let repo_url = "https://github.com/trailofbits/coop.git";
        let inst = cfg.allocate_instance(None, &img, None).expect("inst");
        let state = super::workspace::WorkspaceState {
            guest_path: super::workspace::default_workspace_path(),
            source: super::workspace::WorkspaceSource::GitRepo {
                url: super::github_repo::GitRepoUrl::new(repo_url),
            },
        };
        state.save(&inst).expect("save workspace state");

        let mut opts = up_opts_for_tests(None);
        opts.git_repo = Some(repo_url);
        opts.disk = super::config::GiB::new(20);

        let found = super::find_git_repo_instance(&cfg, repo_url)
            .expect("lookup")
            .expect("match");
        assert_eq!(found.name.as_str(), inst.name.as_str());

        let err = super::ensure_up_existing_inputs_are_compatible_for_git_repo(&inst, &opts)
            .expect_err("expected incompatible");
        assert!(format!("{err}").contains("--disk"));
    }

    #[test]
    fn up_running_rejects_restart_only_inputs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let inst = cfg
            .allocate_instance(None, &img, Some(&project))
            .expect("inst");
        let mut opts = up_opts_for_tests(project.to_str());
        opts.runtime.post_start = Some("echo hi".to_string());

        let err = super::reject_running_up_restart_inputs(&inst, &opts)
            .expect_err("expected restart-only rejection");
        assert!(format!("{err}").contains("--post-start"));
    }

    #[test]
    fn restart_creation_flags_detect_explicit_devcontainer() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg_path = tmp.path().join("config.toml");
        let mut opts = start_opts(Vec::new(), &cfg_path);
        opts.devcontainer_path = Some(std::path::Path::new("/tmp/devcontainer.json"));
        assert!(super::restart_has_ignored_creation_flags(&opts));
    }

    #[test]
    fn find_workspace_instance_returns_none_when_no_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let ws = tmp.path().join("project");
        std::fs::create_dir(&ws).expect("create_dir");
        let result = super::find_workspace_instance(&cfg, &ws).expect("ok");
        assert!(result.is_none());
    }

    #[test]
    fn find_workspace_instance_finds_single_match() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let ws = tmp.path().join("project");
        std::fs::create_dir(&ws).expect("create_dir");
        let canonical = ws.canonicalize().expect("canonicalize");
        let inst = cfg
            .allocate_instance(None, &img, Some(&canonical))
            .expect("allocate");
        write_workspace_state(&inst, &canonical);

        let found = super::find_workspace_instance(&cfg, &ws)
            .expect("ok")
            .expect("expected one match");
        assert_eq!(found.name.as_str(), inst.name.as_str());
    }

    #[test]
    fn find_workspace_instance_bails_on_multiple_matches() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let ws = tmp.path().join("project");
        std::fs::create_dir(&ws).expect("create_dir");
        let canonical = ws.canonicalize().expect("canonicalize");

        let inst_a = cfg
            .allocate_instance(
                Some(&super::config::InstanceName::new("alpha").unwrap()),
                &img,
                Some(&canonical),
            )
            .expect("allocate alpha");
        let inst_b = cfg
            .allocate_instance(
                Some(&super::config::InstanceName::new("beta").unwrap()),
                &img,
                Some(&canonical),
            )
            .expect("allocate beta");
        write_workspace_state(&inst_a, &canonical);
        write_workspace_state(&inst_b, &canonical);

        let err = super::find_workspace_instance(&cfg, &ws).expect_err("expected bail");
        let msg = format!("{err}");
        assert!(msg.contains("alpha"), "{msg}");
        assert!(msg.contains("beta"), "{msg}");
    }

    #[test]
    fn find_workspace_instance_ignores_unrelated_workspaces() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let ws_a = tmp.path().join("a");
        let ws_b = tmp.path().join("b");
        std::fs::create_dir(&ws_a).expect("create_dir a");
        std::fs::create_dir(&ws_b).expect("create_dir b");
        let a = ws_a.canonicalize().expect("canon a");

        let inst_a = cfg
            .allocate_instance(None, &img, Some(&a))
            .expect("allocate");
        write_workspace_state(&inst_a, &a);

        let found = super::find_workspace_instance(&cfg, &ws_b).expect("ok");
        assert!(found.is_none());
    }

    #[test]
    fn up_translator_inputs_maps_opts_to_translator_fields() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // A temp data_dir with no template_config.json makes
        // backend::persisted_guest_user fall back to the default user, so the
        // builder is deterministic.
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let mount_dir = tmp.path().join("mnt");
        std::fs::create_dir(&mount_dir).expect("mnt");

        let mut opts = up_opts_for_tests(None);
        opts.vcpus = Some(7);
        opts.mem =
            Some(super::config::VmMemory::new(super::config::MiB::new(2048).unwrap()).unwrap());
        opts.disk = super::config::GiB::new(50);
        opts.extra_mount =
            vec![super::config::Mount::parse(mount_dir.to_str().unwrap()).expect("mount")];
        opts.profile_target = Some(
            super::ProfileImageTarget::new(&["python".to_string(), "node".to_string()])
                .expect("profile target"),
        );
        opts.runtime.post_start = Some("echo hi".to_string());
        opts.runtime.forward_ports = vec![super::config::PortForward::parse("3000").expect("port")];
        opts.runtime.guest_env = vec![(
            super::guest_env_state::EnvVarName::new("FOO").expect("env"),
            "bar".to_string(),
        )];

        let inputs = super::up_translator_inputs(&cfg, &opts);

        assert_eq!(inputs.cli_vcpus, Some(7));
        assert_eq!(
            inputs.cli_mem_mib,
            Some(super::config::VmMemory::new(super::config::MiB::new(2048).unwrap()).unwrap())
        );
        assert_eq!(inputs.cli_disk_gib, super::config::GiB::new(50));
        assert_eq!(inputs.cli_post_start.as_deref(), Some("echo hi"));
        assert_eq!(
            inputs.cli_guest_env_keys,
            vec![super::guest_env_state::EnvVarName::new("FOO").unwrap()]
        );
        assert_eq!(inputs.cli_forward_ports.len(), 1);
        assert_eq!(inputs.cli_mounts.len(), 1);
        // ProfileImageTarget::new sorts and dedups, so order is canonical.
        assert_eq!(inputs.cli_profiles, vec!["node", "python"]);
        assert!(inputs.persisted_guest_user.is_some());
        assert!(inputs.cli_workspace_or_git_repo);
    }

    #[test]
    fn bytes_to_gib_truncates_to_whole_gibibytes() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(super::bytes_to_gib(0), 0);
        assert_eq!(super::bytes_to_gib(GIB), 1);
        assert_eq!(
            super::bytes_to_gib(GIB - 1),
            0,
            "just under 1 GiB rounds down"
        );
        assert_eq!(
            super::bytes_to_gib(2 * GIB + 512 * 1024 * 1024),
            2,
            "2.5 GiB truncates toward zero"
        );
    }

    #[test]
    fn project_dir_to_str_returns_the_path_string() {
        let s = super::project_dir_to_str(std::path::Path::new("/home/alice/project"))
            .expect("utf8 path");
        assert_eq!(s, "/home/alice/project");
    }

    fn instance_named(name: &str, dir: &std::path::Path) -> super::config::Instance {
        super::config::Instance {
            name: super::config::InstanceName::new(name).expect("valid name"),
            index: super::config::InstanceIndex::new(0).expect("0 in range"),
            dir: dir.to_path_buf(),
            image: super::config::ImageName::new(super::config::DEFAULT_IMAGE)
                .expect("DEFAULT_IMAGE is valid"),
        }
    }

    #[test]
    fn creation_options_rejected_message_names_instance_and_recovery() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let inst = instance_named("demo", tmp.path());
        let msg = super::creation_options_rejected_message(&inst);
        assert!(msg.contains("demo"), "{msg}");
        assert!(msg.contains("coop destroy demo"), "{msg}");
        assert!(msg.contains("already exists"), "{msg}");
    }

    /// `ensure_up_existing_inputs_are_compatible` must accept an `--image`
    /// that matches the existing instance's image (the guard only rejects a
    /// *different* image). Pins the `!=` comparison at the image check.
    #[test]
    fn up_existing_accepts_matching_image() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let inst = cfg
            .allocate_instance(None, &img, Some(&project))
            .expect("inst");

        let mut opts = up_opts_for_tests(project.to_str());
        opts.image = Some(inst.image.clone());

        // No workspace state is written, so the transport check short-circuits
        // to Ok after the (passing) image/creation checks.
        super::ensure_up_existing_inputs_are_compatible(
            &inst,
            super::ProjectTransport::Copy,
            &opts,
        )
        .expect("matching image is compatible");
    }

    /// Each creation-only flag, set *alone*, must independently trigger the
    /// rejection. This pins every `||` in the creation-flag chain (a `&&` flip
    /// on any one term would let that flag slip through) plus the `!` on the
    /// `extra_mount` emptiness test.
    #[test]
    fn up_existing_rejects_each_creation_flag_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let inst = cfg
            .allocate_instance(None, &img, Some(&project))
            .expect("inst");
        let mount_dir = tmp.path().join("mnt");
        std::fs::create_dir(&mount_dir).expect("mnt");
        let dc = tmp.path().join("devcontainer.json");

        let reject = |opts: &super::UpOpts<'_>| {
            super::ensure_up_existing_inputs_are_compatible(
                &inst,
                super::ProjectTransport::Copy,
                opts,
            )
        };

        let mut opts = up_opts_for_tests(project.to_str());
        opts.mem =
            Some(super::config::VmMemory::new(super::config::MiB::new(2048).unwrap()).unwrap());
        reject(&opts).expect_err("--mem must be rejected");

        let mut opts = up_opts_for_tests(project.to_str());
        opts.vcpus = Some(4);
        reject(&opts).expect_err("--vcpus must be rejected");

        let mut opts = up_opts_for_tests(project.to_str());
        opts.runtime.exclude_git = true;
        reject(&opts).expect_err("--exclude-git must be rejected");

        let mut opts = up_opts_for_tests(project.to_str());
        opts.extra_mount =
            vec![super::config::Mount::parse(mount_dir.to_str().unwrap()).expect("mount")];
        reject(&opts).expect_err("--extra-mount must be rejected");

        let mut opts = up_opts_for_tests(project.to_str());
        opts.devcontainer.input = super::DevcontainerInput::Explicit(dc);
        reject(&opts).expect_err("--devcontainer must be rejected");
    }

    /// When the stored transport matches the requested one, the check passes.
    /// Pins the `existing != transport` comparison (an `==` flip would reject
    /// a matching transport).
    #[test]
    fn up_existing_accepts_matching_transport() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let img = super::config::default_image_name();
        let project = tmp.path().join("project");
        std::fs::create_dir(&project).expect("project");
        let canonical = project.canonicalize().expect("canonicalize");
        let inst = cfg
            .allocate_instance(None, &img, Some(&canonical))
            .expect("inst");
        write_workspace_state(&inst, &canonical);

        let opts = up_opts_for_tests(project.to_str());
        super::ensure_up_existing_inputs_are_compatible(
            &inst,
            super::ProjectTransport::Copy,
            &opts,
        )
        .expect("matching copy transport is compatible");
    }

    #[test]
    fn up_existing_git_repo_accepts_matching_image_and_profile() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());

        // Matching --image is compatible.
        let inst = cfg
            .allocate_instance(None, &super::config::default_image_name(), None)
            .expect("inst");
        let mut opts = up_opts_for_tests(None);
        opts.image = Some(inst.image.clone());
        super::ensure_up_existing_inputs_are_compatible_for_git_repo(&inst, &opts)
            .expect("matching image is compatible");

        // A profile target whose derived image matches is compatible too.
        let target = super::ProfileImageTarget::new(&["node".to_string()]).expect("target");
        let profile_inst = cfg
            .allocate_instance(
                Some(&super::config::InstanceName::new("withprofile").unwrap()),
                &target.image,
                None,
            )
            .expect("inst");
        let mut opts = up_opts_for_tests(None);
        opts.profile_target = Some(target);
        super::ensure_up_existing_inputs_are_compatible_for_git_repo(&profile_inst, &opts)
            .expect("matching profile image is compatible");
    }

    #[test]
    fn up_existing_git_repo_rejects_each_creation_flag_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        let inst = cfg
            .allocate_instance(None, &super::config::default_image_name(), None)
            .expect("inst");
        let mount_dir = tmp.path().join("mnt");
        std::fs::create_dir(&mount_dir).expect("mnt");
        let dc = tmp.path().join("devcontainer.json");

        let mut opts = up_opts_for_tests(None);
        opts.mem =
            Some(super::config::VmMemory::new(super::config::MiB::new(2048).unwrap()).unwrap());
        super::ensure_up_existing_inputs_are_compatible_for_git_repo(&inst, &opts)
            .expect_err("--mem must be rejected");

        let mut opts = up_opts_for_tests(None);
        opts.extra_mount =
            vec![super::config::Mount::parse(mount_dir.to_str().unwrap()).expect("mount")];
        super::ensure_up_existing_inputs_are_compatible_for_git_repo(&inst, &opts)
            .expect_err("--extra-mount must be rejected");

        let mut opts = up_opts_for_tests(None);
        opts.devcontainer.input = super::DevcontainerInput::Explicit(dc);
        super::ensure_up_existing_inputs_are_compatible_for_git_repo(&inst, &opts)
            .expect_err("--devcontainer must be rejected");
    }

    #[test]
    fn up_has_restart_only_inputs_detects_each_flag() {
        // None set → false (pins the whole-body `true` mutant).
        let none = up_opts_for_tests(None);
        assert!(!super::up_has_restart_only_inputs(&none));

        let mut no_agents = up_opts_for_tests(None);
        no_agents.runtime.no_agents = true;
        assert!(super::up_has_restart_only_inputs(&no_agents));

        let mut ports = up_opts_for_tests(None);
        ports.runtime.forward_ports = vec![super::config::PortForward::parse("3000").unwrap()];
        assert!(super::up_has_restart_only_inputs(&ports));

        let mut post = up_opts_for_tests(None);
        post.runtime.post_start = Some("echo hi".to_string());
        assert!(super::up_has_restart_only_inputs(&post));

        let mut env = up_opts_for_tests(None);
        env.runtime.guest_env = vec![(
            super::guest_env_state::EnvVarName::new("K").unwrap(),
            "v".to_string(),
        )];
        assert!(super::up_has_restart_only_inputs(&env));
    }
}
