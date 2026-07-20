//! `coop` library crate.
//!
//! Owns every module declaration and the CLI dispatch (`run`). The binary
//! (`src/main.rs`) is a thin shim that calls [`run`]. Splitting the crate this
//! way lets fuzz targets and integration tests depend on `coop` directly
//! (e.g. `coop::config::CoopConfig`) instead of `#[path]`-including modules.

mod backend;
mod cmd;
mod commands;
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
mod model_state;
mod naming;
mod pat_prompt;
mod paths;
mod port_forward;
mod proxy;
mod proxy_state;
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCandidates;

use backend::VmBackend as _;
use commands::json;
use commands::{
    AgentUpdateOpts, DevcontainerInput, DevcontainerOpts, ProfileImageTarget, ProjectTransport,
    QuickstartOpts, ResizeOpts, StartOpts, UninstallOpts, UpDevcontainerOpts, UpOpts,
    UpRuntimeOpts, apply_runtime_guest_env, apply_vm_overrides, cmd_agent_update, cmd_commit,
    cmd_destroy, cmd_devcontainer, cmd_devcontainer_check, cmd_exec, cmd_github, cmd_images,
    cmd_init, cmd_list, cmd_model, cmd_profiles, cmd_proxy, cmd_quickstart, cmd_resize,
    cmd_restore, cmd_shell, cmd_start, cmd_status, cmd_stop, cmd_uninstall, cmd_up, cmd_validate,
    codex_launch_args, open_ssh_session, preflight_start_target, prepend_binary,
    resolve_devcontainer, resolve_devcontainer_collect, resolve_running,
};

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
        #[arg(long, value_parser = config::VmMemory::parse_cli)]
        mem: Option<config::VmMemory>,
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
        /// With --dry-run, emit the resolved plan as JSON on stdout
        #[arg(long, requires = "dry_run")]
        json: bool,
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
        #[arg(long, value_parser = config::VmMemory::parse_cli)]
        mem: Option<config::VmMemory>,
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
        /// Duration to wait for setup image build commands before timing
        /// out. Accepts seconds by default, or an `s`/`m`/`h` suffix
        /// (e.g. `90s`, `30m`, `2h`).
        #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
        builder_timeout: Option<Duration>,
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
        /// With --dry-run, emit the resolved plan as JSON on stdout
        #[arg(long, requires = "dry_run")]
        json: bool,
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
    /// Launch Codex inside the VM (bypasses the sandbox and approvals by default)
    Codex {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Keep Codex's sandbox and approval prompts instead of bypassing them
        #[arg(long)]
        ask: bool,
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
    List {
        /// Emit machine-readable JSON instead of the text table
        #[arg(long)]
        json: bool,
    },
    /// Show VM status
    Status {
        /// Instance name (shows all if omitted)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Emit machine-readable JSON instead of the text output
        #[arg(long)]
        json: bool,
    },
    /// Manage the coding agents installed inside a VM
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Show or switch a VM's model backend (cloud vs. local)
    Model {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// `local` to route at a host-side model, `remote` for cloud
        /// defaults; omit to show the current selection
        #[command(subcommand)]
        action: Option<ModelAction>,
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
        /// Emit machine-readable JSON instead of the text table
        #[arg(long)]
        json: bool,
    },
    /// Resize a stopped instance's disk, memory, or vCPU count
    #[command(group(clap::ArgGroup::new("resize_targets")
        .required(true)
        .multiple(true)
        .args(["size", "mem", "vcpus"])))]
    Resize {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// New disk size: absolute GiB (e.g. 150, 150G) or relative (e.g. +20, +20G)
        #[arg(long, value_parser = config::DiskSize::parse)]
        size: Option<config::DiskSize>,
        /// New memory in MiB (minimum 128)
        #[arg(long, value_parser = config::VmMemory::parse_cli)]
        mem: Option<config::VmMemory>,
        /// New vCPU count
        #[arg(long)]
        vcpus: Option<std::num::NonZeroU8>,
        /// Start the instance after applying the change instead of leaving it stopped
        #[arg(long)]
        start: bool,
    },
    /// Save a stopped instance's filesystem as a reusable image
    Commit {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Name of the image to create
        #[arg(long, required = true, value_parser = config::ImageName::new)]
        image: config::ImageName,
        /// Overwrite an existing image with the same name
        #[arg(long)]
        force: bool,
    },
    /// Roll a stopped instance back to an image's filesystem in place
    Restore {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Name of the image to restore from
        #[arg(
            long,
            required = true,
            value_parser = config::ImageName::new,
            add = ArgValueCandidates::new(completions::image_candidates),
        )]
        image: config::ImageName,
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
    /// Manage the credential-injecting proxy (issue #411)
    Proxy {
        #[command(subcommand)]
        action: ProxyAction,
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
enum AgentAction {
    /// Update coding agent(s) to the latest version inside the VM.
    ///
    /// With no agent flag, both Claude Code and Codex are updated. The VM
    /// must be running.
    Update {
        /// Instance name (required if multiple instances exist)
        #[arg(
            value_parser = config::InstanceName::new,
            add = ArgValueCandidates::new(completions::instance_candidates),
        )]
        name: Option<config::InstanceName>,
        /// Update Claude Code (default: update both agents)
        #[arg(long)]
        claude: bool,
        /// Update Codex (default: update both agents)
        #[arg(long)]
        codex: bool,
        /// Only report installed vs. latest versions — change nothing
        #[arg(long)]
        check: bool,
        /// Skip the confirmation prompt
        #[arg(short = 'y', long)]
        yes: bool,
    },
}

