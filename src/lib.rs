//! `coop` library crate.
//!
//! Owns every module declaration and the CLI dispatch (`run`). The binary
//! (`src/main.rs`) is a thin shim that calls [`run`]. Splitting the crate this
//! way lets fuzz targets and integration tests depend on `coop` directly
//! (e.g. `coop::config::CoopConfig`) instead of `#[path]`-including modules.

mod backend;
mod cmd;
mod completions;
pub mod config;
mod devcontainer;
mod devcontainer_oci;
mod fs_util;
mod git_repo_devcontainer;
mod github_pat;
pub mod github_repo;
mod github_submodules;
mod guest;
mod guest_env_state;
pub mod jsonc;
mod naming;
mod pat_prompt;
mod paths;
mod port_forward;
mod remote_command;
mod secret_store;
mod sha256_hash;
// Lima is an interactive CLI workflow — stderr output is intentional user communication.
#[cfg_attr(not(target_os = "macos"), expect(dead_code, reason = "Lima-only"))]
#[expect(
    clippy::print_stderr,
    reason = "lima setup is interactive CLI — stderr is user communication"
)]
mod lima;
#[cfg_attr(target_os = "macos", expect(dead_code, reason = "Firecracker-only"))]
mod network;
mod prompt;
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
mod update;
#[cfg_attr(target_os = "macos", expect(dead_code, reason = "Firecracker-only"))]
mod vm;
mod workspace;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCandidates;

use backend::VmBackend as _;
use cmd::Cmd;

#[derive(Parser)]
#[command(name = "coop", version = env!("COOP_VERSION_STR"))]
#[command(about = "Isolated VM environment for running Claude Code and Codex")]
pub(crate) struct Cli {
    /// Path to coop config file
    #[arg(long, default_value_os_t = config::CoopConfig::default_path())]
    config: PathBuf,

