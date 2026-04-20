mod backend;
mod cmd;
mod config;
mod fs_util;
mod guest;
// Lima is an interactive CLI workflow — stderr output is intentional user communication.
#[cfg_attr(not(target_os = "macos"), expect(dead_code, reason = "Lima-only"))]
#[expect(
    clippy::print_stderr,
    reason = "lima setup is interactive CLI — stderr is user communication"
)]
mod lima;
#[cfg_attr(target_os = "macos", expect(dead_code, reason = "Firecracker-only"))]
mod network;
mod rootfs;
mod signal;
// Setup is an interactive CLI workflow — stderr output is intentional user communication.
#[cfg_attr(
    target_os = "macos",
    expect(dead_code, reason = "Firecracker setup functions unused on macOS")
)]
#[expect(
    clippy::print_stderr,
    reason = "setup is interactive CLI — stderr is user communication"
)]
mod setup;
mod shell;
mod ssh;
#[cfg_attr(target_os = "macos", expect(dead_code, reason = "Firecracker-only"))]
mod vm;
mod workspace;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};

use backend::VmBackend as _;
use cmd::Cmd;

#[derive(Parser)]
#[command(name = "coop", version)]
#[command(about = "Isolated VM environment for running Claude Code and Codex")]
struct Cli {
    /// Path to coop config file
    #[arg(long, default_value_os_t = config::CoopConfig::default_path())]
    config: PathBuf,

    /// Increase verbosity
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Args)]
struct SessionArgs {
    /// Instance name (required if multiple instances exist)
    name: Option<String>,
    /// tmux session name
    #[arg(long, conflicts_with = "no_tmux")]
    session: Option<String>,
    /// Skip tmux session persistence (raw SSH connection)
    #[arg(long)]
    no_tmux: bool,
}

