mod backend;
mod cmd;
mod completions;
mod config;
mod devcontainer;
mod fs_util;
mod github_pat;
mod github_repo;
mod guest;
mod guest_env_state;
mod pat_prompt;
mod port_forward;
mod secret_store;
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
mod update;
#[cfg_attr(target_os = "macos", expect(dead_code, reason = "Firecracker-only"))]
mod vm;
mod workspace;

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
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

#[derive(clap::Args)]
struct SessionArgs {
    /// Instance name (required if multiple instances exist)
    #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
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

    /// Returns a tmux session name only when explicitly requested via
    /// `--session`. Used by commands where tmux is opt-in (the wrapped
    /// process manages its own persistence, e.g. `claude agents` whose
    /// background sessions live in the daemon, not the terminal).
    fn tmux_session_opt_in(&self) -> Option<&str> {
        if self.no_tmux {
            return None;
        }
        self.session.as_deref()
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
        #[arg(long)]
        template_size: Option<u32>,
        /// Named image to build (default: "default")
        #[arg(
            long,
            default_value = config::DEFAULT_IMAGE,
            add = ArgValueCandidates::new(completions::image_candidates),
        )]
        image: String,
        /// Workspace directory to scan for `.devcontainer/devcontainer.json`.
        /// When present (and `--no-devcontainer` is not set), coop offers to
        /// apply the file's `features` and `hostRequirements` to this setup.
        #[arg(long)]
        workspace: Option<String>,
        /// Explicit path to a `devcontainer.json` to use (skips discovery).
        #[arg(long, value_name = "PATH", conflicts_with = "no_devcontainer")]
        devcontainer: Option<String>,
        /// Ignore any discovered `devcontainer.json` (escape hatch for CI).
        #[arg(long)]
        no_devcontainer: bool,
        /// Translate `devcontainer.json` and print the report, then exit
        /// before doing any setup work.
        #[arg(long)]
        dry_run: bool,
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
        #[arg(long, alias = "no-claude")]
        no_agents: bool,
        /// Mount host directory into guest (`HOST_PATH[:GUEST_PATH]`, repeatable)
        #[arg(long, conflicts_with_all = ["workspace", "git_repo"])]
        mount: Vec<String>,
        /// Forward a guest port to the host (`GUEST[:HOST]`, repeatable).
        /// `--forward-port 3000` forwards guest 3000 to host 3000;
        /// `--forward-port 3000:3001` forwards guest 3000 to host 3001.
        #[arg(long)]
        forward_port: Vec<String>,
        /// Named image to use (default: "default")
        #[arg(
            long,
            default_value = config::DEFAULT_IMAGE,
            add = ArgValueCandidates::new(completions::image_candidates),
        )]
        image: String,
        /// Skip the `.git` directory when syncing the workspace
        #[arg(long, conflicts_with = "git_repo")]
        exclude_git: bool,
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
        #[arg(long = "env", value_name = "KEY=VALUE")]
        guest_env: Vec<String>,
        /// Explicit path to a `devcontainer.json` to use (skips discovery).
        #[arg(long, value_name = "PATH", conflicts_with = "no_devcontainer")]
        devcontainer: Option<String>,
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
    /// Open the Claude Code agent view inside the VM (`claude agents`)
    #[command(alias = "ca")]
    ClaudeAgents {
        #[command(flatten)]
        session: SessionArgs,
        /// Extra arguments passed to `claude agents`
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
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
        name: Option<String>,
    },
    /// Stop and clean up instance resources (keeps template)
    Destroy {
        /// Instance name (required if multiple instances exist)
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
        name: Option<String>,
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
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
        name: Option<String>,
    },
    /// Stream VM serial console logs
    Logs {
        /// Instance name (required if multiple instances exist)
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
        name: Option<String>,
        /// Follow log output
        #[arg(short, long)]
        follow: bool,
    },
    /// Push local workspace into the running VM
    Push {
        /// Instance name (required if multiple instances exist)
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
        name: Option<String>,
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
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
        name: Option<String>,
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
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
        name: Option<String>,
        /// Command and arguments to run (after `--`)
        #[arg(required = true, last = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Open VS Code connected to the guest VM
    Vscode {
        /// Instance name (required if multiple instances exist)
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
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
        #[arg(long, add = ArgValueCandidates::new(completions::image_candidates))]
        delete: Option<String>,
    },
    /// Resize a stopped instance's disk
    Resize {
        /// Instance name (required if multiple instances exist)
        #[arg(add = ArgValueCandidates::new(completions::instance_candidates))]
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
        #[arg(long)]
        repo: Option<String>,
    },
    /// Re-run the wizard against an existing entry
    RotatePat {
        /// Repo slug to rotate
        #[arg(long, required = true)]
        repo: String,
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
        #[arg(long, required = true)]
        repo: String,
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

#[expect(clippy::too_many_lines, reason = "CLI dispatch — flat match arms")]
fn main() -> Result<()> {
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

    let mut cfg = load_and_validate_config(&cli.config)?;
    update::maybe_print_notify(&cfg.updates);
    update::maybe_run_background_check(&cfg.updates);
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
            workspace,
            devcontainer,
            no_devcontainer,
            dry_run,
        } => {
            let ws_path = workspace.as_deref().map(Path::new);
            let inputs = devcontainer::TranslatorInputs {
                cli_vcpus: vcpus,
                cli_mem_mib: mem,
                cli_profiles: profile.clone(),
                ..devcontainer::TranslatorInputs::default()
            };
            let translation = resolve_devcontainer(
                &DevcontainerOpts {
                    explicit_path: devcontainer.as_deref(),
                    no_devcontainer,
                    dry_run,
                    workspace: ws_path,
                    mounts: &[],
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
            let _guard = signal::install_handlers();
            be.setup(
                &cfg,
                &setup::SetupOptions {
                    skip_confirm: yes,
                    rebuild,
                    profiles: resolved_profiles,
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
            no_agents,
            mount,
            image,
            exclude_git,
            no_prompt,
            post_start,
            guest_env,
            forward_port,
            devcontainer,
            no_devcontainer,
            dry_run,
        } => {
            if raw_args_use_deprecated_no_claude(std::env::args()) {
                tracing::warn!(
                    "--no-claude is deprecated and will be removed in a future release; use --no-agents"
                );
            }
            let mounts = mount
                .iter()
                .map(|s| config::Mount::parse(s))
                .collect::<Result<Vec<_>>>()?;
            let mut forward_ports = forward_port
                .iter()
                .map(|s| config::PortForward::parse(s))
                .collect::<Result<Vec<_>>>()?;

            let cli_env_keys = guest_env
                .iter()
                .filter_map(|s| s.split_once('=').map(|(k, _)| k.to_string()))
                .collect();
            let inputs = devcontainer::TranslatorInputs {
                cli_vcpus: vcpus,
                cli_mem_mib: mem,
                cli_disk_gib: disk,
                cli_post_start: post_start.clone(),
                cli_guest_env_keys: cli_env_keys,
                cli_forward_ports: forward_ports.clone(),
                cli_mounts: mounts.clone(),
                cli_profiles: Vec::new(),
                cli_workspace_or_git_repo: workspace.is_some() || git_repo.is_some(),
            };
            let ws_path = workspace.as_deref().map(Path::new);
            let translation = resolve_devcontainer(
                &DevcontainerOpts {
                    explicit_path: devcontainer.as_deref(),
                    no_devcontainer,
                    dry_run,
                    workspace: ws_path,
                    mounts: &mounts,
                },
                &inputs,
                devcontainer::Stage::Start,
            )?;
            if dry_run {
                return Ok(());
            }
            // CLI flags are applied first; the translation only carries
            // values that survived the "CLI > devcontainer.json" precedence
            // check inside `translate`, so the two cannot fight here.
            apply_vm_overrides(&mut cfg, vcpus, mem, None)?;
            if let Some(t) = &translation {
                devcontainer::apply_to_config(&mut cfg, t)?;
                forward_ports =
                    devcontainer::merge_into_forward_ports(&t.forward_ports, &forward_ports);
            }
            let cli_guest_env = guest_env_state::parse_cli_env_args(&guest_env)?;
            for (key, value) in &cli_guest_env {
                cfg.guest_env.insert(key.clone(), value.clone());
            }
            // Union of CLI `--env` and devcontainer `containerEnv`. Both
            // are start-time inputs that won't be re-derived by later
            // `coop shell`/`exec`, so they belong in the on-disk snapshot
            // (see `guest_env_state` module docs for the rationale).
            let dc_guest_env = translation
                .as_ref()
                .map(|t| t.guest_env.clone())
                .unwrap_or_default();
            let persisted_guest_env =
                guest_env_state::merge_persisted_entries(&dc_guest_env, &cli_guest_env);
            let cli_disk = disk
                .map(|d| config::GiB::new(d).context("--disk must be > 0"))
                .transpose()?;
            let default_translation = devcontainer::Translation::default();
            let effective_disk = devcontainer::effective_disk(
                cli_disk,
                translation.as_ref().unwrap_or(&default_translation),
            );
            let post_start_override = post_start
                .clone()
                .or_else(|| translation.as_ref().and_then(|t| t.post_start.clone()));
            // `--mount` and devcontainer `mounts` aren't combined: --mount is
            // a complete replacement of the mount set (its `conflicts_with`
            // rules with --workspace/--git-repo encode this). The CLI-wins
            // outcome is reported by `translate`.
            let final_mounts = if mounts.is_empty() {
                translation
                    .as_ref()
                    .map(|t| t.mounts.clone())
                    .unwrap_or_default()
            } else {
                mounts
            };
            cmd_start(
                &be,
                &mut cfg,
                &StartOpts {
                    name: name.as_deref(),
                    image: &image,
                    workspace_dir: workspace.as_deref(),
                    git_repo: git_repo.as_deref(),
                    no_agents,
                    no_prompt,
                    disk: effective_disk,
                    mounts: final_mounts,
                    exclude_git,
                    forward_ports,
                    config_path: &cli.config,
                    post_start_override: post_start_override.as_deref(),
                    persisted_guest_env,
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
            let sess = open_ssh_session(&be, &cfg, session.name.as_deref())?;
            // Guest user settings set `defaultMode: bypassPermissions`. Opting in
            // to prompts means overriding that default explicitly.
            if ask {
                args.insert(0, "default".to_string());
                args.insert(0, "--permission-mode".to_string());
            }
            let tmux = session.tmux_session("claude");
            ssh::run_interactive(&sess, crate::guest::CLAUDE_BIN, &args, tmux)
        }
        Commands::ClaudeAgents { session, mut args } => {
            let sess = open_ssh_session(&be, &cfg, session.name.as_deref())?;
            args.insert(0, "agents".to_string());
            let tmux = session.tmux_session_opt_in();
            ssh::run_interactive(&sess, crate::guest::CLAUDE_BIN, &args, tmux)
        }
        Commands::Codex { session, args } => {
            let sess = open_ssh_session(&be, &cfg, session.name.as_deref())?;
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
        Commands::List => cmd_list(&be, &cfg),
        Commands::Status { name } => cmd_status(&be, &cfg, name.as_deref()),
        Commands::Logs { name, follow } => {
            let running = resolve_running(&be, &cfg, name.as_deref())?;
            be.stream_logs(&cfg, &running.inst, follow)
        }
        Commands::Push {
            name,
            dir,
            force,
            exclude_git,
        } => {
            let running = resolve_running(&be, &cfg, name.as_deref())?;
            workspace::push(
                &running.target,
                &running.inst,
                dir.as_deref(),
                force,
                exclude_git,
            )
        }
        Commands::Pull {
            name,
            dir,
            force,
            exclude_git,
        } => {
            let running = resolve_running(&be, &cfg, name.as_deref())?;
            workspace::pull(
                &running.target,
                &running.inst,
                dir.as_deref(),
                force,
                exclude_git,
            )
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
        Commands::Github { action } => cmd_github(&cfg, &cli.config, &action),
        Commands::Validate { probe } => cmd_validate(&cfg, &be, probe),
        Commands::Init
        | Commands::Update { .. }
        | Commands::Uninstall { .. }
        | Commands::Completions { .. } => {
            unreachable!("handled before config loading")
        }
    }
}

fn cmd_github(cfg: &config::CoopConfig, config_path: &Path, action: &GithubAction) -> Result<()> {
    match action {
        GithubAction::SetupPat { repo } => {
            let opts = github_pat::SetupOpts {
                repo: repo.as_deref(),
                config_path,
            };
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
            github_pat::run_status(cfg, *probe);
            Ok(())
        }
        GithubAction::ForgetPat { repo } => github_pat::run_forget_pat(cfg, repo, config_path),
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
                        match probe_pat_token(&token) {
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

/// Issue `GET https://api.github.com/user` with `token`. Returns the
/// authenticated user's login on success.
fn probe_pat_token(token: &str) -> Result<String> {
    let body = cmd::Cmd::new("curl")
        .arg("-fsSL")
        .arg("-H")
        .arg("Accept: application/vnd.github+json")
        .arg("-H")
        .arg("@-")
        .stdin_input(format!("Authorization: token {token}\n"))
        .arg("https://api.github.com/user")
        .capture()
        .context("curl /user failed")?;
    // Naive extraction: grab the first `"login": "..."`. Avoids a JSON dep.
    let login = body
        .split("\"login\"")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .unwrap_or("?");
    Ok(login.to_string())
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

/// CLI surface controlling devcontainer.json discovery and apply.
///
/// `explicit_path` opts the caller in to a specific file (skips the
/// prompt). `no_devcontainer` opts out entirely (skips discovery).
/// `dry_run` prints the report and exits before any side effects.
struct DevcontainerOpts<'a> {
    explicit_path: Option<&'a str>,
    no_devcontainer: bool,
    dry_run: bool,
    workspace: Option<&'a Path>,
    mounts: &'a [config::Mount],
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

    if opts.no_devcontainer {
        return Ok(None);
    }

    let (path, losers) = if let Some(p) = opts.explicit_path {
        (PathBuf::from(p), Vec::new())
    } else {
        let found = devcontainer::discover(opts.workspace, opts.mounts);
        if let Some((winner, losers)) = devcontainer::pick_winner(found) {
            (winner.path, losers)
        } else {
            return Ok(None);
        }
    };

    // When discovery (not an explicit flag) found the file, defer to the
    // user. CI/scripted callers must pass --devcontainer or --no-devcontainer.
    if opts.explicit_path.is_none() && !opts.dry_run {
        if !std::io::stdin().is_terminal() {
            bail!(
                "Found {} but stdin is not a TTY.\n\
                 Pass --devcontainer {} to apply it, or --no-devcontainer to ignore.\n\
                 coop reads a subset of devcontainer.json — see docs/devcontainer.md for the supported keys.",
                path.display(),
                path.display()
            );
        }
        let answer =
            prompt::confirm_default_yes(&format!("Use devcontainer.json at {}?", path.display()))?;
        if !answer {
            tracing::info!(
                "Skipping {}. Re-run with --devcontainer {} to apply it later.",
                path.display(),
                path.display()
            );
            return Ok(None);
        }
    }

    let parsed = devcontainer::ParsedDevcontainer::load(&path)?;
    let mut translation = devcontainer::translate(&parsed, inputs, stage);
    translation.report.ignored_paths = losers;

    eprintln!("{}", translation.report.render());

    Ok(Some(translation))
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
    /// Start-time guest-env entries to persist as the per-instance
    /// snapshot, so later `coop shell`/`exec` runs see them. This is
    /// the union of CLI `--env KEY=VALUE` and the devcontainer
    /// translator's `containerEnv` map (CLI wins per-key). `[guest_env]`
    /// from `config.toml` is re-read every invocation and deliberately
    /// not saved here.
    persisted_guest_env: std::collections::BTreeMap<String, String>,
}

fn cmd_start(
    be: &backend::PlatformBackend,
    cfg: &mut config::CoopConfig,
    opts: &StartOpts<'_>,
) -> Result<()> {
    let ws_path = opts.workspace_dir.map(Path::new);
    if let Some(inst) = find_stopped_instance(be, cfg, opts.name, ws_path)? {
        let has_ignored_flags = !opts.mounts.is_empty()
            || opts.workspace_dir.is_some()
            || opts.git_repo.is_some()
            || opts.disk.is_some()
            || opts.exclude_git;

        if has_ignored_flags {
            bail!(
                "Instance '{}' already exists (stopped). The flags \
                 --mount, --workspace, --git-repo, --disk, and --exclude-git \
                 are only applied at creation time and would be silently \
                 ignored on restart.\n\
                 To apply new options, destroy the instance first:\n  \
                 coop destroy {0}\n  coop start {0} [options]",
                inst.name,
            );
        }

        return restart_instance(be, cfg, &inst, opts);
    }

    let inst = cfg.allocate_instance(opts.name, opts.image, ws_path)?;
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
            backend::bootstrap_agents(&session, cfg, inst, true)?;
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

    let post_start = opts.post_start_override.or(cfg.post_start.as_deref());
    if opts.no_agents && post_start.is_none() {
        tracing::info!("Skipping guest agent bootstrap (--no-agents)");
    } else {
        let session = prepare_session_from_target(cfg, None, target.clone(), repo.as_ref())?;
        if opts.no_agents {
            tracing::info!("Skipping guest agent bootstrap (--no-agents)");
        } else {
            backend::bootstrap_agents(&session, cfg, inst, false)?;
        }
        if let Some(cmd) = post_start {
            backend::run_post_start(&session, cmd);
        }
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

        workspace::tar_pipe_transfer(&target, &abs_path, opts.exclude_git)?;

        let state = workspace::WorkspaceState {
            guest_path: "/workspace".to_string(),
            source: workspace::WorkspaceSource::Workspace {
                host_path: abs_path,
            },
        };
        state.save(inst)?;
    } else if let Some(repo_url) = opts.git_repo {
        backend::clone_git_repo(&target, cfg.github.as_ref(), repo_url)?;

        let state = workspace::WorkspaceState {
            guest_path: "/workspace".to_string(),
            source: workspace::WorkspaceSource::GitRepo {
                url: repo_url.to_string(),
            },
        };
        state.save(inst)?;
    } else if !opts.mounts.is_empty() {
        if be.mounts_are_live() {
            // Lima: virtiofs already serves the host directory live. No
            // sync step, but we still record state so `push`/`pull` and
            // PAT slug detection work for follow-up commands.
            workspace::record_mount_state(inst, &opts.mounts)?;
            warn_on_live_git_mounts(&opts.mounts);
        } else {
            workspace::sync_mounts(&target, inst, &opts.mounts, opts.exclude_git)?;
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
    let session = open_ssh_session(be, cfg, name)?;
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
    name: Option<&str>,
) -> Result<backend::SshSession> {
    let running = resolve_running(be, cfg, name)?;
    let repo = backend::detect_instance_repo(&running.inst);
    prepare_session_from_target(cfg, Some(&running.inst), running.target, repo.as_ref())
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
    // Tear down forwards before shutting down the VM so the control
    // master can exit cleanly while SSH is still reachable.
    if let Ok(target) = be.ssh_target(cfg, inst) {
        port_forward::teardown_ssh_forwards(inst, &target);
    } else {
        tracing::debug!("Skipping forward teardown — no SSH target available");
    }
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
#[expect(clippy::unwrap_used, reason = "test code — panics are assertions")]
#[expect(clippy::expect_used, reason = "test code — panics are assertions")]
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
    fn claude_agents_defaults_to_no_tmux() {
        let cli = parse(&["claude-agents"]);
        let super::Commands::ClaudeAgents { session, .. } = cli.command else {
            panic!("expected ClaudeAgents variant");
        };
        assert!(session.tmux_session_opt_in().is_none());
    }

    #[test]
    fn claude_agents_session_flag_opts_into_tmux() {
        let cli = parse(&["claude-agents", "--session", "dev"]);
        let super::Commands::ClaudeAgents { session, .. } = cli.command else {
            panic!("expected ClaudeAgents variant");
        };
        assert_eq!(session.tmux_session_opt_in(), Some("dev"));
    }

    #[test]
    fn claude_agents_no_tmux_stays_off() {
        let cli = parse(&["claude-agents", "--no-tmux"]);
        let super::Commands::ClaudeAgents { session, .. } = cli.command else {
            panic!("expected ClaudeAgents variant");
        };
        assert!(session.no_tmux);
        assert!(session.tmux_session_opt_in().is_none());
    }

    #[test]
    fn claude_agents_session_and_no_tmux_conflict() {
        let err = parse_err(&["claude-agents", "--session", "x", "--no-tmux"]);
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn claude_agents_name_and_trailing_args_parse() {
        let cli = parse(&["ca", "myvm", "--", "--cwd", "/workspace"]);
        let super::Commands::ClaudeAgents { session, args, .. } = cli.command else {
            panic!("expected ClaudeAgents variant");
        };
        assert_eq!(session.name.as_deref(), Some("myvm"));
        assert_eq!(args, vec!["--cwd", "/workspace"]);
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
    fn start_no_agents_flag_parses() {
        let cli = parse(&["start", "--no-agents"]);
        let super::Commands::Start { no_agents, .. } = cli.command else {
            panic!("expected Start variant");
        };
        assert!(no_agents);
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
        assert_eq!(
            guest_env,
            vec!["FOO=bar".to_string(), "BAZ=qux".to_string()]
        );
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
            index: super::config::InstanceIndex::new(0),
            dir: tmp.path().to_path_buf(),
            image: super::config::DEFAULT_IMAGE.to_string(),
        };
        let mut state = super::guest_env_state::GuestEnvState::default();
        state
            .entries
            .insert("FROM_CLI".to_string(), "saved-value".to_string());
        state.save(&inst).expect("save snapshot");

        let mut cfg = super::config::CoopConfig::default();
        // Sanity: an entry in cfg without a CLI override should still
        // appear (so the overlay is additive, not replacing).
        cfg.guest_env
            .insert("FROM_CFG".to_string(), "cfg-value".to_string());

        let target = super::backend::SshTarget {
            host: "127.0.0.1".to_string(),
            port: NonZeroU16::new(22).expect("non-zero"),
            user: "ubuntu".to_string(),
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
    fn prepare_session_skips_overlay_without_instance() {
        use std::num::NonZeroU16;

        let tmp = tempfile::tempdir().expect("tempdir");
        let cfg = super::config::CoopConfig::default();
        let target = super::backend::SshTarget {
            host: "127.0.0.1".to_string(),
            port: NonZeroU16::new(22).expect("non-zero"),
            user: "ubuntu".to_string(),
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
        assert_eq!(name.as_deref(), Some("myvm"));
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
        assert_eq!(name.as_deref(), Some("myvm"));
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
        assert_eq!(name.as_deref(), Some("myvm"));
        assert_eq!(dir.as_deref(), Some("./out"));
        assert!(force);
    }

    #[test]
    fn exec_positional_name_and_command_parse() {
        let cli = parse(&["exec", "myvm", "--", "ls", "-la"]);
        let super::Commands::Exec { name, command } = cli.command else {
            panic!("expected Exec variant");
        };
        assert_eq!(name.as_deref(), Some("myvm"));
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
    fn shell_no_tmux_without_session() {
        let cli = parse(&["shell", "--no-tmux"]);
        let super::Commands::Shell { session, .. } = cli.command else {
            panic!("expected Shell variant");
        };
        assert!(session.no_tmux);
        assert!(session.tmux_session("main").is_none());
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
            image: super::config::DEFAULT_IMAGE,
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
            guest_path: "/workspace".to_string(),
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
            guest_path: "/workspace".to_string(),
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