    /// Increase verbosity
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Ensure an environment for a project directory exists and is running.
    ///
    /// Re-runnable: if an instance already exists for DIR it is reused
    /// (running) or restarted (stopped) instead of being recreated.
    Up {
        /// Project directory (default: current directory)
        dir: Option<String>,
        /// Instance name to use when creating the project environment
        #[arg(long, value_parser = config::InstanceName::new)]
        name: Option<config::InstanceName>,
        /// Copy/sync DIR into the guest as /workspace (default)
        #[arg(long, conflicts_with = "mount")]
        copy: bool,
        /// Mount DIR at /workspace instead of using --copy
        #[arg(long)]
        mount: bool,
        /// Additional host directory to mount into the guest (`HOST_PATH[:GUEST_PATH]`, repeatable)
        #[arg(long, value_parser = config::Mount::parse)]
        extra_mount: Vec<config::Mount>,
        /// Clone a git repository into /workspace instead of copying a local project directory
        #[arg(long, conflicts_with_all = ["dir", "copy", "mount"])]
        git_repo: Option<String>,
        /// Number of vCPUs (overrides config when creating a new instance)
        #[arg(long)]
        vcpus: Option<u8>,
        /// Memory in MiB (overrides config when creating a new instance)
        #[arg(long, value_parser = config::MiB::parse_cli)]
        mem: Option<config::MiB>,
        /// Instance disk size in GiB (only used when creating a new instance)
        #[arg(long, value_parser = config::GiB::parse_cli)]
        disk: Option<config::GiB>,
        /// Skip injecting Claude Code and Codex credentials/config into the VM
        #[arg(long, alias = "no-claude")]
        no_agents: bool,
        /// Named image to use when creating a new instance (default: "default")
        #[arg(
            long,
            value_parser = config::ImageName::new,
            add = ArgValueCandidates::new(completions::image_candidates),
        )]
        image: Option<config::ImageName>,
        /// Build or reuse a profile-derived image when creating a new instance
        #[arg(
            long,
            value_delimiter = ',',
            add = ArgValueCandidates::new(completions::profile_candidates),
        )]
        profile: Vec<String>,
        /// Skip `.git/` when copying/syncing local directories
        #[arg(long)]
        exclude_git: bool,
        /// Suppress the interactive prompt to set up a scoped GitHub PAT
        /// when one is missing for the resolved repo.
        #[arg(long)]
        no_prompt: bool,
        /// Forward a guest port to the host (`GUEST[:HOST]`, repeatable).
        #[arg(long, value_parser = config::PortForward::parse)]
        forward_port: Vec<config::PortForward>,
        /// Shell command to run inside the guest after boot (overrides
        /// `post_start` from `config.toml`). Failure is logged but does
        /// not fail the start.
        #[arg(long, value_name = "CMD")]
        post_start: Option<String>,
        /// Literal env var to set in the guest (`KEY=VALUE`, repeatable).
        /// Overrides `guest_env` entries from config and any forwarded
        /// values with the same name.
        #[arg(
            long = "env",
            value_name = "KEY=VALUE",
            value_parser = guest_env_state::parse_cli_env_arg,
        )]
        guest_env: Vec<(guest_env_state::EnvVarName, String)>,
        /// Explicit path to a `devcontainer.json` to use (skips discovery).
        #[arg(long, value_name = "PATH", conflicts_with = "no_devcontainer")]
        devcontainer: Option<PathBuf>,
        /// Ignore any discovered `devcontainer.json` (escape hatch for CI).
        #[arg(long)]
        no_devcontainer: bool,
        /// Translate `devcontainer.json` and print the report, then exit
        /// before any VM work.
        #[arg(long)]
        dry_run: bool,
    },
    /// One-shot: ensure default image, start an instance for cwd, launch Claude.
    ///
    /// Re-runnable: if an instance already exists for the current workspace it
    /// is reconnected (running) or restarted (stopped) instead of being
    /// recreated. For per-instance tuning (profiles, mounts, image, ...) use
    /// `coop setup` / `coop up` directly.
    Quickstart {
        /// Skip mounting the current directory as the workspace.
        #[arg(long)]
        no_workspace: bool,
        /// Ignore any discovered `devcontainer.json` (escape hatch for CI).
        #[arg(long)]
        no_devcontainer: bool,
    },
    /// Check prerequisites, install Firecracker, fetch kernel and build template rootfs
    Setup {
        /// Skip confirmation prompts (accept all)
        #[arg(short = 'y', long)]
        yes: bool,
        /// Number of vCPUs (overrides config)
        #[arg(long)]
        vcpus: Option<u8>,
        /// Memory in MiB (overrides config)
        #[arg(long, value_parser = config::MiB::parse_cli)]
        mem: Option<config::MiB>,
        /// Force rebuild of template rootfs
        #[arg(long)]
        rebuild: bool,
        /// Install profiles (comma-separated: python,node,c,fuzz,rust,go)
        #[arg(
            long,
            value_delimiter = ',',
            add = ArgValueCandidates::new(completions::profile_candidates),
        )]
        profile: Vec<String>,
        /// Extra apt packages to install (comma-separated)
        #[arg(long, value_delimiter = ',')]
        extra_packages: Vec<String>,
        /// Path to a post-install script to run in the chroot
        #[arg(long)]
        post_install: Option<String>,
        /// Template rootfs size in GiB (default: 8)
        #[arg(long, value_parser = config::GiB::parse_cli)]
        template_size: Option<config::GiB>,
        /// Named image to build (default: "default")
        #[arg(
            long,
            default_value = config::DEFAULT_IMAGE,
            value_parser = config::ImageName::new,
            add = ArgValueCandidates::new(completions::image_candidates),
        )]
        image: config::ImageName,
        /// Guest username to create in the image (default: "ubuntu").
        /// Baked into the image at setup time and immutable for its
        /// lifetime — `start`/`shell`/`exec` read it from the image's
        /// `template_config.json`. Use this when a workspace's
        /// `devcontainer.json` declares a `remoteUser` other than
        /// `ubuntu` (e.g. `vscode` for the Microsoft devcontainer base
        /// images).
        #[arg(long, value_name = "NAME", value_parser = guest::GuestUser::parse)]
        guest_user: Option<guest::GuestUser>,
        /// Workspace directory to scan for `.devcontainer/devcontainer.json`.
        /// When present (and `--no-devcontainer` is not set), coop offers to
        /// apply the file's `features` and `hostRequirements` to this setup.
        #[arg(long)]
        workspace: Option<String>,
        /// Explicit path to a `devcontainer.json` to use (skips discovery).
        #[arg(long, value_name = "PATH", conflicts_with = "no_devcontainer")]
        devcontainer: Option<PathBuf>,
        /// Ignore any discovered `devcontainer.json` (escape hatch for CI).
        #[arg(long)]
        no_devcontainer: bool,
        /// Translate `devcontainer.json` and print the report, then exit
        /// before doing any setup work.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect devcontainer.json support without starting setup or a VM
    Devcontainer {
        #[command(subcommand)]
        command: DevcontainerCommands,
    },
    /// Restart a stopped VM
    Start {
        /// Stopped instance name (optional only when exactly one stopped instance exists)
        #[arg(value_parser = config::InstanceName::new)]
        name: Option<config::InstanceName>,
        /// Project directory used to select an associated stopped instance
        #[arg(long)]
        workspace: Option<String>,
        /// Skip injecting Claude Code and Codex credentials/config into the VM
        #[arg(long, alias = "no-claude")]
        no_agents: bool,
        /// Forward a guest port to the host (`GUEST[:HOST]`, repeatable).
        /// `--forward-port 3000` forwards guest 3000 to host 3000;
        /// `--forward-port 3000:3001` forwards guest 3000 to host 3001.
        #[arg(long, value_parser = config::PortForward::parse)]
        forward_port: Vec<config::PortForward>,
        /// Suppress the interactive prompt to set up a scoped GitHub PAT
        /// when one is missing for the resolved repo.
        #[arg(long)]
        no_prompt: bool,
        /// Shell command to run inside the guest after boot (overrides
        /// `post_start` from `config.toml`). Failure is logged but does
        /// not fail the start.
        #[arg(long, value_name = "CMD")]
        post_start: Option<String>,
        /// Literal env var to set in the guest (`KEY=VALUE`, repeatable).
        /// Overrides `guest_env` entries from config and any forwarded
        /// values with the same name.
        #[arg(
            long = "env",
            value_name = "KEY=VALUE",
            value_parser = guest_env_state::parse_cli_env_arg,
        )]
        guest_env: Vec<(guest_env_state::EnvVarName, String)>,
        /// Explicit path to a `devcontainer.json` to use (skips discovery).
        #[arg(long, value_name = "PATH", conflicts_with = "no_devcontainer")]
        devcontainer: Option<PathBuf>,
        /// Ignore any discovered `devcontainer.json` (escape hatch for CI).
        #[arg(long)]
        no_devcontainer: bool,
        /// Translate `devcontainer.json` and print the report, then exit
        /// before doing any VM work.
        #[arg(long)]
        dry_run: bool,
    },
    /// Open an interactive shell in the VM (or run a command non-interactively)
    #[command(alias = "ssh")]
    Shell {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Command to run (non-interactive, no PTY)
        #[arg(allow_hyphen_values = true, last = true)]
        command: Vec<String>,
    },
    /// Launch Claude Code inside the VM (skips permissions by default)
    Claude {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Prompt for permissions instead of skipping them
        #[arg(long)]
        ask: bool,
        /// Extra arguments passed to `claude`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Open the Claude Code agent view inside the VM (`claude agents`)
    #[command(
        alias = "ca",
        after_help = "If the remote TUI stops responding, type Enter, then ~. to disconnect. If your terminal remains broken afterward, run `stty sane`."
    )]
    ClaudeAgents {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Extra arguments passed to `claude agents`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Launch Codex inside the VM
    Codex {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Extra arguments passed to `codex`
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Gracefully stop the VM
    Stop {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
    },
    /// Stop and clean up instance resources (keeps template)
    Destroy {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Also remove template, kernel, and Firecracker binary
        #[arg(long)]
        all: bool,
    },
    /// List instances by name and state
    #[command(alias = "ls")]
    List,
    /// Show VM status
    Status {
        /// Instance name (shows all if omitted)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
    },
    /// Stream VM serial console logs
    Logs {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Push local workspace into the running VM
    Push {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Local directory to push (defaults to `workspace.json` `host_path`)
        #[arg(long)]
        dir: Option<String>,
        /// Overwrite guest changes without confirmation
        #[arg(long)]
        force: bool,
        /// Skip the `.git` directory in this transfer
        #[arg(long)]
        exclude_git: bool,
    },
    /// Pull guest workspace to local directory
    Pull {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Local directory to pull into (defaults to `workspace.json` `host_path`)
        #[arg(long)]
        dir: Option<String>,
        /// Overwrite local changes without confirmation
        #[arg(long)]
        force: bool,
        /// Skip the `.git` directory in this transfer
        #[arg(long)]
        exclude_git: bool,
    },
    /// Run a command in the VM and return its output (non-interactive)
    ///
    /// The command and its arguments must follow `--` to avoid conflicting
    /// with the optional instance name positional, e.g.
    /// `coop exec my-vm -- ls -la` or `coop exec -- ls -la`.
    Exec {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Command and arguments to run (after `--`)
        #[arg(required = true, last = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Open VS Code connected to the guest VM
    Vscode {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
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
    /// Install a `coop-<name>` SSH alias for ad-hoc ssh/scp/rsync
    SshConfig {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Remove the SSH config entry for this instance and exit
        #[arg(long)]
        clean: bool,
    },
    /// List or manage golden images
    Images {
        /// Delete a named image
        #[arg(
            long,
            value_parser = config::ImageName::new,
            add = ArgValueCandidates::new(completions::image_candidates),
        )]
        delete: Option<config::ImageName>,
    },
    /// Resize a stopped instance's disk
    Resize {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// New size: absolute GiB (e.g. 150, 150G) or relative (e.g. +20, +20G)
        #[arg(long, required = true, value_parser = config::DiskSize::parse)]
        size: config::DiskSize,
    },
    /// List or inspect available profiles
    Profiles {
        #[command(subcommand)]
        action: Option<ProfilesAction>,
    },
    /// Manage GitHub authentication (fine-grained PAT wizard, status, rotate, forget)
    Github {
        #[command(subcommand)]
        action: GithubAction,
    },
    /// Validate configuration and check prerequisites
    Validate {
        /// Probe live state for each `[github.pat]` entry (talks to api.github.com)
        #[arg(long)]
        probe: bool,
    },
    /// Generate a starter config file at ~/.coop/config.toml
    Init,
    /// Replace the running coop binary with the latest GitHub release
    Update {
        /// Only check for an available update — do not download or install
        #[arg(long)]
        check: bool,
        /// Reinstall even if the current binary is already up to date
        #[arg(long)]
        force: bool,
        /// Install a specific version (e.g. `v0.3.2` or `0.3.2`)
        #[arg(long = "version", value_name = "VERSION")]
        target_version: Option<String>,
        /// Skip the interactive confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Remove the coop binary and (optionally) its data directories
    Uninstall {
        /// Skip interactive confirmation prompts (removes data unless --keep-data)
        #[arg(short = 'y', long)]
        yes: bool,
        /// Remove only the binary, keep ~/.coop and update-check state
        #[arg(long, conflicts_with = "purge")]
        keep_data: bool,
        /// Also remove data without prompting (pairs with --yes for CI)
        #[arg(long, conflicts_with = "keep_data")]
        purge: bool,
    },
    /// Print a shell completion script (run `coop completions --help` for setup)
    #[command(after_help = COMPLETIONS_AFTER_HELP)]
    Completions {
        /// Shell to generate completions for
        shell: clap_complete::Shell,
    },
}

const COMPLETIONS_AFTER_HELP: &str = "\
Examples:
  # bash — system-wide
  coop completions bash | sudo tee /etc/bash_completion.d/coop > /dev/null

  # bash — user
  coop completions bash > ~/.local/share/bash-completion/completions/coop

  # zsh — user (ensure dir is on $fpath; restart shell)
  coop completions zsh > ~/.zfunc/_coop

  # fish — user
  coop completions fish > ~/.config/fish/completions/coop.fish

Dynamic completion (live instance / image / profile names) requires one
extra line in your shell rc:

  bash:  source <(COMPLETE=bash coop)
  zsh:   source <(COMPLETE=zsh coop)
  fish:  source (COMPLETE=fish coop | psub)
";

#[derive(Subcommand)]
enum ProfilesAction {
    /// List all available profiles (builtin and custom)
    List,
    /// Show the full definition of a profile
    Show {
        /// Profile name to inspect
        #[arg(add = ArgValueCandidates::new(completions::profile_candidates))]
        name: String,
    },
}

#[derive(Subcommand)]
enum GithubAction {
    /// Run the fine-grained PAT wizard for a repo
    SetupPat {
        /// Repo slug to scope to (auto-detected if omitted)
        #[arg(long, value_parser = github_repo::RepoSlug::parse_cli)]
        repo: Option<github_repo::RepoSlug>,
    },
    /// Re-run the wizard against an existing entry
    RotatePat {
        /// Repo slug to rotate
        #[arg(long, required = true, value_parser = github_repo::RepoSlug::parse_cli)]
        repo: github_repo::RepoSlug,
    },
    /// Print configured PAT entries and their validation state
    Status {
        /// Resolve each entry's `cmd:` invocation (may trigger Keychain /
        /// 1Password prompts) to confirm the secret store still serves it.
        #[arg(long)]
        probe: bool,
    },
    /// Remove a configured PAT entry and its stored secret
    ForgetPat {
        /// Repo slug whose entry should be removed
        #[arg(long, required = true, value_parser = github_repo::RepoSlug::parse_cli)]
        repo: github_repo::RepoSlug,
    },
}

#[derive(Subcommand)]
enum DevcontainerCommands {
    /// Parse devcontainer.json and print coop's translation report.
    Check {
        /// Path to the devcontainer.json file to inspect
        path: PathBuf,
        /// Which lifecycle translation to report
        #[arg(long, value_enum, default_value_t = DevcontainerCheckStage::Both)]
        stage: DevcontainerCheckStage,
    },
    /// Persistently ignore discovered devcontainer.json for a project.
    Ignore {
        /// Project directory whose discovered devcontainer.json should be ignored
        project: PathBuf,
    },
    /// Show persistent devcontainer opt-outs.
    Status {
        /// Project directory to inspect; omitted lists every stored opt-out
        project: Option<PathBuf>,
    },
    /// Clear a persistent devcontainer opt-out for a project.
    Clear {
        /// Project directory whose opt-out should be cleared
        project: PathBuf,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum DevcontainerCheckStage {
    /// Report setup-time keys such as features, hostRequirements, and remoteUser
    Setup,
    /// Report start-time keys such as postStartCommand, containerEnv, ports, and mounts
    Start,
    /// Report both setup and start translations
    Both,
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

/// Returns true if the raw argv contains the deprecated `--no-claude`
/// alias. Clap rewrites the alias to `--no-agents` before the `Start`
/// variant is matched, so we inspect the raw args to emit a one-time
/// deprecation warning.
fn raw_args_use_deprecated_no_claude<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .any(|a| a.as_ref() == "--no-claude" || a.as_ref().starts_with("--no-claude="))
}

/// Parse CLI arguments and dispatch to the matching command.
///
/// # Errors
///
/// Returns any error surfaced by the dispatched command — config load
/// failures, backend/VM operations, SSH, network, and filesystem errors all
/// bubble up here for `main` to report.
#[expect(clippy::too_many_lines, reason = "CLI dispatch — flat match arms")]
pub fn run() -> Result<()> {
    // Dynamic shell completion: when invoked with COMPLETE=<shell>, compute
    // candidates and exit before doing anything else (no tracing init, no
    // config load — completion is expected to be cheap).
    clap_complete::CompleteEnv::with_factory(<Cli as clap::CommandFactory>::command).complete();

    let cli = Cli::parse();
    init_tracing(cli.verbose);

    if let Commands::Completions { shell } = cli.command {
        completions::emit_static(shell);
        return Ok(());
    }
    if matches!(cli.command, Commands::Init) {
        return cmd_init(&cli.config);
    }
    if let Commands::Update {
        check,
        force,
        target_version,
        yes,
    } = cli.command
    {
        return update::run(&update::UpdateOpts {
            check_only: check,
            force,
            pinned_version: target_version,
            skip_confirm: yes,
        });
    }
    if let Commands::Uninstall {
        yes,
        keep_data,
        purge,
    } = cli.command
    {
        // Best-effort config load — uninstall must work even on a half-installed
        // or partially-corrupted system. `load` already tolerates a missing file;
        // a parse failure means we fall back to defaults, which can mask a
        // custom `data_dir` — warn so the user can re-run with --keep-data.
        let cfg = match config::CoopConfig::load(&cli.config) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse {} ({e}); using defaults. \
                     A custom data_dir may not be honoured — re-run with \
                     --keep-data if your data lives outside {}.",
                    cli.config.display(),
                    config::CoopConfig::default().data_dir.display(),
                );
                config::CoopConfig::default()
            }
        };
        let be: backend::PlatformBackend = backend::PlatformBackend::new();
        return cmd_uninstall(
            &be,
            &cfg,
            &cli.config,
            &UninstallOpts {
                yes,
                keep_data,
                purge,
            },
        );
    }

    if let Commands::Devcontainer { ref command } = cli.command
        && matches!(command, DevcontainerCommands::Check { .. })
    {
        return cmd_devcontainer_check(command);
    }

    let mut cfg = config::CoopConfig::load(&cli.config)?;
    update::maybe_print_notify(&cfg.updates);
    update::maybe_run_background_check(&cfg.updates);
    let be: backend::PlatformBackend = backend::PlatformBackend::new();
    tracing::debug!("Using backend: {be}");

    let raw_args: Vec<String> = std::env::args().collect();
    match cli.command {
        Commands::Up {
            dir,
            name,
            copy,
            mount,
            extra_mount,
            git_repo,
            vcpus,
            mem,
            disk,
            no_agents,
            image,
            profile,
            exclude_git,
            no_prompt,
            forward_port,
            post_start,
            guest_env,
            devcontainer,
            no_devcontainer,
            dry_run,
        } => {
            let validated = cfg.validate_and_warn()?;
            let transport = match (copy, mount) {
                (_, true) => ProjectTransport::Mount,
                (_, false) => ProjectTransport::Copy,
            };
            let profile_target = if profile.is_empty() {
                None
            } else {
                if let Some(image) = &image {
                    bail!(
                        "`coop up --profile` derives the image name from the sorted profile list; \
                         use `coop setup --image {image} --profile ...` and then `coop up --image {image}` \
                         for an explicit named image."
                    );
                }
                Some(ProfileImageTarget::new(&profile)?)
            };
            let opts = UpOpts {
                dir: dir.as_deref(),
                name: name.as_ref(),
                transport,
                extra_mount,
                git_repo: git_repo.as_deref(),
                vcpus,
                mem,
                disk,
                image,
                profile_target,
                runtime: UpRuntimeOpts {
                    no_agents,
                    exclude_git,
                    no_prompt,
                    forward_ports: forward_port,
                    post_start,
                    guest_env,
                },
                devcontainer: UpDevcontainerOpts {
                    input: DevcontainerInput::from_flags(devcontainer, no_devcontainer),
                    dry_run,
                },
            };
            cmd_up(&be, &mut cfg, &validated, &cli.config, &opts)
        }
        Commands::Quickstart {
            no_workspace,
            no_devcontainer,
        } => cmd_quickstart(
            &be,
            &mut cfg,
            &cli.config,
            &QuickstartOpts {
                no_workspace,
                no_devcontainer,
            },
        ),
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
            guest_user,
            workspace,
            devcontainer,
            no_devcontainer,
            dry_run,
        } => {
            let validated = cfg.validate_and_warn()?;
            let ws_path = workspace.as_deref().map(Path::new);
            let inputs = devcontainer::TranslatorInputs {
                cli_vcpus: vcpus,
                cli_mem_mib: mem,
                cli_profiles: profile.clone(),
                cli_guest_user: guest_user.clone(),
                ..devcontainer::TranslatorInputs::default()
            };
            let dc_input = DevcontainerInput::from_flags(devcontainer, no_devcontainer);
            let translation = resolve_devcontainer(
                &DevcontainerOpts {
                    input: &dc_input,
                    dry_run,
                    workspace: ws_path,
                    mounts: &[],
                    git_repo: None,
                    github_auth: cfg.github.as_ref(),
                    preference_path: Some(&cfg.devcontainer_preferences_path()),
                },
                &inputs,
                devcontainer::Stage::Setup,
            )?;
            if dry_run {
                return Ok(());
            }
            let mut profile = profile;
            if let Some(t) = &translation {
                for p in &t.profiles {
                    if !profile.contains(p) {
                        profile.push(p.clone());
                    }
                }
            }
            // CLI flags first; translation values then fill in any blanks.
            apply_vm_overrides(&mut cfg, vcpus, mem, template_size)?;
            if let Some(t) = &translation {
                devcontainer::apply_to_config(&mut cfg, t)?;
            }
            // Resolve profile names once at the CLI boundary.
            let resolved_profiles = guest::resolve_profiles(&profile, &cfg.profiles)?;
            // CLI wins; otherwise honour the devcontainer's `remoteUser`;
            // otherwise default to `ubuntu`.
            let resolved_guest_user = guest_user
                .or_else(|| translation.as_ref().and_then(|t| t.guest_user.clone()))
                .unwrap_or_default();
            let _guard = signal::install_handlers();
            be.setup(
                &cfg,
                &validated,
                &setup::SetupOptions {
                    skip_confirm: yes,
                    rebuild,
                    profiles: resolved_profiles,
                    oci_features: translation
                        .as_ref()
                        .map(|t| t.oci_features.clone())
                        .unwrap_or_default(),
                    extra_packages,
                    post_install: post_install.map(PathBuf::from),
                    image,
                    guest_user: resolved_guest_user,
                },
            )
        }
        Commands::Devcontainer { command } => cmd_devcontainer(&cfg, &command),
        Commands::Start {
            name,
            workspace,
            no_agents,
            no_prompt,
            post_start,
            guest_env,
            forward_port: forward_ports,
            devcontainer,
            no_devcontainer,
            dry_run,
        } => {
            let validated = cfg.validate_and_warn()?;
            if raw_args_use_deprecated_no_claude(&raw_args) {
                tracing::warn!(
                    "--no-claude is deprecated and will be removed in a future release; use --no-agents"
                );
            }
            if dry_run {
                let cli_env_keys = guest_env.iter().map(|(k, _)| k.clone()).collect();
                let dry_run_image = config::default_image_name();
                let inputs = devcontainer::TranslatorInputs {
                    cli_post_start: post_start.clone(),
                    cli_guest_env_keys: cli_env_keys,
                    cli_forward_ports: forward_ports.clone(),
                    persisted_guest_user: Some(backend::persisted_guest_user(&cfg, &dry_run_image)),
                    cli_workspace_or_git_repo: workspace.is_some(),
                    ..devcontainer::TranslatorInputs::default()
                };
                let ws_path = workspace.as_deref().map(Path::new);
                let dc_input = DevcontainerInput::from_flags(devcontainer.clone(), no_devcontainer);
                let _ = resolve_devcontainer(
                    &DevcontainerOpts {
                        input: &dc_input,
                        dry_run,
                        workspace: ws_path,
                        mounts: &[],
                        git_repo: None,
                        github_auth: cfg.github.as_ref(),
                        preference_path: Some(&cfg.devcontainer_preferences_path()),
                    },
                    &inputs,
                    devcontainer::Stage::Start,
                )?;
                return Ok(());
            }

            let mut start_opts = StartOpts {
                name: name.as_ref(),
                workspace_dir: workspace.as_deref(),
                git_repo: None,
                no_agents,
                no_prompt,
                disk: None,
                mounts: Vec::new(),
                exclude_git: false,
                forward_ports,
                config_path: &cli.config,
                post_start_override: post_start.as_deref(),
                persisted_guest_env: std::collections::BTreeMap::new(),
                devcontainer_path: devcontainer.as_deref(),
                applied_devcontainer: None,
            };
            preflight_start_target(&be, &cfg, &start_opts)?;
            apply_runtime_guest_env(&mut cfg, &guest_env, None, &mut start_opts);
            cmd_start(&be, &mut cfg, &validated, &start_opts).map(|_| ())
        }
        Commands::Shell { name, command } => cmd_shell(&be, &cfg, name.as_ref(), &command),
        Commands::Claude {
            name,
            ask,
            mut args,
        } => {
            let sess = open_ssh_session(&be, &cfg, name.as_ref())?;
            // Guest user settings set `defaultMode: bypassPermissions`. Opting in
            // to prompts means overriding that default explicitly.
            if ask {
                args.insert(0, "default".to_string());
                args.insert(0, "--permission-mode".to_string());
            }
            let claude_bin = guest::GuestUser::new(sess.target.user.as_ref())?.claude_bin();
            ssh::run_interactive(&sess, &prepend_binary(claude_bin.as_ref(), args))
        }
        Commands::ClaudeAgents { name, mut args } => {
            let sess = open_ssh_session(&be, &cfg, name.as_ref())?;
            args.insert(0, "agents".to_string());
            let claude_bin = guest::GuestUser::new(sess.target.user.as_ref())?.claude_bin();
            ssh::run_interactive(&sess, &prepend_binary(claude_bin.as_ref(), args))
        }
        Commands::Codex { name, args } => {
            let sess = open_ssh_session(&be, &cfg, name.as_ref())?;
            ssh::run_interactive(&sess, &prepend_binary(guest::codex_bin().as_ref(), args))
        }
        Commands::Stop { name } => {
            let inst = cfg.resolve_instance(name.as_ref())?;
            cmd_stop(&be, &cfg, &inst)
        }
        Commands::Destroy { name, all } => {
            let _guard = signal::install_handlers();
            cmd_destroy(&be, &cfg, name.as_ref(), all)
        }
        Commands::List => cmd_list(&be, &cfg),
        Commands::Status { name } => cmd_status(&be, &cfg, name.as_ref()),
        Commands::Logs { name, follow } => {
            let running = resolve_running(&be, &cfg, name.as_ref())?;
            let mode = if follow {
                backend::LogMode::Follow
            } else {
                backend::LogMode::Snapshot
            };
            be.stream_logs(&cfg, &running, mode)
        }
        Commands::Push {
            name,
            dir,
            force,
            exclude_git,
        } => {
            let running = resolve_running(&be, &cfg, name.as_ref())?;
            workspace::push(&running, dir.as_deref(), force, exclude_git)
        }
        Commands::Pull {
            name,
            dir,
            force,
            exclude_git,
        } => {
            let running = resolve_running(&be, &cfg, name.as_ref())?;
            workspace::pull(&running, dir.as_deref(), force, exclude_git)
        }
        Commands::Exec { name, command } => cmd_exec(&be, &cfg, name.as_ref(), &command),
        Commands::Vscode {
            name,
            project,
            editor,
            clean,
        } => {
            if clean {
                let inst = cfg.resolve_instance(name.as_ref())?;
                workspace::remove_ssh_config(&inst)?;
                tracing::info!("Removed SSH config for '{}'", inst.name);
                return Ok(());
            }
            let running = resolve_running(&be, &cfg, name.as_ref())?;
            workspace::vscode(&running, Some(&project), editor.as_deref())
        }
        Commands::SshConfig { name, clean } => {
            if clean {
                let inst = cfg.resolve_instance(name.as_ref())?;
                workspace::remove_ssh_config(&inst)?;
                tracing::info!("Removed SSH config for '{}'", inst.name);
                return Ok(());
            }
            let running = resolve_running(&be, &cfg, name.as_ref())?;
            workspace::write_ssh_config(&running)
        }
        Commands::Images { delete } => cmd_images(&be, &cfg, delete.as_ref()),
        Commands::Resize { name, size } => cmd_resize(&be, &cfg, name.as_ref(), size),
        Commands::Profiles { action } => {
            cmd_profiles(&cfg, &action.unwrap_or(ProfilesAction::List))
        }
        Commands::Github { action } => cmd_github(&cfg, &cli.config, action),
        Commands::Validate { probe } => cmd_validate(&cfg, &be, probe),
        Commands::Init
        | Commands::Update { .. }
        | Commands::Uninstall { .. }
        | Commands::Completions { .. } => {
            unreachable!("handled before config loading")
        }
    }
}

fn cmd_github(cfg: &config::CoopConfig, config_path: &Path, action: GithubAction) -> Result<()> {
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

fn cmd_validate(
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

struct QuickstartOpts {
    no_workspace: bool,
    no_devcontainer: bool,
}

struct UpOpts<'a> {
    dir: Option<&'a str>,
    name: Option<&'a config::InstanceName>,
    transport: ProjectTransport,
    extra_mount: Vec<config::Mount>,
    git_repo: Option<&'a str>,
    vcpus: Option<u8>,
    mem: Option<config::MiB>,
    disk: Option<config::GiB>,
    /// Explicit `--image NAME`, or `None` when unset. `None` selects the
    /// profile-derived image when `--profile` is given, else the default.
    image: Option<config::ImageName>,
    profile_target: Option<ProfileImageTarget>,
    runtime: UpRuntimeOpts,
    devcontainer: UpDevcontainerOpts,
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

struct UpRuntimeOpts {
    no_agents: bool,
    exclude_git: bool,
    no_prompt: bool,
    forward_ports: Vec<config::PortForward>,
    post_start: Option<String>,
    guest_env: Vec<(guest_env_state::EnvVarName, String)>,
}

struct UpDevcontainerOpts {
    input: DevcontainerInput,
    dry_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectTransport {
    Copy,
    Mount,
}

/// Project-oriented start: ensure DIR has a single matching environment.
///
/// Unlike `start`, `up` treats the project directory as identity and keeps
/// transport explicit. Existing instances are found by their recorded
/// `workspace.json` host path; creation-only inputs are only applied when no
/// matching instance exists.
fn cmd_up(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    validated: &config::Validated,
    config_path: &Path,
    opts: &UpOpts<'_>,
) -> Result<()> {
    if let Some(repo_url) = opts.git_repo {
        return cmd_up_git_repo(be, cfg, validated, config_path, opts, repo_url);
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
        let inputs = up_translator_inputs(cfg, opts);
        resolve_devcontainer(
            &DevcontainerOpts {
                input: &opts.devcontainer.input,
                dry_run: true,
                workspace: Some(&project_dir),
                mounts: discovery_mounts,
                git_repo: None,
                github_auth: cfg.github.as_ref(),
                preference_path: Some(&cfg.devcontainer_preferences_path()),
            },
            &inputs,
            devcontainer::Stage::Start,
        )?;
        return Ok(());
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

    ensure_profile_image(be, cfg, validated, opts.profile_target.as_ref())?;

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
    validated: &config::Validated,
    config_path: &Path,
    opts: &UpOpts<'_>,
    repo_url: &str,
) -> Result<()> {
    if opts.devcontainer.dry_run {
        let inputs = up_translator_inputs(cfg, opts);
        resolve_devcontainer(
            &DevcontainerOpts {
                input: &opts.devcontainer.input,
                dry_run: true,
                workspace: None,
                mounts: &[],
                git_repo: Some(repo_url),
                github_auth: cfg.github.as_ref(),
                preference_path: Some(&cfg.devcontainer_preferences_path()),
            },
            &inputs,
            devcontainer::Stage::Start,
        )?;
        return Ok(());
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

    ensure_profile_image(be, cfg, validated, opts.profile_target.as_ref())?;
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

    let workspace_dir = match opts.transport {
        ProjectTransport::Copy => Some(project_dir_to_str(project_dir)?),
        ProjectTransport::Mount => {
            mounts.insert(0, project_mount.clone());
            None
        }
    };
    validate_copy_workspace_mounts(opts.transport, &mounts)?;
    validate_unique_guest_paths(&mounts)?;

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
    validate_git_repo_workspace_mounts(&mounts)?;
    validate_unique_guest_paths(&mounts)?;

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
        .or_else(|| git_repo_default_instance_name(repo_url));
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
    validated: &config::Validated,
    target: Option<&ProfileImageTarget>,
) -> Result<()> {
    let Some(target) = target else {
        return Ok(());
    };
    let resolved_profiles = guest::resolve_profiles(&target.profiles, &cfg.profiles)?;
    let _guard = signal::install_handlers();
    be.setup(
        cfg,
        validated,
        &setup::SetupOptions {
            skip_confirm: true,
            rebuild: false,
            profiles: resolved_profiles,
            oci_features: Vec::new(),
            extra_packages: Vec::new(),
            post_install: None,
            image: target.image.clone(),
            guest_user: guest::GuestUser::default(),
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

/// Merge guest-env entries by precedence (CLI > devcontainer.json > config.toml),
/// persist the result into `cfg.guest_env`, and return the merged map.
///
/// This is the single implementation of the precedence rule. All call sites —
/// the `start_opts`-mutating [`apply_runtime_guest_env`] and the struct-literal
/// construction paths — route through here.
fn merge_runtime_guest_env(
    cfg: &mut config::CoopConfig,
    cli_guest_env: &[(guest_env_state::EnvVarName, String)],
    dc_guest_env: Option<&std::collections::BTreeMap<guest_env_state::EnvVarName, String>>,
) -> std::collections::BTreeMap<guest_env_state::EnvVarName, String> {
    let cli_guest_env: std::collections::BTreeMap<_, _> = cli_guest_env.iter().cloned().collect();
    let dc_guest_env = dc_guest_env.cloned().unwrap_or_default();
    let persisted_guest_env =
        guest_env_state::merge_persisted_entries(&dc_guest_env, &cli_guest_env);
    for (key, value) in &persisted_guest_env {
        cfg.guest_env.insert(key.clone(), value.clone());
    }
    persisted_guest_env
}

fn apply_runtime_guest_env(
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
             Use `coop destroy {}` first to recreate it with those options.",
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
             Use `coop destroy {}` first to recreate it with those options.",
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

fn validate_unique_guest_paths(mounts: &[config::Mount]) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for mount in mounts {
        let guest_path = mount.guest_path.to_string();
        if !seen.insert(guest_path.clone()) {
            bail!("Duplicate mount guest path: {guest_path}");
        }
    }
    Ok(())
}

fn validate_git_repo_workspace_mounts(mounts: &[config::Mount]) -> Result<()> {
    let workspace_path = workspace::default_workspace_path().to_string();
    if mounts
        .iter()
        .any(|mount| mount.guest_path.to_string() == workspace_path)
    {
        bail!(
            "`coop up --git-repo` clones into /workspace. \
             Give --extra-mount an explicit non-/workspace guest path."
        );
    }
    Ok(())
}

fn validate_copy_workspace_mounts(
    transport: ProjectTransport,
    mounts: &[config::Mount],
) -> Result<()> {
    if transport != ProjectTransport::Copy {
        return Ok(());
    }
    let workspace_path = workspace::default_workspace_path().to_string();
    if mounts
        .iter()
        .any(|mount| mount.guest_path.to_string() == workspace_path)
    {
        bail!(
            "`coop up --copy` already uses /workspace for the project. \
             Give --extra-mount an explicit non-/workspace guest path, or use \
             `coop up --mount` to mount the project itself."
        );
    }
    Ok(())
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
fn cmd_quickstart(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    opts: &QuickstartOpts,
) -> Result<()> {
    let validated = cfg.validate_and_warn()?;
    let image = config::default_image_name();

    if be.image_is_built(cfg, &image) {
        tracing::debug!("Image '{image}' already built — skipping setup");
    } else {
        tracing::info!("No '{image}' image found — running setup");
        let _guard = signal::install_handlers();
        be.setup(
            cfg,
            &validated,
            &setup::SetupOptions {
                skip_confirm: true,
                rebuild: false,
                profiles: Vec::new(),
                oci_features: Vec::new(),
                extra_packages: Vec::new(),
                post_install: None,
                image: image.clone(),
                guest_user: guest::GuestUser::default(),
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
                &validated,
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
            &validated,
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
fn quickstart_fresh_start(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    config_path: &Path,
    validated: &config::Validated,
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

    let final_mounts = translation
        .as_ref()
        .map(|t| t.mounts.clone())
        .unwrap_or_default();

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

    let _ = validated;
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

/// Find the (single) instance whose persisted workspace state's `host_path`
/// matches `workspace` (after canonicalisation). Returns `None` when no
/// instance has been started for this directory; bails when multiple do
/// (the caller has to pick one explicitly).
fn find_workspace_instance(
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

fn git_repo_default_instance_name(repo_url: &str) -> Option<config::InstanceName> {
    let base = github_repo::parse_repo_slug_from_url(repo_url)
        .and_then(|slug| slug.as_str().rsplit('/').next().map(ToOwned::to_owned))
        .or_else(|| {
            repo_url
                .trim_end_matches('/')
                .rsplit(['/', ':'])
                .next()
                .map(|s| s.trim_end_matches(".git").to_string())
        })?;
    let sanitized: String = base
        .chars()
        .map(|c| {
            if matches!(c, 'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        return None;
    }
    let max = 60.min(sanitized.len());
    config::InstanceName::new(&sanitized[..max]).ok()
}

fn apply_vm_overrides(
    cfg: &mut config::CoopConfig,
    vcpus: Option<u8>,
    mem: Option<config::MiB>,
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

/// Whether and how a `devcontainer.json` should be resolved.
///
/// Models the mutually exclusive `--devcontainer PATH` and `--no-devcontainer`
/// flags as a single value, so "explicit path *and* disabled" cannot be
/// represented.
enum DevcontainerInput {
    /// `--devcontainer PATH`: use this exact file, skipping discovery and the
    /// prompt.
    Explicit(PathBuf),
    /// `--no-devcontainer`: skip discovery entirely.
    Disabled,
    /// Default: discover a `devcontainer.json`, then prompt before applying.
    Discover,
}

impl DevcontainerInput {
    /// Build from the parsed `--devcontainer` / `--no-devcontainer` flags.
    ///
    /// clap enforces their mutual exclusion at parse time; should both ever
    /// arrive, `--no-devcontainer` wins (matching the opt-out's precedence).
    fn from_flags(path: Option<PathBuf>, no_devcontainer: bool) -> Self {
        match (path, no_devcontainer) {
            (_, true) => Self::Disabled,
            (Some(p), false) => Self::Explicit(p),
            (None, false) => Self::Discover,
        }
    }
}

/// CLI surface controlling devcontainer.json discovery and apply.
///
/// `input` selects an explicit file, opts out entirely, or requests
/// discovery. `dry_run` prints the report and exits before any side effects.
struct DevcontainerOpts<'a> {
    input: &'a DevcontainerInput,
    dry_run: bool,
    workspace: Option<&'a Path>,
    mounts: &'a [config::Mount],
    git_repo: Option<&'a str>,
    github_auth: Option<&'a config::GitHubAuth>,
    preference_path: Option<&'a Path>,
}

enum DevcontainerSource {
    Path(PathBuf),
    Contents {
        display_path: PathBuf,
        contents: String,
    },
}

fn discovered_local_devcontainer(opts: &DevcontainerOpts<'_>, source: &DevcontainerSource) -> bool {
    matches!(opts.input, DevcontainerInput::Discover)
        && matches!(source, DevcontainerSource::Path(_))
}

fn maybe_skip_stored_devcontainer_opt_out(
    opts: &DevcontainerOpts<'_>,
    source: &DevcontainerSource,
    display_path: &Path,
) -> Result<bool> {
    if !discovered_local_devcontainer(opts, source) {
        return Ok(false);
    }
    let (Some(preference_path), Some(project)) = (opts.preference_path, opts.workspace) else {
        return Ok(false);
    };
    let preferences = devcontainer::DevcontainerPreferences::load(preference_path)?;
    let Some(project_key) = preferences.ignored_project(project)? else {
        return Ok(false);
    };

    let mut stderr = std::io::stderr();
    writeln!(
        stderr,
        "Skipping {} because a stored devcontainer opt-out is set for project {}.\n\
         Run `coop devcontainer clear {}` to re-enable discovery, or pass --devcontainer {} to apply this file once.",
        display_path.display(),
        project_key,
        project_key,
        display_path.display(),
    )
    .context("Failed to write devcontainer opt-out message")?;
    Ok(true)
}

fn maybe_record_devcontainer_opt_out(opts: &DevcontainerOpts<'_>, project: &Path) -> Result<()> {
    let Some(preference_path) = opts.preference_path else {
        return Ok(());
    };
    if !prompt::confirm(&format!(
        "Always ignore devcontainer.json for project {}?",
        project.display()
    ))? {
        return Ok(());
    }

    let mut preferences = devcontainer::DevcontainerPreferences::load(preference_path)?;
    let project_key = preferences.set_ignored(project)?;
    preferences.save(preference_path)?;
    tracing::info!("Recorded persistent devcontainer opt-out for project {project_key}");
    Ok(())
}

/// Discover, prompt, and translate a `devcontainer.json` for the given
/// lifecycle `stage`.
///
/// Returns `None` when the user opts out, no file is found, or stdin is
/// not a TTY with no explicit flag (in which case an error is returned
/// instead — the issue forbids silently choosing). The report is always
/// emitted to stderr when a file is loaded, so the user sees every key
/// that did and didn't take effect.
#[expect(
    clippy::print_stderr,
    reason = "devcontainer report is intentional user-facing CLI output"
)]
fn resolve_devcontainer(
    opts: &DevcontainerOpts<'_>,
    inputs: &devcontainer::TranslatorInputs,
    stage: devcontainer::Stage,
) -> Result<Option<devcontainer::Translation>> {
    use std::io::IsTerminal as _;

    if matches!(opts.input, DevcontainerInput::Disabled) {
        return Ok(None);
    }

    let (source, losers) = if let DevcontainerInput::Explicit(p) = opts.input {
        (DevcontainerSource::Path(p.clone()), Vec::new())
    } else {
        let found = devcontainer::discover(opts.workspace, opts.mounts);
        if let Some((winner, losers)) = devcontainer::pick_winner(found) {
            (DevcontainerSource::Path(winner.path), losers)
        } else if let Some(repo_url) = opts.git_repo
            && let Some(remote) = git_repo_devcontainer::discover(repo_url, opts.github_auth)?
        {
            (
                DevcontainerSource::Contents {
                    display_path: remote.display_path,
                    contents: remote.contents,
                },
                Vec::new(),
            )
        } else {
            return Ok(None);
        }
    };
    let display_path = match &source {
        DevcontainerSource::Path(path) => path,
        DevcontainerSource::Contents { display_path, .. } => display_path,
    };

    if maybe_skip_stored_devcontainer_opt_out(opts, &source, display_path)? {
        return Ok(None);
    }

    // When discovery (not an explicit flag) found the file, defer to the
    // user. CI/scripted callers must pass --devcontainer or --no-devcontainer.
    if matches!(opts.input, DevcontainerInput::Discover) && !opts.dry_run {
        if !std::io::stdin().is_terminal() {
            match &source {
                DevcontainerSource::Path(_) => bail!(
                    "Found {} but stdin is not a TTY.\n\
                     Pass --devcontainer {} to apply it, or --no-devcontainer to ignore.\n\
                     coop reads a subset of devcontainer.json — see docs/devcontainer.md for the supported keys.",
                    display_path.display(),
                    display_path.display()
                ),
                DevcontainerSource::Contents { .. } => bail!(
                    "Found {} but stdin is not a TTY.\n\
                     Run interactively to confirm the remote file, pass --no-devcontainer to ignore it, \
                     or pass --devcontainer <local-path> to apply an explicit file.\n\
                     coop reads a subset of devcontainer.json — see docs/devcontainer.md for the supported keys.",
                    display_path.display()
                ),
            }
        }
        let answer = prompt::confirm_default_yes(&format!(
            "Use devcontainer.json at {}?",
            display_path.display()
        ))?;
        if !answer {
            match &source {
                DevcontainerSource::Path(_) => {
                    tracing::info!(
                        "Skipping {}. Re-run with --devcontainer {} to apply it later.",
                        display_path.display(),
                        display_path.display()
                    );
                    if let Some(project) = opts.workspace {
                        maybe_record_devcontainer_opt_out(opts, project)?;
                    }
                }
                DevcontainerSource::Contents { .. } => tracing::info!(
                    "Skipping {}. Re-run interactively to apply it later.",
                    display_path.display()
                ),
            }
            return Ok(None);
        }
    }

    let source_is_remote = matches!(source, DevcontainerSource::Contents { .. });
    let parsed = match source {
        DevcontainerSource::Path(path) => devcontainer::ParsedDevcontainer::load(&path)?,
        DevcontainerSource::Contents {
            display_path,
            contents,
        } => devcontainer::ParsedDevcontainer::from_str(display_path, &contents)?,
    };
    let mut translation = devcontainer::translate(&parsed, inputs, stage);
    if source_is_remote && let Some(applied) = &mut translation.applied {
        applied.source = devcontainer::AppliedDevcontainerSource::RemoteContents;
    }
    translation.report.ignored_paths = losers;
    resolve_oci_feature_requests(&mut translation);

    eprintln!("{}", translation.report.render());

    Ok(Some(translation))
}

fn resolve_oci_feature_requests(translation: &mut devcontainer::Translation) {
    if translation.oci_feature_requests.is_empty() {
        return;
    }
    for (request, resolved) in
        translation
            .oci_feature_requests
            .iter()
            .zip(devcontainer_oci::resolve_features(
                &translation.oci_feature_requests,
            ))
    {
        let key = format!("features.{}", request.raw_id);
        match resolved {
            Ok(feature) => {
                translation.report.push(
                    key,
                    devcontainer::ReportStatus::Applied,
                    devcontainer::ReportSource::Devcontainer,
                    feature.installed.digest.to_string(),
                    format!(
                        "OCI feature '{}' install.sh sha256 {} will run during setup",
                        feature.installed.id, feature.installed.install_script_hash
                    ),
                );
                translation.oci_features.push(feature);
            }
            Err(e) => translation.report.push(
                key,
                devcontainer::ReportStatus::Invalid,
                devcontainer::ReportSource::Devcontainer,
                request.raw_id.clone(),
                format!("failed to resolve OCI feature: {e:#}"),
            ),
        }
    }
}

#[expect(
    clippy::print_stderr,
    reason = "devcontainer check report is intentional user-facing CLI output"
)]
fn cmd_devcontainer_check(command: &DevcontainerCommands) -> Result<()> {
    match command {
        DevcontainerCommands::Check { path, stage } => {
            let dc_input = DevcontainerInput::Explicit(path.clone());
            let opts = DevcontainerOpts {
                input: &dc_input,
                dry_run: true,
                workspace: None,
                mounts: &[],
                git_repo: None,
                github_auth: None,
                preference_path: None,
            };
            let setup_inputs = devcontainer::TranslatorInputs::default();

            match stage {
                DevcontainerCheckStage::Setup => {
                    resolve_devcontainer(&opts, &setup_inputs, devcontainer::Stage::Setup)?;
                }
                DevcontainerCheckStage::Start => {
                    let start_inputs = devcontainer::TranslatorInputs {
                        persisted_guest_user: Some(guest::GuestUser::default()),
                        ..devcontainer::TranslatorInputs::default()
                    };
                    resolve_devcontainer(&opts, &start_inputs, devcontainer::Stage::Start)?;
                }
                DevcontainerCheckStage::Both => {
                    eprintln!("setup-stage translation:");
                    let setup_translation =
                        resolve_devcontainer(&opts, &setup_inputs, devcontainer::Stage::Setup)?;
                    let assumed_guest_user =
                        devcontainer_check_assumed_guest_user(setup_translation.as_ref());
                    let start_inputs = devcontainer::TranslatorInputs {
                        persisted_guest_user: Some(assumed_guest_user),
                        ..devcontainer::TranslatorInputs::default()
                    };
                    eprintln!();
                    eprintln!("start-stage translation:");
                    resolve_devcontainer(&opts, &start_inputs, devcontainer::Stage::Start)?;
                }
            }
            Ok(())
        }
        DevcontainerCommands::Ignore { .. }
        | DevcontainerCommands::Status { .. }
        | DevcontainerCommands::Clear { .. } => {
            unreachable!("devcontainer preference commands require config")
        }
    }
}

fn cmd_devcontainer(cfg: &config::CoopConfig, command: &DevcontainerCommands) -> Result<()> {
    let preference_path = cfg.devcontainer_preferences_path();
    match command {
        DevcontainerCommands::Check { .. } => cmd_devcontainer_check(command),
        DevcontainerCommands::Ignore { project } => {
            let mut preferences = devcontainer::DevcontainerPreferences::load(&preference_path)?;
            let project_key = preferences.set_ignored(project)?;
            preferences.save(&preference_path)?;
            let mut stdout = std::io::stdout();
            writeln!(
                stdout,
                "Devcontainer discovery disabled for project {project_key}"
            )
            .context("Failed to write devcontainer ignore status")?;
            Ok(())
        }
        DevcontainerCommands::Status { project } => {
            let preferences = devcontainer::DevcontainerPreferences::load(&preference_path)?;
            let mut stdout = std::io::stdout();
            if let Some(project) = project {
                if let Some(project_key) = preferences.ignored_project(project)? {
                    writeln!(
                        stdout,
                        "Devcontainer discovery disabled for project {project_key}"
                    )
                    .context("Failed to write devcontainer status")?;
                } else {
                    let project_key = devcontainer::project_preference_lookup_key(project)?;
                    writeln!(
                        stdout,
                        "Devcontainer discovery enabled for project {project_key}"
                    )
                    .context("Failed to write devcontainer status")?;
                }
            } else {
                let ignored: Vec<_> = preferences.ignored_projects().collect();
                if ignored.is_empty() {
                    writeln!(stdout, "No persistent devcontainer opt-outs recorded.")
                        .context("Failed to write devcontainer status")?;
                } else {
                    writeln!(stdout, "Persistent devcontainer opt-outs:")
                        .context("Failed to write devcontainer status")?;
                    for project in ignored {
                        writeln!(stdout, "  {project}")
                            .context("Failed to write devcontainer status")?;
                    }
                }
            }
            Ok(())
        }
        DevcontainerCommands::Clear { project } => {
            let mut preferences = devcontainer::DevcontainerPreferences::load(&preference_path)?;
            let project_key = devcontainer::project_preference_lookup_key(project)?;
            let removed = preferences.clear(project)?;
            preferences.save(&preference_path)?;
            let mut stdout = std::io::stdout();
            if removed {
                writeln!(
                    stdout,
                    "Cleared devcontainer opt-out for project {project_key}"
                )
                .context("Failed to write devcontainer clear status")?;
            } else {
                writeln!(
                    stdout,
                    "No persistent devcontainer opt-out recorded for project {project_key}"
                )
                .context("Failed to write devcontainer clear status")?;
            }
            Ok(())
        }
    }
}

fn devcontainer_check_assumed_guest_user(
    setup_translation: Option<&devcontainer::Translation>,
) -> guest::GuestUser {
    setup_translation
        .and_then(|t| t.guest_user.clone())
        .unwrap_or_default()
}

struct ProfileImageTarget {
    profiles: Vec<String>,
    image: config::ImageName,
}

impl ProfileImageTarget {
    fn new(profiles: &[String]) -> Result<Self> {
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

struct StartOpts<'a> {
    name: Option<&'a config::InstanceName>,
    workspace_dir: Option<&'a str>,
    git_repo: Option<&'a str>,
    no_agents: bool,
    /// Skip the interactive PAT auto-prompt unconditionally.
    no_prompt: bool,
    disk: Option<config::GiB>,
    mounts: Vec<config::Mount>,
    exclude_git: bool,
    /// Per-start forwards from `--forward-port`. Merged with
    /// `cfg.forward_ports` at start time (CLI overrides on guest-port
    /// collision).
    forward_ports: Vec<config::PortForward>,
    /// Path to the on-disk config file. Re-read after the auto-prompt
    /// in case the wizard added a new `[github.pat."..."]` entry.
    config_path: &'a Path,
    /// CLI override for `post_start` from `config.toml`. `None` means
    /// "use the configured value (if any)"; `Some` always wins.
    post_start_override: Option<&'a str>,
    persisted_guest_env: std::collections::BTreeMap<guest_env_state::EnvVarName, String>,
    /// Explicit `--devcontainer` is creation-only. `start --dry-run` handles
    /// translation before this struct is built; normal `start` only uses this
    /// marker to reject silently ignored creation options on restart.
    devcontainer_path: Option<&'a Path>,
    /// Devcontainer path/hash that was applied to a newly-created instance.
    /// Empty on restarts and when no devcontainer was used.
    applied_devcontainer: Option<devcontainer::AppliedDevcontainer>,
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

fn preflight_start_target(
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

fn cmd_start(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    _: &config::Validated,
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
fn allocate_and_start(
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

    let post_start = opts.post_start_override.or(cfg.post_start.as_deref());
    if opts.no_agents && post_start.is_none() {
        tracing::info!("Skipping guest agent bootstrap (--no-agents)");
    } else {
        let session = prepare_session_from_target(cfg, None, target.clone(), repo.as_ref())?;
        if opts.no_agents {
            tracing::info!("Skipping guest agent bootstrap (--no-agents)");
        } else {
            backend::bootstrap_agents(&session, cfg, inst, backend::BootMode::Restart)?;
        }
        if let Some(cmd) = post_start {
            backend::run_post_start(&session, cmd);
        }
    }

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

    let post_start = opts.post_start_override.or(cfg.post_start.as_deref());
    if opts.no_agents && post_start.is_none() {
        tracing::info!("Skipping guest agent bootstrap (--no-agents)");
    } else {
        let session = prepare_session_from_target(cfg, None, target.clone(), repo.as_ref())?;
        if opts.no_agents {
            tracing::info!("Skipping guest agent bootstrap (--no-agents)");
        } else {
            backend::bootstrap_agents(&session, cfg, inst, backend::BootMode::FirstBoot)?;
        }
        if let Some(cmd) = post_start {
            backend::run_post_start(&session, cmd);
        }
    }

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
fn resolve_running(
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

fn cmd_shell(
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
fn prepend_binary(binary: &str, args: Vec<String>) -> Vec<String> {
    let mut command = Vec::with_capacity(1 + args.len());
    command.push(binary.to_string());
    command.extend(args);
    command
}

fn cmd_exec(
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
fn open_ssh_session(
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
fn prepare_session_from_target(
    cfg: &config::CoopConfig,
    inst: Option<&config::Instance>,
    target: backend::SshTarget,
    repo: Option<&github_repo::RepoSlug>,
) -> Result<backend::SshSession> {
    let mut env = backend::prepare_env_forwarding(cfg, repo)?;
    if let Some(inst) = inst
        && let Some(state) = guest_env_state::GuestEnvState::try_load(inst)?
    {
        for (name, value) in &state.entries {
            env.set(name.as_str(), value.as_str());
        }
    }
    Ok(backend::SshSession { target, env })
}

fn cmd_stop(
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
    // The `coop-<name>` SSH alias is left in place across stop: a stale
    // entry has no effect while the VM is down, and `coop start` refreshes
    // it (the Lima port changes per boot). `destroy`/`ssh-config --clean`
    // remove it.
    tracing::info!("Instance '{}' stopped", inst.name);
    Ok(())
}

fn cmd_destroy(
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
        be.destroy_instance(cfg, &inst)?;
        workspace::remove_ssh_config(&inst)?;
        tracing::info!("Instance '{}' destroyed", inst.name);
    }

    Ok(())
}

fn cmd_list(be: &backend::PlatformBackend, cfg: &config::CoopConfig) -> Result<()> {
    let mut instances = cfg.list_instances()?;
    if instances.is_empty() {
        writeln!(std::io::stdout(), "No instances found")
            .map_err(|e| anyhow::anyhow!("Failed to write list: {e}"))?;
        return Ok(());
    }
    instances.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    writeln!(std::io::stdout(), "{:<16} STATE", "NAME")
        .map_err(|e| anyhow::anyhow!("Failed to write list: {e}"))?;
    for inst in &instances {
        let state = if be.is_running(inst) {
            "running"
        } else {
            "stopped"
        };
        writeln!(std::io::stdout(), "{:<16} {state}", inst.name.as_str())
            .map_err(|e| anyhow::anyhow!("Failed to write list: {e}"))?;
    }
    Ok(())
}

/// Destroy every instance, delegate backend-specific shared-state cleanup
/// (`destroy_shared`), wipe the SSH keypair and instances dir, and strip every
/// coop block from `~/.ssh/config`.
///
/// Shared by `coop destroy --all` and `coop uninstall`. Does **not** remove
/// the `data_dir` itself or the binary — uninstall handles those.
fn purge_all_data(be: &backend::PlatformBackend, cfg: &config::CoopConfig) -> Result<()> {
    let instances = cfg.list_instances()?;
    for inst in &instances {
        tracing::info!("Destroying instance '{}'", inst.name);
        if let Ok(target) = be.ssh_target(cfg, inst) {
            port_forward::teardown_ssh_forwards(inst, &target);
        }
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
    Ok(())
}

struct UninstallOpts {
    yes: bool,
    keep_data: bool,
    purge: bool,
}

fn cmd_uninstall(
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

    let remove_data = decide_remove_data(cfg, opts)?;

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

fn decide_remove_data(cfg: &config::CoopConfig, opts: &UninstallOpts) -> Result<bool> {
    if opts.keep_data {
        return Ok(false);
    }
    if opts.yes || opts.purge {
        return Ok(true);
    }
    let instance_count = cfg.list_instances().map(|v| v.len()).unwrap_or(0);
    let image_count = cfg.list_images().map(|v| v.len()).unwrap_or(0);
    prompt::confirm(&format!(
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

fn cmd_status(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
) -> Result<()> {
    if let Some(name) = name {
        let inst = cfg.resolve_instance(Some(name))?;
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

fn cmd_resize(
    be: &backend::PlatformBackend,
    cfg: &config::CoopConfig,
    name: Option<&config::InstanceName>,
    disk_size: config::DiskSize,
) -> Result<()> {
    let inst = cfg.resolve_instance(name)?;

    let stopped = be.as_stopped(inst)?;
    let current = current_disk_gib(be, stopped.instance())?;
    let new_size = disk_size.resolve(current)?;

    be.resize_disk(cfg, &stopped, new_size)
}

fn current_disk_gib(be: &backend::PlatformBackend, inst: &config::Instance) -> Result<config::GiB> {
    let path = be.disk_path(inst)?;
    let bytes = std::fs::metadata(&path)
        .with_context(|| format!("Failed to stat {}", path.display()))?
        .len();
    #[expect(clippy::cast_possible_truncation, reason = "disk GiB fits in u32")]
    let gib = (bytes / (1024 * 1024 * 1024)) as u32;
    config::GiB::new(gib)
        .with_context(|| format!("Disk at {} is smaller than 1 GiB", path.display()))
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
    delete: Option<&config::ImageName>,
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
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {
    use clap::Parser;
    use proptest::prelude::*;

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
    fn ssh_config_subcommand_parses() {
        let cli = parse(&["ssh-config", "myvm"]);
        let super::Commands::SshConfig { name, clean } = cli.command else {
            panic!("expected SshConfig variant");
        };
        assert_eq!(name.expect("name").as_str(), "myvm");
        assert!(!clean);
    }

    #[test]
    fn ssh_config_clean_flag_parses() {
        let cli = parse(&["ssh-config", "--clean"]);
        let super::Commands::SshConfig { name, clean } = cli.command else {
            panic!("expected SshConfig variant");
        };
        assert!(name.is_none());
        assert!(clean);
    }

    #[test]
    fn ssh_config_does_not_collide_with_ssh_shell_alias() {
        // `ssh` is an alias for `shell`; `ssh-config` is its own command.
        assert!(matches!(
            parse(&["ssh"]).command,
            super::Commands::Shell { .. }
        ));
        assert!(matches!(
            parse(&["ssh-config"]).command,
            super::Commands::SshConfig { .. }
        ));
    }

    #[test]
    fn claude_name_and_trailing_args_parse() {
        let cli = parse(&["claude", "myvm", "--", "--model", "opus"]);
        let super::Commands::Claude { name, args, .. } = cli.command else {
            panic!("expected Claude variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(args, vec!["--model", "opus"]);
    }

    #[test]
    fn claude_agents_subcommand_parses() {
        let cli = parse(&["claude-agents"]);
        assert!(matches!(cli.command, super::Commands::ClaudeAgents { .. }));
    }

    #[test]
    fn claude_agents_ca_alias_parses() {
        let cli = parse(&["ca"]);
        assert!(matches!(cli.command, super::Commands::ClaudeAgents { .. }));
    }

    #[test]
    fn claude_agents_name_and_trailing_args_parse() {
        let cli = parse(&["ca", "myvm", "--", "--cwd", "/workspace"]);
        let super::Commands::ClaudeAgents { name, args, .. } = cli.command else {
            panic!("expected ClaudeAgents variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(args, vec!["--cwd", "/workspace"]);
    }

    #[test]
    fn codex_name_and_trailing_args_parse() {
        let cli = parse(&["codex", "myvm", "--", "--model", "gpt-5"]);
        let super::Commands::Codex { name, args, .. } = cli.command else {
            panic!("expected Codex variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(args, vec!["--model", "gpt-5"]);
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
    fn start_no_agents_flag_parses() {
        let cli = parse(&["start", "--no-agents"]);
        let super::Commands::Start { no_agents, .. } = cli.command else {
            panic!("expected Start variant");
        };
        assert!(no_agents);
    }

    #[test]
    fn up_profile_flag_parses_comma_list() {
        let cli = parse(&["up", "--profile", "python,node"]);
        let super::Commands::Up { profile, .. } = cli.command else {
            panic!("expected Up variant");
        };
        assert_eq!(profile, vec!["python", "node"]);
    }

    #[test]
    fn start_rejects_creation_flags_at_clap_time() {
        use clap::Parser as _;
        for flag in [
            ["--profile", "python"],
            ["--git-repo", "https://github.com/o/r"],
            ["--vcpus", "4"],
            ["--mem", "2048"],
            ["--disk", "16"],
            ["--mount", "/tmp"],
            ["--image", "default"],
        ] {
            let argv = ["coop", "start", flag[0], flag[1]];
            assert!(
                super::Cli::try_parse_from(argv).is_err(),
                "`coop start {}` should be rejected by clap",
                flag[0],
            );
        }
        assert!(
            super::Cli::try_parse_from(["coop", "start", "--exclude-git"]).is_err(),
            "`coop start --exclude-git` should be rejected by clap",
        );
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

    #[test]
    fn start_post_start_flag_parses() {
        let cli = parse(&["start", "--post-start", "touch /tmp/x"]);
        let super::Commands::Start { post_start, .. } = cli.command else {
            panic!("expected Start variant");
        };
        assert_eq!(post_start.as_deref(), Some("touch /tmp/x"));
    }

    #[test]
    fn start_no_claude_alias_parses() {
        let cli = parse(&["start", "--no-claude"]);
        let super::Commands::Start { no_agents, .. } = cli.command else {
            panic!("expected Start variant");
        };
        assert!(no_agents);
    }

    #[test]
    fn detect_deprecated_no_claude_flag() {
        assert!(super::raw_args_use_deprecated_no_claude([
            "coop",
            "start",
            "--no-claude"
        ]));
        assert!(!super::raw_args_use_deprecated_no_claude([
            "coop",
            "start",
            "--no-agents"
        ]));
        assert!(!super::raw_args_use_deprecated_no_claude([
            "coop",
            "start",
            "--name",
            "--no-claude-work"
        ]));
    }

    #[test]
    fn start_env_flag_parses_repeatable() {
        let cli = parse(&["start", "--env", "FOO=bar", "--env", "BAZ=qux"]);
        let super::Commands::Start { guest_env, .. } = cli.command else {
            panic!("expected Start variant");
        };
        let pairs: Vec<(String, String)> = guest_env
            .into_iter()
            .map(|(k, v)| (k.as_str().to_string(), v))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ]
        );
    }

    #[test]
    fn start_env_flag_rejects_invalid_key_at_clap_time() {
        use clap::Parser as _;
        let argv = ["coop", "start", "--env", "1BAD=value"];
        let Err(err) = super::Cli::try_parse_from(argv) else {
            panic!("expected --env to reject invalid KEY");
        };
        assert!(format!("{err}").contains("--env KEY is invalid"));
    }

    #[test]
    fn start_forward_port_rejects_invalid_spec_at_clap_time() {
        use clap::Parser as _;
        let argv = ["coop", "start", "--forward-port", "abc"];
        let Err(err) = super::Cli::try_parse_from(argv) else {
            panic!("expected --forward-port to reject invalid spec");
        };
        assert!(format!("{err}").contains("forward-port"));
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

    #[test]
    fn merge_runtime_guest_env_applies_three_tier_precedence() {
        use super::guest_env_state::EnvVarName;

        let env = |s: &str| EnvVarName::new(s).expect("valid env var");

        let mut cfg = super::config::CoopConfig::default();
        cfg.guest_env.insert(env("ONLY_CFG"), "cfg".to_string());
        cfg.guest_env.insert(env("SHARED"), "cfg".to_string());

        let mut dc = std::collections::BTreeMap::new();
        dc.insert(env("ONLY_DC"), "dc".to_string());
        dc.insert(env("SHARED"), "dc".to_string());

        let cli = vec![
            (env("ONLY_CLI"), "cli".to_string()),
            (env("SHARED"), "cli".to_string()),
        ];

        let merged = super::merge_runtime_guest_env(&mut cfg, &cli, Some(&dc));

        // CLI wins over devcontainer wins over config.toml on conflict.
        assert_eq!(merged.get(&env("SHARED")).map(String::as_str), Some("cli"));
        assert_eq!(
            cfg.guest_env.get(&env("SHARED")).map(String::as_str),
            Some("cli")
        );

        // The merged map carries dc + cli entries; config-only entries are not
        // re-added to it but remain in cfg.guest_env.
        assert_eq!(merged.get(&env("ONLY_DC")).map(String::as_str), Some("dc"));
        assert_eq!(
            merged.get(&env("ONLY_CLI")).map(String::as_str),
            Some("cli")
        );
        assert!(!merged.contains_key(&env("ONLY_CFG")));

        // The side effect folds dc + cli into cfg.guest_env without dropping
        // the pre-existing config.toml-only entry.
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_CFG")).map(String::as_str),
            Some("cfg")
        );
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_DC")).map(String::as_str),
            Some("dc")
        );
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_CLI")).map(String::as_str),
            Some("cli")
        );
    }

    #[test]
    fn merge_runtime_guest_env_without_devcontainer_uses_cli_only() {
        use super::guest_env_state::EnvVarName;

        let env = |s: &str| EnvVarName::new(s).expect("valid env var");

        let mut cfg = super::config::CoopConfig::default();
        cfg.guest_env.insert(env("ONLY_CFG"), "cfg".to_string());

        let cli = vec![(env("FROM_CLI"), "cli".to_string())];
        let merged = super::merge_runtime_guest_env(&mut cfg, &cli, None);

        assert_eq!(merged, cli.into_iter().collect());
        assert_eq!(
            cfg.guest_env.get(&env("FROM_CLI")).map(String::as_str),
            Some("cli")
        );
        assert_eq!(
            cfg.guest_env.get(&env("ONLY_CFG")).map(String::as_str),
            Some("cfg")
        );
    }

    /// Guest-env maps keyed in a deliberately small space so the three tiers
    /// overlap often, exercising the precedence path rather than only disjoint
    /// unions.
    fn small_env_map()
    -> impl Strategy<Value = std::collections::BTreeMap<super::guest_env_state::EnvVarName, String>>
    {
        prop::collection::btree_map(
            "[A-E]".prop_map(|s| {
                super::guest_env_state::EnvVarName::new(&s)
                    .expect("single uppercase letter is valid")
            }),
            any::<String>(),
            0..6,
        )
    }

    proptest! {
        /// `merge_runtime_guest_env` resolves all three tiers with
        /// CLI > devcontainer.json > config.toml precedence. The returned map
        /// carries exactly the devcontainer ∪ CLI entries (config-only keys
        /// stay in `cfg.guest_env` but are never re-added to the persisted
        /// map), and `cfg.guest_env` reflects the full three-tier merge.
        #[test]
        fn merge_runtime_guest_env_precedence_holds(
            cfg_env in small_env_map(),
            dc in small_env_map(),
            cli_map in small_env_map(),
        ) {
            use super::guest_env_state::EnvVarName;

            let mut cfg = super::config::CoopConfig::default();
            for (k, v) in &cfg_env {
                cfg.guest_env.insert(k.clone(), v.clone());
            }
            let cli: Vec<(EnvVarName, String)> =
                cli_map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

            let merged = super::merge_runtime_guest_env(&mut cfg, &cli, Some(&dc));

            // Returned map = devcontainer ∪ CLI (CLI wins); no config-only keys.
            let persisted_union: std::collections::BTreeSet<_> =
                dc.keys().chain(cli_map.keys()).cloned().collect();
            let got: std::collections::BTreeSet<_> = merged.keys().cloned().collect();
            prop_assert_eq!(got, persisted_union);
            for (k, v) in &merged {
                let expected = cli_map.get(k).or_else(|| dc.get(k)).expect("key from a tier");
                prop_assert_eq!(v, expected);
            }

            // cfg.guest_env now reflects the full three-tier precedence over
            // every key seen in any tier.
            let all_keys: std::collections::BTreeSet<_> = cfg_env
                .keys()
                .chain(dc.keys())
                .chain(cli_map.keys())
                .cloned()
                .collect();
            for k in &all_keys {
                let expected = cli_map
                    .get(k)
                    .or_else(|| dc.get(k))
                    .or_else(|| cfg_env.get(k))
                    .expect("key from a tier");
                prop_assert_eq!(cfg.guest_env.get(k).expect("merged key present"), expected);
            }
        }
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
    fn push_positional_name_and_dir_flag_parse() {
        let cli = parse(&["push", "myvm", "--dir", "./src", "--force"]);
        let super::Commands::Push {
            name, dir, force, ..
        } = cli.command
        else {
            panic!("expected Push variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(dir.as_deref(), Some("./src"));
        assert!(force);
    }

    #[test]
    fn push_single_positional_is_name_not_dir() {
        let cli = parse(&["push", "myvm"]);
        let super::Commands::Push {
            name, dir, force, ..
        } = cli.command
        else {
            panic!("expected Push variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert!(dir.is_none());
        assert!(!force);
    }

    #[test]
    fn push_bare_parses() {
        let cli = parse(&["push"]);
        let super::Commands::Push {
            name, dir, force, ..
        } = cli.command
        else {
            panic!("expected Push variant");
        };
        assert!(name.is_none());
        assert!(dir.is_none());
        assert!(!force);
    }

    #[test]
    fn pull_positional_name_and_dir_flag_parse() {
        let cli = parse(&["pull", "myvm", "--dir", "./out", "--force"]);
        let super::Commands::Pull {
            name, dir, force, ..
        } = cli.command
        else {
            panic!("expected Pull variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(dir.as_deref(), Some("./out"));
        assert!(force);
    }

    #[test]
    fn exec_positional_name_and_command_parse() {
        let cli = parse(&["exec", "myvm", "--", "ls", "-la"]);
        let super::Commands::Exec { name, command } = cli.command else {
            panic!("expected Exec variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(command, vec!["ls", "-la"]);
    }

    #[test]
    fn exec_without_name_parses() {
        let cli = parse(&["exec", "--", "ls", "-la"]);
        let super::Commands::Exec { name, command } = cli.command else {
            panic!("expected Exec variant");
        };
        assert!(name.is_none());
        assert_eq!(command, vec!["ls", "-la"]);
    }

    #[test]
    fn exec_requires_command_after_separator() {
        let err = parse_err(&["exec", "myvm"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn list_subcommand_parses() {
        let cli = parse(&["list"]);
        assert!(matches!(cli.command, super::Commands::List));
    }

    #[test]
    fn ls_alias_parses_as_list() {
        let cli = parse(&["ls"]);
        assert!(matches!(cli.command, super::Commands::List));
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

    #[test]
    fn uninstall_subcommand_parses() {
        let cli = parse(&["uninstall"]);
        let super::Commands::Uninstall {
            yes,
            keep_data,
            purge,
        } = cli.command
        else {
            panic!("expected Uninstall variant");
        };
        assert!(!yes);
        assert!(!keep_data);
        assert!(!purge);
    }

    #[test]
    fn uninstall_yes_flag_parses() {
        let cli = parse(&["uninstall", "--yes"]);
        let super::Commands::Uninstall { yes, .. } = cli.command else {
            panic!("expected Uninstall variant");
        };
        assert!(yes);
    }

    #[test]
    fn uninstall_short_y_flag_parses() {
        let cli = parse(&["uninstall", "-y"]);
        let super::Commands::Uninstall { yes, .. } = cli.command else {
            panic!("expected Uninstall variant");
        };
        assert!(yes);
    }

    #[test]
    fn uninstall_keep_data_flag_parses() {
        let cli = parse(&["uninstall", "--keep-data"]);
        let super::Commands::Uninstall { keep_data, .. } = cli.command else {
            panic!("expected Uninstall variant");
        };
        assert!(keep_data);
    }

    #[test]
    fn uninstall_purge_flag_parses() {
        let cli = parse(&["uninstall", "--purge"]);
        let super::Commands::Uninstall { purge, .. } = cli.command else {
            panic!("expected Uninstall variant");
        };
        assert!(purge);
    }

    #[test]
    fn uninstall_keep_data_and_purge_conflict() {
        let err = parse_err(&["uninstall", "--keep-data", "--purge"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
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
    fn decide_remove_data_keeps_data_when_keep_data_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(!super::decide_remove_data(&cfg, &opts(true, true, false)).unwrap());
        assert!(!super::decide_remove_data(&cfg, &opts(false, true, false)).unwrap());
    }

    #[test]
    fn decide_remove_data_removes_when_yes_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(super::decide_remove_data(&cfg, &opts(true, false, false)).unwrap());
    }

    #[test]
    fn decide_remove_data_removes_when_purge_set() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(super::decide_remove_data(&cfg, &opts(false, false, true)).unwrap());
        assert!(super::decide_remove_data(&cfg, &opts(true, false, true)).unwrap());
    }

    #[test]
    fn decide_remove_data_interactive_returns_false_without_tty() {
        // In `cargo test` stdin is not a TTY, so `prompt::confirm` returns
        // `Ok(false)` — exercising the interactive branch deterministically.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_data_dir(tmp.path().to_path_buf());
        assert!(!super::decide_remove_data(&cfg, &opts(false, false, false)).unwrap());
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

    #[test]
    fn completions_subcommand_parses_each_shell() {
        for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
            let cli = parse(&["completions", shell]);
            let super::Commands::Completions { shell: parsed } = cli.command else {
                panic!("expected Completions variant for {shell}");
            };
            assert_eq!(parsed.to_string(), shell);
        }
    }

    #[test]
    fn completions_subcommand_requires_shell() {
        let err = parse_err(&["completions"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
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
    fn up_subcommand_parses_defaults() {
        let cli = parse(&["up"]);
        let super::Commands::Up {
            dir,
            name,
            copy,
            mount,
            extra_mount,
            ..
        } = cli.command
        else {
            panic!("expected Up variant");
        };
        assert_eq!(dir, None);
        assert!(name.is_none());
        assert!(!copy);
        assert!(!mount);
        assert!(extra_mount.is_empty());
    }

    #[test]
    fn up_subcommand_parses_project_and_mount_mode() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let cli = parse(&[
            "up",
            tmp.path().to_str().unwrap(),
            "--mount",
            "--name",
            "named-project",
        ]);
        let super::Commands::Up {
            dir, name, mount, ..
        } = cli.command
        else {
            panic!("expected Up variant");
        };
        assert_eq!(dir.as_deref(), tmp.path().to_str());
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("named-project")
        );
        assert!(mount);
    }

    #[test]
    fn up_copy_and_mount_conflict() {
        let err = parse_err(&["up", "--copy", "--mount"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn up_git_repo_parses_as_workspace_source() {
        let cli = parse(&[
            "up",
            "--git-repo",
            "https://github.com/trailofbits/coop.git",
        ]);
        let super::Commands::Up { git_repo, .. } = cli.command else {
            panic!("expected Up variant");
        };
        assert_eq!(
            git_repo.as_deref(),
            Some("https://github.com/trailofbits/coop.git")
        );
    }

    #[test]
    fn up_git_repo_conflicts_with_local_workspace_sources() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let err = parse_err(&[
            "up",
            tmp.path().to_str().unwrap(),
            "--git-repo",
            "https://github.com/trailofbits/coop.git",
        ]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);

        let err = parse_err(&[
            "up",
            "--git-repo",
            "https://github.com/trailofbits/coop.git",
            "--mount",
        ]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn git_repo_default_instance_name_uses_repo_basename() {
        let name = super::git_repo_default_instance_name("https://github.com/trailofbits/coop.git")
            .expect("name");
        assert_eq!(name.as_str(), "coop");

        let name =
            super::git_repo_default_instance_name("git@example.com:org/my.repo.git").expect("name");
        assert_eq!(name.as_str(), "my-repo");
    }

    #[test]
    fn git_repo_default_instance_name_handles_url_edges() {
        // Trailing slash, no `.git` suffix.
        let name =
            super::git_repo_default_instance_name("https://example.com/org/widget/").expect("name");
        assert_eq!(name.as_str(), "widget");

        // scp-style without a `.git` suffix.
        let name =
            super::git_repo_default_instance_name("git@example.com:org/tools").expect("name");
        assert_eq!(name.as_str(), "tools");

        // Bare path with no host.
        let name = super::git_repo_default_instance_name("/srv/git/repo.git").expect("name");
        assert_eq!(name.as_str(), "repo");
    }

    #[test]
    fn git_repo_default_instance_name_rejects_unusable_basenames() {
        // A basename that sanitizes to nothing has no usable instance name.
        assert!(super::git_repo_default_instance_name("https://example.com/org/.git").is_none());
        assert!(super::git_repo_default_instance_name("https://example.com/org/---").is_none());
    }

    #[test]
    fn git_repo_default_instance_name_caps_long_basenames() {
        let long = format!("https://example.com/org/{}.git", "a".repeat(200));
        let name = super::git_repo_default_instance_name(&long).expect("name");
        assert_eq!(name.as_str().len(), 60);
        assert!(name.as_str().chars().all(|c| c == 'a'));
    }

    #[test]
    fn git_repo_rejects_extra_mount_at_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).expect("data");
        let mounts = vec![super::config::Mount::parse(data.to_str().unwrap()).expect("mount")];

        let err = super::validate_git_repo_workspace_mounts(&mounts)
            .expect_err("expected /workspace collision");
        assert!(format!("{err}").contains("/workspace"));
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
    fn validate_unique_guest_paths_rejects_duplicates() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir(&a).expect("create a");
        std::fs::create_dir(&b).expect("create b");
        let mounts = vec![
            super::config::Mount::parse(&format!("{}:/data", a.display())).expect("mount a"),
            super::config::Mount::parse(&format!("{}:/data", b.display())).expect("mount b"),
        ];
        let err = super::validate_unique_guest_paths(&mounts).expect_err("expected duplicate");
        assert!(format!("{err}").contains("/data"));
    }

    #[test]
    fn up_copy_rejects_extra_mount_at_workspace() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = tmp.path().join("data");
        std::fs::create_dir(&data).expect("data");
        let mounts = vec![super::config::Mount::parse(data.to_str().unwrap()).expect("mount")];

        let err = super::validate_copy_workspace_mounts(super::ProjectTransport::Copy, &mounts)
            .expect_err("expected /workspace collision");
        assert!(format!("{err}").contains("/workspace"));
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
            },
        }
    }

    #[test]
    fn devcontainer_input_from_flags_precedence() {
        let path = std::path::PathBuf::from("/tmp/devcontainer.json");
        assert!(matches!(
            super::DevcontainerInput::from_flags(Some(path.clone()), false),
            super::DevcontainerInput::Explicit(p) if p == path
        ));
        assert!(matches!(
            super::DevcontainerInput::from_flags(None, false),
            super::DevcontainerInput::Discover
        ));
        assert!(matches!(
            super::DevcontainerInput::from_flags(None, true),
            super::DevcontainerInput::Disabled
        ));
        // --no-devcontainer wins even if a path is also present.
        assert!(matches!(
            super::DevcontainerInput::from_flags(Some(path), true),
            super::DevcontainerInput::Disabled
        ));
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
    fn quickstart_subcommand_parses_with_defaults() {
        let cli = parse(&["quickstart"]);
        let super::Commands::Quickstart {
            no_workspace,
            no_devcontainer,
        } = cli.command
        else {
            panic!("expected Quickstart variant");
        };
        assert!(!no_workspace);
        assert!(!no_devcontainer);
    }

    #[test]
    fn quickstart_no_workspace_flag_parses() {
        let cli = parse(&["quickstart", "--no-workspace"]);
        let super::Commands::Quickstart { no_workspace, .. } = cli.command else {
            panic!("expected Quickstart variant");
        };
        assert!(no_workspace);
    }

    #[test]
    fn quickstart_no_devcontainer_flag_parses() {
        let cli = parse(&["quickstart", "--no-devcontainer"]);
        let super::Commands::Quickstart {
            no_devcontainer, ..
        } = cli.command
        else {
            panic!("expected Quickstart variant");
        };
        assert!(no_devcontainer);
    }

    #[test]
    fn quickstart_rejects_unknown_flag() {
        let err = parse_err(&["quickstart", "--name", "foo"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn devcontainer_check_subcommand_parses_with_defaults() {
        let cli = parse(&["devcontainer", "check", ".devcontainer/devcontainer.json"]);
        let super::Commands::Devcontainer { command } = cli.command else {
            panic!("expected Devcontainer variant");
        };
        let super::DevcontainerCommands::Check { path, stage } = command else {
            panic!("expected devcontainer check variant");
        };
        assert_eq!(
            path,
            std::path::PathBuf::from(".devcontainer/devcontainer.json")
        );
        assert!(matches!(stage, super::DevcontainerCheckStage::Both));
    }

    #[test]
    fn devcontainer_check_stage_flag_parses() {
        let cli = parse(&[
            "devcontainer",
            "check",
            ".devcontainer/devcontainer.json",
            "--stage",
            "start",
        ]);
        let super::Commands::Devcontainer { command } = cli.command else {
            panic!("expected Devcontainer variant");
        };
        let super::DevcontainerCommands::Check { stage, .. } = command else {
            panic!("expected devcontainer check variant");
        };
        assert!(matches!(stage, super::DevcontainerCheckStage::Start));
    }

    #[test]
    fn devcontainer_ignore_subcommand_parses() {
        let cli = parse(&["devcontainer", "ignore", "."]);
        let super::Commands::Devcontainer { command } = cli.command else {
            panic!("expected Devcontainer variant");
        };
        let super::DevcontainerCommands::Ignore { project } = command else {
            panic!("expected devcontainer ignore variant");
        };
        assert_eq!(project, std::path::PathBuf::from("."));
    }

    #[test]
    fn devcontainer_status_subcommand_parses_optional_project() {
        let cli = parse(&["devcontainer", "status"]);
        let super::Commands::Devcontainer { command } = cli.command else {
            panic!("expected Devcontainer variant");
        };
        let super::DevcontainerCommands::Status { project } = command else {
            panic!("expected devcontainer status variant");
        };
        assert!(project.is_none());

        let cli = parse(&["devcontainer", "status", "."]);
        let super::Commands::Devcontainer { command } = cli.command else {
            panic!("expected Devcontainer variant");
        };
        let super::DevcontainerCommands::Status { project } = command else {
            panic!("expected devcontainer status variant");
        };
        assert_eq!(project, Some(std::path::PathBuf::from(".")));
    }

    #[test]
    fn devcontainer_clear_subcommand_parses() {
        let cli = parse(&["devcontainer", "clear", "."]);
        let super::Commands::Devcontainer { command } = cli.command else {
            panic!("expected Devcontainer variant");
        };
        let super::DevcontainerCommands::Clear { project } = command else {
            panic!("expected devcontainer clear variant");
        };
        assert_eq!(project, std::path::PathBuf::from("."));
    }

    #[test]
    fn devcontainer_check_both_stage_reuses_setup_guest_user() {
        let translation = super::devcontainer::Translation {
            guest_user: Some(super::guest::GuestUser::new("vscode").unwrap()),
            ..super::devcontainer::Translation::default()
        };
        assert_eq!(
            super::devcontainer_check_assumed_guest_user(Some(&translation)).to_string(),
            "vscode"
        );
    }

    #[test]
    fn devcontainer_check_both_stage_defaults_without_setup_guest_user() {
        assert_eq!(
            super::devcontainer_check_assumed_guest_user(None).to_string(),
            "ubuntu"
        );
    }

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
    fn completions_emit_static_includes_subcommands() {
        let mut buf: Vec<u8> = Vec::new();
        let mut cmd = <super::Cli as clap::CommandFactory>::command();
        let name = cmd.get_name().to_string();
        clap_complete::generate(clap_complete::Shell::Bash, &mut cmd, name, &mut buf);
        let Ok(script) = String::from_utf8(buf) else {
            panic!("bash completion script is not valid utf-8");
        };
        for sub in ["shell", "claude", "destroy", "completions"] {
            assert!(script.contains(sub), "completion script missing `{sub}`");
        }
    }
}