impl SessionArgs {
    fn tmux_session<'a>(&'a self, default: &'a str) -> Option<&'a str> {
        if self.no_tmux {
            return None;
        }
        Some(self.session.as_deref().unwrap_or(default))
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Check prerequisites, install Firecracker, fetch kernel and build template rootfs
    Setup {
        /// Skip confirmation prompts (accept all)
        #[arg(short = 'y', long)]
        yes: bool,
        /// Number of vCPUs (overrides config)
        #[arg(long)]
        vcpus: Option<u8>,
        /// Memory in MiB (overrides config)
        #[arg(long)]
        mem: Option<u32>,
        /// Force rebuild of template rootfs
        #[arg(long)]
        rebuild: bool,
        /// Install profiles (comma-separated: python,node,c,fuzz,rust,go,full)
        #[arg(long, value_delimiter = ',')]
        profile: Vec<String>,
        /// Extra apt packages to install (comma-separated)
        #[arg(long, value_delimiter = ',')]
        extra_packages: Vec<String>,
        /// Path to a post-install script to run in the chroot
        #[arg(long)]
        post_install: Option<String>,
        /// Template rootfs size in GiB (default: 8)
        #[arg(long)]
        template_size: Option<u32>,
        /// Named image to build (default: "default")
        #[arg(long, default_value = config::DEFAULT_IMAGE)]
        image: String,
    },
    /// Build rootfs image and fetch kernel (use `setup` for first-time install)
    Build,
    /// Launch a new Firecracker VM instance
    Start {
        /// Instance name (auto-generated if omitted)
        name: Option<String>,
        /// Workspace directory to sync into the VM
        #[arg(long, conflicts_with = "git_repo")]
        workspace: Option<String>,
        /// Git repository URL to clone inside the VM
        #[arg(long, conflicts_with = "workspace")]
        git_repo: Option<String>,
        /// Number of vCPUs (overrides config)
        #[arg(long)]
        vcpus: Option<u8>,
        /// Memory in MiB (overrides config)
        #[arg(long)]
        mem: Option<u32>,
        /// Instance disk size in GiB (grows from template size if larger)
        #[arg(long)]
        disk: Option<u32>,
        /// Skip injecting Claude Code and Codex credentials/config into the VM
        #[arg(long)]
        no_claude: bool,
        /// Mount host directory into guest (`HOST_PATH[:GUEST_PATH]`, repeatable)
        #[arg(long, conflicts_with_all = ["workspace", "git_repo"])]
        mount: Vec<String>,
        /// Named image to use (default: "default")
        #[arg(long, default_value = config::DEFAULT_IMAGE)]
        image: String,
    },
    /// Open an interactive shell in the VM (or run a command non-interactively)
    #[command(alias = "ssh")]
    Shell {
        #[command(flatten)]
        session: SessionArgs,
        /// Command to run (non-interactive, no PTY)
        #[arg(allow_hyphen_values = true, last = true)]
        command: Vec<String>,
    },
    /// Launch Claude Code inside the VM (skips permissions by default)
    Claude {
        #[command(flatten)]
        session: SessionArgs,
        /// Prompt for permissions instead of skipping them
        #[arg(long)]
        ask: bool,
        /// Extra arguments passed to `claude`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Launch Codex inside the VM
    Codex {
        #[command(flatten)]
        session: SessionArgs,
        /// Extra arguments passed to `codex`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Gracefully stop the VM
    Stop {
        /// Instance name (required if multiple instances exist)
        name: Option<String>,
    },
    /// Stop and clean up instance resources (keeps template)
    Destroy {
        /// Instance name (required if multiple instances exist)
        name: Option<String>,
        /// Also remove template, kernel, and Firecracker binary
        #[arg(long)]
        all: bool,
    },
    /// Show VM status
    Status {
        /// Instance name (shows all if omitted)
        name: Option<String>,
    },
    /// Stream VM serial console logs
    Logs {
        /// Instance name (required if multiple instances exist)
        name: Option<String>,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Push local workspace into the running VM
    Push {
        /// Instance name (required if multiple instances exist)
        #[arg(long)]
        name: Option<String>,
        /// Local directory to push (defaults to `workspace.json` `host_path`)
        dir: Option<String>,
        /// Overwrite guest changes without confirmation
        #[arg(long)]
        force: bool,
    },
    /// Pull guest workspace to local directory
    Pull {
        /// Instance name (required if multiple instances exist)
        #[arg(long)]
        name: Option<String>,
        /// Local directory to pull into (defaults to `workspace.json` `host_path`)
        dir: Option<String>,
        /// Overwrite local changes without confirmation
        #[arg(long)]
        force: bool,
    },
    /// Run a command in the VM and return its output (non-interactive)
    Exec {
        /// Instance name (required if multiple instances exist)
        #[arg(long)]
        name: Option<String>,
        /// Command and arguments to run
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Open VS Code connected to the guest VM
    Vscode {
        /// Instance name (required if multiple instances exist)
        name: Option<String>,
        /// Remote path to open in VS Code
        #[arg(long, default_value = "/workspace")]
        project: String,
        /// Editor to use (e.g. "code"). Overrides auto-detection
        #[arg(long)]
        editor: Option<String>,
        /// Remove the SSH config entry for this instance and exit
        #[arg(long)]
        clean: bool,
    },
    /// List or manage golden images
    Images {
        /// Delete a named image
        #[arg(long)]
        delete: Option<String>,
    },
    /// Resize a stopped instance's disk
    Resize {
        /// Instance name (required if multiple instances exist)
        name: Option<String>,
        /// New size: absolute GiB (e.g. 150, 150G) or relative (e.g. +20, +20G)
        #[arg(long, required = true)]
        size: String,
    },
    /// List or inspect available profiles
    Profiles {
        #[command(subcommand)]
        action: Option<ProfilesAction>,
    },
    /// Validate configuration and check prerequisites
    Validate,
    /// Generate a starter config file at ~/.coop/config.toml
    Init,
}

#[derive(Subcommand)]
enum ProfilesAction {
    /// List all available profiles (builtin and custom)
    List,
    /// Show the full definition of a profile
    Show {
        /// Profile name to inspect
        name: String,
    },
}

fn init_tracing(verbosity: u8) {
    let filter = match verbosity {
        0 => "coop=info",
        1 => "coop=debug",
        _ => "coop=trace",
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()),
        )
        .init();
}

fn load_and_validate_config(path: &Path) -> Result<config::CoopConfig> {
    let cfg = config::CoopConfig::load(path)?;
    for w in cfg.validate()? {
        tracing::warn!("{w}");
    }
    Ok(cfg)
}

#[expect(clippy::too_many_lines, reason = "CLI dispatch — flat match arms")]
fn main() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if matches!(cli.command, Commands::Init) {
        return cmd_init(&cli.config);
    }

    let mut cfg = load_and_validate_config(&cli.config)?;
    let be: backend::PlatformBackend = backend::PlatformBackend::new();
    tracing::debug!("Using backend: {be}");

    match cli.command {
        Commands::Setup {
            yes,
            vcpus,
            mem,
            rebuild,
            profile,
            extra_packages,
            post_install,
            template_size,
            image,
        } => {
            apply_vm_overrides(&mut cfg, vcpus, mem, template_size)?;
            let _guard = signal::install_handlers();
            be.setup(
                &cfg,
                &setup::SetupOptions {
                    skip_confirm: yes,
                    rebuild,
                    profiles: profile,
                    extra_packages,
                    post_install: post_install.map(PathBuf::from),
                    image,
                },
            )
        }
        Commands::Build => cmd_build(&cfg),
        Commands::Start {
            name,
            workspace,
            git_repo,
            vcpus,
            mem,
            disk,
            no_claude,
            mount,
            image,
        } => {
            apply_vm_overrides(&mut cfg, vcpus, mem, None)?;
            let mounts = mount
                .iter()
                .map(|s| config::Mount::parse(s))
                .collect::<Result<Vec<_>>>()?;
            cmd_start(
                &be,
                &cfg,
                &StartOpts {
                    name: name.as_deref(),
                    image: &image,
                    workspace_dir: workspace.as_deref(),
                    git_repo: git_repo.as_deref(),
                    no_claude,
                    disk: disk
                        .map(|d| config::GiB::new(d).context("--disk must be > 0"))
                        .transpose()?,
                    mounts,
                },
            )
        }
        Commands::Shell { session, command } => {
            let tmux = session.tmux_session("main");
            cmd_shell(&be, &cfg, session.name.as_deref(), &command, tmux)
        }
        Commands::Claude {
            session,
            ask,
            mut args,
        } => {
            let running = resolve_running(&be, &cfg, session.name.as_deref())?;
            let env_vars = backend::prepare_env_forwarding(&cfg)?;
            let sess = backend::SshSession {
                target: &running.target,
                env: &env_vars,
            };
            if !ask {
                args.insert(0, "--dangerously-skip-permissions".to_string());
            }
            let tmux = session.tmux_session("claude");
            ssh::run_interactive(&sess, crate::guest::CLAUDE_BIN, &args, tmux)
        }
        Commands::Codex { session, args } => {
            let running = resolve_running(&be, &cfg, session.name.as_deref())?;
            let env_vars = backend::prepare_env_forwarding(&cfg)?;
            let sess = backend::SshSession {
                target: &running.target,
                env: &env_vars,
            };
            let tmux = session.tmux_session("codex");
            ssh::run_interactive(&sess, crate::guest::CODEX_BIN, &args, tmux)
        }
        Commands::Stop { name } => {
            let inst = cfg.resolve_instance(name.as_deref())?;
            cmd_stop(&be, &cfg, &inst)
        }
        Commands::Destroy { name, all } => {
            let _guard = signal::install_handlers();
            cmd_destroy(&be, &cfg, name.as_deref(), all)
        }
        Commands::Status { name } => cmd_status(&be, &cfg, name.as_deref()),
        Commands::Logs { name, follow } => {
            let running = resolve_running(&be, &cfg, name.as_deref())?;
            be.stream_logs(&cfg, &running.inst, follow)
        }
        Commands::Push { name, dir, force } => {
            let running = resolve_running(&be, &cfg, name.as_deref())?;
            workspace::push(&running.target, &running.inst, dir.as_deref(), force)
        }
        Commands::Pull { name, dir, force } => {
            let running = resolve_running(&be, &cfg, name.as_deref())?;
            workspace::pull(&running.target, &running.inst, dir.as_deref(), force)
        }
        Commands::Exec { name, command } => cmd_exec(&be, &cfg, name.as_deref(), &command),
        Commands::Vscode {
            name,
            project,
            editor,
            clean,
        } => {
            if clean {
                let inst = cfg.resolve_instance(name.as_deref())?;
                workspace::remove_ssh_config(&inst)?;
                tracing::info!("Removed SSH config for '{}'", inst.name);
                return Ok(());
            }
            let running = resolve_running(&be, &cfg, name.as_deref())?;
            workspace::vscode(
                &running.target,
                &running.inst,
                Some(&project),
                editor.as_deref(),
            )
        }
        Commands::Images { delete } => cmd_images(&be, &cfg, delete.as_deref()),
        Commands::Resize { name, size } => cmd_resize(&be, &cfg, name.as_deref(), &size),
        Commands::Profiles { action } => {
            cmd_profiles(&cfg, &action.unwrap_or(ProfilesAction::List))
        }
        Commands::Validate => cmd_validate(&cfg, &be),
        Commands::Init => unreachable!("handled before config loading"),
    }
}

fn cmd_init(config_path: &Path) -> Result<()> {
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
    std::fs::write(config_path, include_str!("../config.example.toml"))
        .with_context(|| format!("Failed to write {}", config_path.display()))?;
    writeln!(
        std::io::stdout(),
        "Created {}. Edit to customize, or leave as-is for defaults.",
        config_path.display()
    )
    .ok();
    Ok(())
}

fn cmd_validate(cfg: &config::CoopConfig, be: &backend::PlatformBackend) -> Result<()> {
    writeln!(std::io::stdout(), "Validating config (backend: {be})...",).ok();

    let warnings = cfg.validate()?;

    for w in &warnings {
        writeln!(std::io::stdout(), "  warning: {w}").ok();
    }

    writeln!(std::io::stdout(), "Config OK").ok();
    Ok(())
}

fn apply_vm_overrides(
    cfg: &mut config::CoopConfig,
    vcpus: Option<u8>,
    mem: Option<u32>,
    template_size: Option<u32>,
) -> Result<()> {
    if let Some(v) = vcpus {
        cfg.vm.vcpu_count = std::num::NonZeroU8::new(v).context("--vcpus must be > 0")?;
    }
    if let Some(m) = mem {
        cfg.vm.mem_size_mib = config::MiB::new(m).context("--mem must be > 0")?;
    }
    if let Some(ts) = template_size {
        cfg.vm.template_size_gib = config::GiB::new(ts).context("--template-size must be > 0")?;
    }
    Ok(())
}

fn cmd_build(cfg: &config::CoopConfig) -> Result<()> {
    tracing::info!("Building rootfs and fetching kernel");
    rootfs::build(cfg)?;
    tracing::info!("Build complete");
    Ok(())
}

struct StartOpts<'a> {
    name: Option<&'a str>,
    image: &'a str,
    workspace_dir: Option<&'a str>,
    git_repo: Option<&'a str>,
    no_claude: bool,
    disk: Option<config::GiB>,
    mounts: Vec<config::Mount>,
}

fn cmd_start(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    opts: &StartOpts<'_>,
) -> Result<()> {
    let ws_path = opts.workspace_dir.map(Path::new);
    if let Some(inst) = find_stopped_instance(be, cfg, opts.name, ws_path)? {
        let has_ignored_flags = !opts.mounts.is_empty()
            || opts.workspace_dir.is_some()
            || opts.git_repo.is_some()
            || opts.disk.is_some();

        if has_ignored_flags {
            bail!(
                "Instance '{}' already exists (stopped). The flags \
                 --mount, --workspace, --git-repo, and --disk are only \
                 applied at creation time and would be silently ignored \
                 on restart.\n\
                 To apply new options, destroy the instance first:\n  \
                 coop destroy {0}\n  coop start {0} [options]",
                inst.name,
            );
        }

        return restart_instance(be, cfg, &inst, opts.no_claude);
    }

    let inst = cfg.allocate_instance(opts.name, opts.image, ws_path)?;
    tracing::info!("Starting instance '{}' (index {})", inst.name, inst.index);

    let _guard = signal::install_handlers();
    let result = start_instance(be, cfg, &inst, opts);

    if let Err(e) = &result {
        tracing::error!("Failed to start instance '{}': {e}", inst.name);
        if let Err(cleanup_err) = be.destroy_instance(cfg, &inst) {
            tracing::debug!("Cleanup failed (non-fatal): {cleanup_err}");
        }
        if let Err(ssh_err) = workspace::remove_ssh_config(&inst) {
            tracing::debug!("SSH config cleanup failed (non-fatal): {ssh_err}");
        }
    }

    result
}

/// Find a stopped instance to restart, if applicable.
///
/// With a name: returns the instance if it exists and is stopped,
/// errors if it's running, returns None if it doesn't exist.
///
/// With a workspace path (no name): looks up instances by their stored
/// workspace `host_path`. If a match is found and running, errors with
/// a helpful message. If stopped, returns it for restart. If no match,
/// returns None so a new instance is allocated.
///
/// With neither: returns the single stopped instance if exactly one
/// exists, errors if multiple stopped instances exist, returns None
/// if none exist.
fn find_stopped_instance(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&str>,
    workspace_dir: Option<&Path>,
) -> Result<Option<config::Instance>> {
    let mut instances = cfg.list_instances()?;

    if let Some(name) = name {
        let Some(inst) = instances.into_iter().find(|i| i.name.as_str() == name) else {
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
                workspace::WorkspaceState::try_load(inst)
                    .ok()
                    .flatten()
                    .and_then(|s| s.host_path)
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
    cfg: &config::CoopConfig,
    inst: &config::Instance,
    no_claude: bool,
) -> Result<()> {
    tracing::info!("Restarting stopped instance '{}'", inst.name);

    let _guard = signal::install_handlers();

    be.start_existing(cfg, inst)?;

    signal::check_shutdown()?;

    let target = be.ssh_target(cfg, inst)?;
    target
        .wait_until_ready(std::time::Duration::from_secs(30))
        .context("Guest booted but SSH is not accepting connections")?;

    signal::check_shutdown()?;

    if no_claude {
        tracing::info!("Skipping guest agent bootstrap (--no-claude)");
    } else {
        let env_vars = backend::prepare_env_forwarding(cfg)?;
        let session = backend::SshSession {
            target: &target,
            env: &env_vars,
        };
        backend::bootstrap_agents(&session, cfg, inst, true)?;
    }

    tracing::info!(
        "Instance '{}' restarted — SSH: {}:{}",
        inst.name,
        target.host,
        target.port,
    );
    Ok(())
}

fn start_instance(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
    opts: &StartOpts<'_>,
) -> Result<()> {
    be.create_and_start(cfg, inst, opts.disk, &opts.mounts)?;

    signal::check_shutdown()?;

    let target = be.ssh_target(cfg, inst)?;
    target
        .wait_until_ready(std::time::Duration::from_secs(30))
        .context("Guest booted but SSH is not accepting connections")?;

    signal::check_shutdown()?;

    let env_vars = backend::prepare_env_forwarding(cfg)?;

    if opts.no_claude {
        tracing::info!("Skipping guest agent bootstrap (--no-claude)");
    } else {
        let session = backend::SshSession {
            target: &target,
            env: &env_vars,
        };
        backend::bootstrap_agents(&session, cfg, inst, false)?;
    }

    signal::check_shutdown()?;

    // Workspace sync: tar-pipe for --workspace, git clone for --git-repo,
    // rsync for --mount on Firecracker (Lima mounts are live via virtiofs).
    if let Some(ws_dir) = opts.workspace_dir {
        let ws_path = std::path::Path::new(ws_dir);
        anyhow::ensure!(
            ws_path.is_dir(),
            "Workspace path {ws_dir} is not a directory"
        );

        let abs_path = ws_path
            .canonicalize()
            .with_context(|| format!("Failed to resolve {ws_dir}"))?;

        workspace::tar_pipe_transfer(&target, &abs_path)?;

        let state = workspace::WorkspaceState {
            host_path: Some(abs_path),
            guest_path: "/workspace".to_string(),
            source: workspace::WorkspaceSource::Workspace,
        };
        state.save(inst)?;
    } else if let Some(repo_url) = opts.git_repo {
        backend::clone_git_repo(&target, repo_url)?;

        let state = workspace::WorkspaceState {
            host_path: None,
            guest_path: "/workspace".to_string(),
            source: workspace::WorkspaceSource::GitRepo,
        };
        state.save(inst)?;
    } else if !opts.mounts.is_empty() && !be.mounts_are_live() {
        workspace::sync_mounts(&target, inst, &opts.mounts)?;
        tracing::warn!(
            "Firecracker mounts use one-time sync, not live filesystem sharing. \
             Use `coop push` / `coop pull` to sync changes."
        );
    }

    tracing::info!(
        "Instance '{}' started — SSH: {}:{}",
        inst.name,
        target.host,
        target.port,
    );
    Ok(())
}

/// Resolve an instance that must be running.
///
/// With a name: finds that instance and verifies it's running.
/// Without a name: auto-selects if exactly one instance is running.
/// Provides contextual error messages for every failure mode.
fn resolve_running(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&str>,
) -> Result<backend::RunningInstance> {
    let instances = cfg.list_instances()?;

    if let Some(name) = name {
        let inst = instances
            .into_iter()
            .find(|i| i.name.as_str() == name)
            .with_context(|| {
                format!(
                    "No instance named '{name}'.\n\
                     Create one with: coop start {name}"
                )
            })?;
        if !be.is_running(&inst) {
            bail!(
                "Instance '{name}' is not running.\n\
                 Start it with: coop start {name}"
            );
        }
        let target = be.ssh_target(cfg, &inst)?;
        return Ok(backend::RunningInstance { inst, target });
    }

    let (running, stopped): (Vec<_>, Vec<_>) =
        instances.into_iter().partition(|i| be.is_running(i));

    match running.len() {
        1 => {
            let inst = running
                .into_iter()
                .next()
                .context("Instance list unexpectedly empty")?;
            let target = be.ssh_target(cfg, &inst)?;
            Ok(backend::RunningInstance { inst, target })
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
                 Create one with: coop start\n\
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

fn cmd_shell(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&str>,
    command: &[String],
    tmux_session: Option<&str>,
) -> Result<()> {
    let running = resolve_running(be, cfg, name)?;
    let env_vars = backend::prepare_env_forwarding(cfg)?;
    let session = backend::SshSession {
        target: &running.target,
        env: &env_vars,
    };
    if command.is_empty() {
        ssh::connect(&session, tmux_session)
    } else {
        ssh::run_command(&session, command)
    }
}

fn cmd_exec(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&str>,
    command: &[String],
) -> Result<()> {
    let running = resolve_running(be, cfg, name)?;
    let env_vars = backend::prepare_env_forwarding(cfg)?;
    let session = backend::SshSession {
        target: &running.target,
        env: &env_vars,
    };
    ssh::exec_command(&session, command)
}

fn cmd_stop(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    inst: &config::Instance,
) -> Result<()> {
    tracing::info!("Stopping instance '{}'", inst.name);
    be.stop(cfg, inst)?;
    if let Err(e) = workspace::remove_ssh_config(inst) {
        tracing::debug!("SSH config cleanup failed (non-fatal): {e}");
    }
    tracing::info!("Instance '{}' stopped", inst.name);
    Ok(())
}

fn cmd_destroy(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&str>,
    all: bool,
) -> Result<()> {
    if all {
        let instances = cfg.list_instances()?;
        for inst in &instances {
            tracing::info!("Destroying instance '{}'", inst.name);
            be.destroy_instance(cfg, inst)?;
            workspace::remove_ssh_config(inst)?;
        }

        be.destroy_shared(cfg);

        let key = cfg.ssh_key_path();
        if let Err(e) = std::fs::remove_file(&key) {
            tracing::debug!("Failed to remove SSH private key (non-fatal): {e}");
        }
        if let Err(e) = std::fs::remove_file(key.with_extension("pub")) {
            tracing::debug!("Failed to remove SSH public key (non-fatal): {e}");
        }

        let instances_dir = cfg.instances_dir();
        if instances_dir.exists() {
            // Instance dirs may be root-owned (Firecracker) or user-owned (Lima)
            if let Err(e) = std::fs::remove_dir_all(&instances_dir) {
                tracing::debug!("User remove_dir_all failed, trying sudo: {e}");
                if let Err(e) = Cmd::new("rm").arg("-rf").arg(&instances_dir).sudo().run() {
                    tracing::debug!(
                        "Failed to remove instances dir {} (non-fatal): {e}",
                        instances_dir.display()
                    );
                }
            }
        }

        workspace::remove_all_ssh_config()?;
        tracing::info!("All resources cleaned up");
    } else {
        let inst = cfg.resolve_instance(name)?;
        tracing::info!("Destroying instance '{}'", inst.name);
        be.destroy_instance(cfg, &inst)?;
        workspace::remove_ssh_config(&inst)?;
        tracing::info!("Instance '{}' destroyed", inst.name);
    }

    Ok(())
}

fn cmd_status(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&str>,
) -> Result<()> {
    if let Some(name) = name {
        let inst = cfg.resolve_instance(Some(name))?;
        let status = be.status(cfg, &inst)?;
        writeln!(std::io::stdout(), "{status}")
            .map_err(|e| anyhow::anyhow!("Failed to write status: {e}"))?;
    } else {
        let instances = cfg.list_instances()?;
        if instances.is_empty() {
            writeln!(std::io::stdout(), "No instances found")
                .map_err(|e| anyhow::anyhow!("Failed to write status: {e}"))?;
            return Ok(());
        }
        for inst in &instances {
            let running = be.is_running(inst);
            let state = if running { "running" } else { "stopped" };
            let usage_str = if running {
                be.ssh_target(cfg, inst)
                    .ok()
                    .and_then(|t| backend::query_resource_usage(&t))
                    .map(|u| format!("  {}", u.summary()))
                    .unwrap_or_default()
            } else {
                String::new()
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

fn cmd_resize(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&str>,
    size: &str,
) -> Result<()> {
    let inst = cfg.resolve_instance(name)?;
    let disk_size = config::DiskSize::parse(size)?;

    let current_gib = current_disk_gib(be, &inst)?;
    let new_size = disk_size.resolve(current_gib)?;

    be.resize_disk(cfg, &inst, new_size)
}

fn current_disk_gib(be: &backend::PlatformBackend, inst: &config::Instance) -> Result<u32> {
    let path = be.disk_path(inst)?;
    let bytes = std::fs::metadata(&path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .len();
    #[expect(clippy::cast_possible_truncation, reason = "disk GiB fits in u32")]
    let gib = (bytes / (1024 * 1024 * 1024)) as u32;
    Ok(gib)
}

fn cmd_profiles(cfg: &config::CoopConfig, action: &ProfilesAction) -> Result<()> {
    let out = &mut std::io::stdout();
    let write_result = match action {
        ProfilesAction::List => write_profiles_list(out, cfg),
        ProfilesAction::Show { name } => {
            let def = guest::lookup_profile(name, &cfg.profiles)?;
            write_profile_show(out, cfg, name, &def)
        }
    };
    write_result.context("failed to write profile output")
}

fn write_profiles_list(
    out: &mut impl std::io::Write,
    cfg: &config::CoopConfig,
) -> std::io::Result<()> {
    let mut custom_names: Vec<&str> = cfg.profiles.keys().map(String::as_str).collect();
    custom_names.sort_unstable();

    let width = guest::BUILTIN_PROFILES
        .iter()
        .map(|bp| bp.name.len())
        .chain(custom_names.iter().map(|n| n.len()))
        .max()
        .unwrap_or(0);

    writeln!(out, "Builtin:")?;
    for bp in guest::BUILTIN_PROFILES {
        let summary = builtin_summary(bp);
        writeln!(out, "  {:<width$} {summary}", bp.name)?;
    }

    if !custom_names.is_empty() {
        writeln!(out)?;
        writeln!(out, "Custom:")?;
        for name in custom_names {
            let cp = &cfg.profiles[name];
            let detail = format_custom_summary(cp);
            writeln!(out, "  {name:<width$} {detail}")?;
        }
    }
    Ok(())
}

fn builtin_summary(bp: &guest::BuiltinProfile) -> String {
    let mut parts = Vec::new();
    if !bp.apt_packages.is_empty() {
        parts.push(bp.apt_packages.join(", "));
    }
    if bp.pre_install.is_some() {
        parts.push("pre-install script".to_owned());
    }
    if bp.post_install.is_some() {
        parts.push("post-install script".to_owned());
    }
    if !bp.plugins.is_empty() {
        parts.push(format!("plugins: {}", bp.plugins.join(", ")));
    }
    if parts.is_empty() {
        "(empty)".to_owned()
    } else {
        parts.join("; ")
    }
}

fn write_profile_show(
    out: &mut impl std::io::Write,
    cfg: &config::CoopConfig,
    name: &str,
    def: &guest::ProfileDef,
) -> std::io::Result<()> {
    let origin = if cfg.profiles.contains_key(name) {
        "custom"
    } else {
        "builtin"
    };
    writeln!(out, "Profile: {name} ({origin})")?;
    writeln!(
        out,
        "  apt_packages: {}",
        if def.apt_packages.is_empty() {
            "(none)".to_string()
        } else {
            def.apt_packages.join(", ")
        }
    )?;
    writeln!(
        out,
        "  pre_install:  {}",
        script_summary(def.pre_install.as_deref())
    )?;
    writeln!(
        out,
        "  post_install: {}",
        script_summary(def.post_install.as_deref())
    )?;
    writeln!(
        out,
        "  marketplaces: {}",
        if def.marketplaces.is_empty() {
            "(none)".to_string()
        } else {
            def.marketplaces.join(", ")
        }
    )?;
    writeln!(
        out,
        "  plugins:      {}",
        if def.plugins.is_empty() {
            "(none)".to_string()
        } else {
            def.plugins.join(", ")
        }
    )?;
    Ok(())
}

fn format_custom_summary(cp: &config::CustomProfile) -> String {
    let mut parts = Vec::new();
    if !cp.apt_packages.is_empty() {
        parts.push(format!("{} apt packages", cp.apt_packages.len()));
    }
    if cp.pre_install.is_some() {
        parts.push("pre-install script".to_string());
    }
    if cp.post_install.is_some() {
        parts.push("post-install script".to_string());
    }
    if !cp.marketplaces.is_empty() {
        parts.push(format!("{} marketplaces", cp.marketplaces.len()));
    }
    if !cp.plugins.is_empty() {
        parts.push(format!("{} plugins", cp.plugins.len()));
    }
    if parts.is_empty() {
        "(empty)".to_string()
    } else {
        format!("({})", parts.join(", "))
    }
}

fn script_summary(script: Option<&str>) -> String {
    match script {
        None | Some("") => "(none)".to_string(),
        Some(s) => {
            let lines = s.lines().count();
            let first = s.lines().next().unwrap_or("");
            if lines <= 1 {
                first.to_string()
            } else {
                format!("{first} ... ({lines} lines)")
            }
        }
    }
}

fn cmd_images(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    delete: Option<&str>,
) -> Result<()> {
    if let Some(name) = delete {
        return be.destroy_image(cfg, name);
    }

    let images = cfg.list_images()?;
    if images.is_empty() {
        writeln!(
            std::io::stdout(),
            "No images found. Run `coop setup` to build one."
        )
        .map_err(|e| anyhow::anyhow!("Failed to write: {e}"))?;
        return Ok(());
    }

    for img in &images {
        let profiles = match &img.config {
            Some(c) if !c.profiles.is_empty() => c.profiles.join(", "),
            Some(_) => "none".to_string(),
            None => "unknown".to_string(),
        };
        let created = img
            .config
            .as_ref()
            .map_or("unknown", |c| c.created.as_str());
        let size = dir_size_display(&img.dir);
        writeln!(
            std::io::stdout(),
            "{:<20} profiles: {:<30} created: {:<24} size: {}",
            img.name,
            profiles,
            created,
            size,
        )
        .map_err(|e| anyhow::anyhow!("Failed to write: {e}"))?;
    }
    Ok(())
}

fn dir_size_display(dir: &std::path::Path) -> String {
    let mut total: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    #[expect(clippy::cast_precision_loss, reason = "file sizes fit in f64")]
    let gib = total as f64 / (1024.0 * 1024.0 * 1024.0);
    format!("{gib:.1} GiB")
}

#[cfg(test)]
#[expect(clippy::panic, reason = "tests use panic for unreachable branches")]
mod tests {
    use clap::Parser;

    use super::Cli;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("coop").chain(args.iter().copied()))
    }

    fn parse_err(args: &[&str]) -> clap::Error {
        match Cli::try_parse_from(std::iter::once("coop").chain(args.iter().copied())) {
            Err(e) => e,
            Ok(_) => panic!("expected parse failure"),
        }
    }

    #[test]
    fn shell_subcommand_parses() {
        let cli = parse(&["shell"]);
        assert!(matches!(cli.command, super::Commands::Shell { .. }));
    }

    #[test]
    fn ssh_alias_parses_as_shell() {
        let cli = parse(&["ssh"]);
        assert!(matches!(cli.command, super::Commands::Shell { .. }));
    }

    #[test]
    fn shell_session_flag_parses() {
        let cli = parse(&["shell", "--session", "work"]);
        let super::Commands::Shell { session, .. } = cli.command else {
            panic!("expected Shell variant");
        };
        assert_eq!(session.session.as_deref(), Some("work"));
        assert_eq!(session.tmux_session("main"), Some("work"));
    }

    #[test]
    fn shell_default_no_session() {
        let cli = parse(&["shell"]);
        let super::Commands::Shell { session, .. } = cli.command else {
            panic!("expected Shell variant");
        };
        assert!(session.session.is_none());
        assert_eq!(session.tmux_session("main"), Some("main"));
    }

    #[test]
    fn shell_session_and_no_tmux_conflict() {
        let err = parse_err(&["shell", "--session", "x", "--no-tmux"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn claude_session_flag_parses() {
        let cli = parse(&["claude", "--session", "dev"]);
        let super::Commands::Claude { session, .. } = cli.command else {
            panic!("expected Claude variant");
        };
        assert_eq!(session.session.as_deref(), Some("dev"));
        assert_eq!(session.tmux_session("claude"), Some("dev"));
    }

    #[test]
    fn claude_session_and_no_tmux_conflict() {
        let err = parse_err(&["claude", "--session", "x", "--no-tmux"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn claude_name_and_trailing_args_parse() {
        let cli = parse(&["claude", "myvm", "--", "--model", "opus"]);
        let super::Commands::Claude { session, args, .. } = cli.command else {
            panic!("expected Claude variant");
        };
        assert_eq!(session.name.as_deref(), Some("myvm"));
        assert_eq!(args, vec!["--model", "opus"]);
    }

    #[test]
    fn codex_session_flag_parses() {
        let cli = parse(&["codex", "--session", "dev"]);
        let super::Commands::Codex { session, .. } = cli.command else {
            panic!("expected Codex variant");
        };
        assert_eq!(session.session.as_deref(), Some("dev"));
        assert_eq!(session.tmux_session("codex"), Some("dev"));
    }

    #[test]
    fn codex_session_and_no_tmux_conflict() {
        let err = parse_err(&["codex", "--session", "x", "--no-tmux"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn codex_name_and_trailing_args_parse() {
        let cli = parse(&["codex", "myvm", "--", "--model", "gpt-5"]);
        let super::Commands::Codex { session, args, .. } = cli.command else {
            panic!("expected Codex variant");
        };
        assert_eq!(session.name.as_deref(), Some("myvm"));
        assert_eq!(args, vec!["--model", "gpt-5"]);
    }

    #[test]
    fn shell_no_tmux_without_session() {
        let cli = parse(&["shell", "--no-tmux"]);
        let super::Commands::Shell { session, .. } = cli.command else {
            panic!("expected Shell variant");
        };
        assert!(session.no_tmux);
        assert!(session.tmux_session("main").is_none());
    }

    #[test]
    fn profiles_list_parses() {
        let cli = parse(&["profiles", "list"]);
        let super::Commands::Profiles { action } = cli.command else {
            panic!("expected Profiles variant");
        };
        assert!(matches!(action, Some(super::ProfilesAction::List)));
    }

    #[test]
    fn profiles_bare_defaults_to_list() {
        let cli = parse(&["profiles"]);
        let super::Commands::Profiles { action } = cli.command else {
            panic!("expected Profiles variant");
        };
        assert!(
            action.is_none(),
            "bare `profiles` should have no action (defaults to List at dispatch)"
        );
    }

    #[test]
    fn profiles_show_parses() {
        let cli = parse(&["profiles", "show", "rust"]);
        let super::Commands::Profiles { action } = cli.command else {
            panic!("expected Profiles variant");
        };
        let Some(super::ProfilesAction::Show { name }) = action else {
            panic!("expected Show variant");
        };
        assert_eq!(name, "rust");
    }

    #[test]
    fn profiles_show_requires_name() {
        let err = parse_err(&["profiles", "show"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }
}