#[derive(Subcommand, Clone, Copy)]
enum ModelAction {
    /// Route this VM's agents at a host-side local model server
    Local,
    /// Restore cloud defaults (Anthropic / `OpenAI`)
    Remote,
}

impl From<ModelAction> for model_state::ModelMode {
    fn from(action: ModelAction) -> Self {
        match action {
            ModelAction::Local => model_state::ModelMode::Local,
            ModelAction::Remote => model_state::ModelMode::Remote,
        }
    }
}

#[derive(Subcommand)]
enum ProfilesAction {
    /// List all available profiles (builtin and custom)
    List {
        /// Emit machine-readable JSON instead of the text listing
        #[arg(long)]
        json: bool,
    },
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
        /// Emit machine-readable JSON instead of the text report
        #[arg(long)]
        json: bool,
    },
    /// Remove a configured PAT entry and its stored secret
    ForgetPat {
        /// Repo slug whose entry should be removed
        #[arg(long, required = true, value_parser = github_repo::RepoSlug::parse_cli)]
        repo: github_repo::RepoSlug,
    },
}

#[derive(Subcommand)]
enum ProxyAction {
    /// Store a provider credential in a secret backend and wire it into
    /// `[proxy.<provider>]` (the default) or a per-VM override. Anthropic
    /// (Claude) is the default provider; pass `--openai` for Codex.
    Setup {
        /// Configure the `OpenAI` (Codex) upstream instead of Anthropic. `OpenAI`
        /// keys are always injected as `Authorization: Bearer`.
        #[arg(long, conflicts_with = "anthropic")]
        openai: bool,
        /// Configure the Anthropic (Claude) upstream (the default provider).
        #[arg(long)]
        anthropic: bool,
        /// Store the credential as a per-VM override for this instance instead
        /// of the global `[proxy.<provider>]` default.
        #[arg(
            long,
            value_name = "NAME",
            add = ArgValueCandidates::new(completions::instance_candidates)
        )]
        vm: Option<String>,
        /// Anthropic only: store an API key (`x-api-key`) instead of a Claude
        /// `setup-token`. Ignored for `--openai` (always Bearer).
        #[arg(long)]
        api_key: bool,
    },
    /// Show what each VM's agents resolve to (per-VM override → default → off),
    /// with credentials redacted.
    Status {
        /// Show the effective resolution for a single VM instead of all.
        #[arg(
            long,
            value_name = "NAME",
            add = ArgValueCandidates::new(completions::instance_candidates)
        )]
        vm: Option<String>,
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
        /// Emit machine-readable JSON on stdout instead of the text report on stderr
        #[arg(long)]
        json: bool,
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
// Scoped out of mutation testing: pure arg dispatch to the cmd_* handlers,
// unobservable from a `--lib` test. exclude_re cannot suppress its
// `delete field … from struct devcontainer::TranslatorInputs` mutants
// (see the note in .cargo/mutants.toml), so skip the whole body here.
#[mutants::skip]
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
            json,
        } => {
            cfg.validate_and_warn()?;
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
                    json,
                },
            };
            cmd_up(&be, &mut cfg, &cli.config, &opts)
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
            builder_timeout,
            workspace,
            devcontainer,
            no_devcontainer,
            dry_run,
        } => {
            cfg.validate_and_warn()?;
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
                    builder_timeout,
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
            json: json_out,
        } => {
            cfg.validate_and_warn()?;
            if raw_args_use_deprecated_no_claude(&raw_args) {
                tracing::warn!(
                    "--no-claude is deprecated and will be removed in a future release; use --no-agents"
                );
            }
            if dry_run {
                let cli_env_keys = guest_env.iter().map(|(k, _)| k.clone()).collect();
                let dry_run_image = config::default_image_name();
                let dry_run_guest_user = backend::persisted_guest_user(&cfg, &dry_run_image);
                let inputs = devcontainer::TranslatorInputs {
                    cli_post_start: post_start.clone(),
                    cli_guest_env_keys: cli_env_keys,
                    cli_forward_ports: forward_ports.clone(),
                    persisted_guest_user: Some(dry_run_guest_user.clone()),
                    cli_workspace_or_git_repo: workspace.is_some(),
                    ..devcontainer::TranslatorInputs::default()
                };
                let ws_path = workspace.as_deref().map(Path::new);
                let dc_input = DevcontainerInput::from_flags(devcontainer.clone(), no_devcontainer);
                let dc_opts = DevcontainerOpts {
                    input: &dc_input,
                    dry_run,
                    workspace: ws_path,
                    mounts: &[],
                    git_repo: None,
                    github_auth: cfg.github.as_ref(),
                    preference_path: Some(&cfg.devcontainer_preferences_path()),
                };
                if json_out {
                    let translation = resolve_devcontainer_collect(
                        &dc_opts,
                        &inputs,
                        devcontainer::Stage::Start,
                    )?;
                    let plan = json::DryRunPlan {
                        report: translation.as_ref().map(|t| &t.report),
                        profiles: &[],
                        guest_user: &dry_run_guest_user,
                        vm: json::VmOverrides {
                            vcpus: None,
                            mem_mib: None,
                            disk_gib: None,
                        },
                    };
                    return json::render_json(&plan);
                }
                resolve_devcontainer(&dc_opts, &inputs, devcontainer::Stage::Start)?;
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
            cmd_start(&be, &mut cfg, &start_opts).map(|_| ())
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
        Commands::Codex { name, ask, args } => {
            let sess = open_ssh_session(&be, &cfg, name.as_ref())?;
            let args = codex_launch_args(ask, args);
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
        Commands::List { json } => cmd_list(&be, &cfg, json),
        Commands::Status { name, json } => cmd_status(&be, &cfg, name.as_ref(), json),
        Commands::Agent {
            action:
                AgentAction::Update {
                    name,
                    claude,
                    codex,
                    check,
                    yes,
                },
        } => cmd_agent_update(
            &be,
            &cfg,
            name.as_ref(),
            &AgentUpdateOpts {
                selection: commands::AgentSelection::from_flags(claude, codex),
                check,
                yes,
            },
        ),
        Commands::Model { name, action } => {
            cmd_model(&be, &cfg, name.as_ref(), action.map(Into::into))
        }
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
        Commands::Images { delete, json } => cmd_images(&be, &cfg, delete.as_ref(), json),
        Commands::Resize {
            name,
            size,
            mem,
            vcpus,
            start,
        } => cmd_resize(
            &be,
            &cfg,
            &ResizeOpts {
                name: name.as_ref(),
                disk: size,
                mem,
                vcpus,
                start,
            },
        ),
        Commands::Commit { name, image, force } => {
            cmd_commit(&be, &cfg, name.as_ref(), &image, force)
        }
        Commands::Restore { name, image } => cmd_restore(&be, &cfg, name.as_ref(), &image),
        Commands::Profiles { action } => cmd_profiles(
            &cfg,
            &action.unwrap_or(ProfilesAction::List { json: false }),
        ),
        Commands::Github { action } => cmd_github(&cfg, &cli.config, action),
        Commands::Proxy { action } => cmd_proxy(&cfg, &cli.config, &action),
        Commands::Validate { probe } => cmd_validate(&cfg, &be, probe),
        Commands::Init
        | Commands::Update { .. }
        | Commands::Uninstall { .. }
        | Commands::Completions { .. } => {
            unreachable!("handled before config loading")
        }
    }
}

fn parse_duration(value: &str) -> std::result::Result<Duration, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("duration cannot be empty".to_string());
    }

    let (number, multiplier) = if let Some(number) = value.strip_suffix('s') {
        (number, 1)
    } else if let Some(number) = value.strip_suffix('m') {
        (number, 60)
    } else if let Some(number) = value.strip_suffix('h') {
        (number, 60 * 60)
    } else {
        (value, 1)
    };

    let number = number
        .parse::<u64>()
        .map_err(|_| format!("invalid duration {value:?}; use seconds, or suffix s/m/h"))?;
    let seconds = number
        .checked_mul(multiplier)
        .ok_or_else(|| format!("duration {value:?} is too large"))?;
    if seconds == 0 {
        return Err("duration must be greater than zero".to_string());
    }
    Ok(Duration::from_secs(seconds))
}

fn format_duration(duration: Duration) -> String {
    format!("{}s", duration.as_secs())
}

#[cfg(test)]
#[expect(clippy::panic, reason = "tests use panic for unreachable branches")]
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
mod tests {
    use std::time::Duration;

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
    fn setup_builder_timeout_parses() {
        let cli = parse(&["setup", "--builder-timeout", "60m"]);
        let super::Commands::Setup {
            builder_timeout, ..
        } = cli.command
        else {
            panic!("expected Setup variant");
        };
        assert_eq!(builder_timeout, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn parse_duration_accepts_suffixes_and_bare_seconds() {
        assert_eq!(super::parse_duration("600"), Ok(Duration::from_secs(600)));
        assert_eq!(super::parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(super::parse_duration("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(super::parse_duration("2h"), Ok(Duration::from_secs(7200)));
        assert_eq!(super::parse_duration(" 30s "), Ok(Duration::from_secs(30)));
    }

    #[test]
    fn setup_builder_timeout_rejects_invalid_durations() {
        for value in ["", "0m", "abc", "1.5h", "18446744073709551615h"] {
            assert!(
                super::parse_duration(value).is_err(),
                "{value:?} should not parse as a duration",
            );
        }
    }

    #[test]
    fn format_duration_outputs_seconds() {
        assert_eq!(super::format_duration(Duration::from_secs(90)), "90s");
    }

    #[test]
    fn parse_then_format_normalizes_to_seconds() {
        let parsed = super::parse_duration("1h").expect("1h parses");
        assert_eq!(super::format_duration(parsed), "3600s");
    }

    #[test]
    fn agent_update_parses_name_and_flags() {
        let cli = parse(&["agent", "update", "myvm", "--codex", "--check"]);
        let super::Commands::Agent {
            action:
                super::AgentAction::Update {
                    name,
                    claude,
                    codex,
                    check,
                    yes,
                },
        } = cli.command
        else {
            panic!("expected Agent::Update variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert!(!claude);
        assert!(codex);
        assert!(check);
        assert!(!yes);
    }

    #[test]
    fn agent_update_defaults_are_all_false() {
        let cli = parse(&["agent", "update"]);
        let super::Commands::Agent {
            action:
                super::AgentAction::Update {
                    name,
                    claude,
                    codex,
                    check,
                    yes,
                },
        } = cli.command
        else {
            panic!("expected Agent::Update variant");
        };
        assert!(name.is_none());
        assert!(!claude && !codex && !check && !yes);
    }

    #[test]
    fn agent_update_yes_short_flag_parses() {
        let cli = parse(&["agent", "update", "-y"]);
        let super::Commands::Agent {
            action: super::AgentAction::Update { yes, .. },
        } = cli.command
        else {
            panic!("expected Agent::Update variant");
        };
        assert!(yes);
    }

    #[test]
    fn model_action_maps_to_mode() {
        use crate::model_state::ModelMode;

        use super::ModelAction;
        assert_eq!(ModelMode::from(ModelAction::Local), ModelMode::Local);
        assert_eq!(ModelMode::from(ModelAction::Remote), ModelMode::Remote);
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
    fn commit_parses_name_image_and_force() {
        let cli = parse(&["commit", "myvm", "--image", "safe-point", "--force"]);
        let super::Commands::Commit { name, image, force } = cli.command else {
            panic!("expected Commit variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(image.as_str(), "safe-point");
        assert!(force);
    }

    #[test]
    fn commit_force_defaults_off_and_image_required() {
        let cli = parse(&["commit", "--image", "snap"]);
        let super::Commands::Commit { name, force, .. } = cli.command else {
            panic!("expected Commit variant");
        };
        assert!(name.is_none());
        assert!(!force);
        // `--image` is mandatory.
        assert!(
            parse_err(&["commit", "myvm"]).kind()
                == clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn restore_parses_name_and_image() {
        let cli = parse(&["restore", "myvm", "--image", "safe-point"]);
        let super::Commands::Restore { name, image } = cli.command else {
            panic!("expected Restore variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert_eq!(image.as_str(), "safe-point");
        // `--image` is mandatory; `restore` has no `--force`.
        assert!(
            parse_err(&["restore", "myvm"]).kind()
                == clap::error::ErrorKind::MissingRequiredArgument
        );
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
        let super::Commands::Codex { name, ask, args } = cli.command else {
            panic!("expected Codex variant");
        };
        assert_eq!(
            name.as_ref().map(super::config::InstanceName::as_str),
            Some("myvm")
        );
        assert!(!ask, "ask defaults to false (sandbox bypassed)");
        assert_eq!(args, vec!["--model", "gpt-5"]);
    }

    #[test]
    fn codex_ask_flag_parses() {
        let cli = parse(&["codex", "myvm", "--ask", "--", "--model", "gpt-5"]);
        let super::Commands::Codex { ask, args, .. } = cli.command else {
            panic!("expected Codex variant");
        };
        assert!(ask, "--ask keeps Codex's sandbox and approvals");
        assert_eq!(args, vec!["--model", "gpt-5"]);
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
        assert!(matches!(cli.command, super::Commands::List { json: false }));
    }

    #[test]
    fn list_json_flag_parses() {
        let cli = parse(&["list", "--json"]);
        assert!(matches!(cli.command, super::Commands::List { json: true }));
    }

    #[test]
    fn ls_alias_parses_as_list() {
        let cli = parse(&["ls"]);
        assert!(matches!(cli.command, super::Commands::List { .. }));
    }

    #[test]
    fn status_json_flag_parses() {
        let cli = parse(&["status", "--json"]);
        let super::Commands::Status { json, .. } = cli.command else {
            panic!("expected Status variant");
        };
        assert!(json);
    }

    #[test]
    fn profiles_list_parses() {
        let cli = parse(&["profiles", "list"]);
        let super::Commands::Profiles { action } = cli.command else {
            panic!("expected Profiles variant");
        };
        assert!(matches!(
            action,
            Some(super::ProfilesAction::List { json: false })
        ));
    }

    #[test]
    fn profiles_list_json_flag_parses() {
        let cli = parse(&["profiles", "list", "--json"]);
        let super::Commands::Profiles { action } = cli.command else {
            panic!("expected Profiles variant");
        };
        assert!(matches!(
            action,
            Some(super::ProfilesAction::List { json: true })
        ));
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
        let super::DevcontainerCommands::Check { path, stage, json } = command else {
            panic!("expected devcontainer check variant");
        };
        assert_eq!(
            path,
            std::path::PathBuf::from(".devcontainer/devcontainer.json")
        );
        assert!(matches!(stage, super::DevcontainerCheckStage::Both));
        assert!(!json);
    }

    #[test]
    fn devcontainer_check_json_flag_parses() {
        let cli = parse(&[
            "devcontainer",
            "check",
            ".devcontainer/devcontainer.json",
            "--json",
        ]);
        let super::Commands::Devcontainer { command } = cli.command else {
            panic!("expected Devcontainer variant");
        };
        let super::DevcontainerCommands::Check { json, .. } = command else {
            panic!("expected devcontainer check variant");
        };
        assert!(json);
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
